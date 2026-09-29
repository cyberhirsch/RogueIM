//! Signals over Nostr for devices we have no direct connection to:
//! presence (so a contact behind a router still shows as online) and the
//! coordination of a NAT hole punch (both sides dial each other at the same
//! moment, so both routers let the other side in).

use std::time::Duration;

use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
use libp2p::{Multiaddr, PeerId};

use super::net::{is_public, is_quic};
use super::*;
use crate::identity::{self, now};
use crate::mailbox::parse_seed;

/// Presence over Nostr is refreshed this often ...
const SIGNAL_PRESENCE_EVERY: i64 = 60;
/// ... and counts this long without a refresh.
pub(crate) const REMOTE_PRESENCE_TTL: Duration = Duration::from_secs(150);
/// Ask the same device to hole-punch at most this often.
const PUNCH_EVERY: i64 = 90;
/// Dial attempts per punch, spaced so both sides overlap despite relay delay.
const PUNCH_ROUNDS: u8 = 3;

impl Engine {
    /// Authorized devices (contacts' and our own) without a direct connection.
    fn unreachable_devices(&self) -> Vec<(String, String, DeviceSeeds)> {
        let me = self.peer_id.to_string();
        let mut out = vec![];
        for c in &self.p.contacts {
            if c.removed || !c.authorized || c.ignored {
                continue;
            }
            for d in &c.devices {
                if let Some(s) = &d.seeds {
                    if !self.is_connected_peer(&d.entry.peer_id) {
                        out.push((c.id.clone(), d.entry.peer_id.clone(), s.clone()));
                    }
                }
            }
        }
        for (peer, s) in &self.p.own_seeds {
            if peer != &me && !self.is_connected_peer(peer) {
                out.push(("self".to_string(), peer.clone(), s.clone()));
            }
        }
        out
    }

    fn seeds_of(&self, device: &str) -> Option<DeviceSeeds> {
        if let Some(s) = self.p.own_seeds.get(device) {
            return Some(s.clone());
        }
        self.p.contacts.iter().flat_map(|c| c.devices.iter()).find(|d| d.entry.peer_id == device).and_then(|d| d.seeds.clone())
    }

    fn signal_to(&self, device: &str, seeds: &DeviceSeeds, kind: SignalKind) {
        let Some(mb) = self.mailbox.clone() else { return };
        if self.p.net.lan_only {
            return;
        }
        let Ok(seed) = parse_seed(&seeds.inbox) else { return };
        let s = Signal { from: self.peer_id.to_string(), to: device.to_string(), ts: now(), kind, sig: String::new() };
        let Ok(s) = identity::sign_signal(&self.device_key, s) else { return };
        let Ok(bytes) = serde_json::to_vec(&s) else { return };
        tokio::spawn(async move {
            let _ = mb.post_signal(&seed, &bytes).await;
        });
    }

    /// Our presence to every device that cannot hear it directly.
    pub fn signal_presence_all(&mut self) {
        self.last_signal_presence = now();
        for (owner, device, seeds) in self.unreachable_devices() {
            if let Some(p) = self.presence_for(&owner) {
                self.signal_to(&device, &seeds, SignalKind::Presence(p));
            }
        }
    }

    /// Where others can punch through to us: public QUIC addresses only.
    fn punch_addrs(&self) -> Vec<String> {
        self.observed.iter().chain(self.external.iter()).filter(|a| is_quic(a) && is_public(a)).map(|a| a.to_string()).collect()
    }

    pub fn signal_heartbeat(&mut self, now: i64) {
        if self.mailbox.is_none() || self.p.net.lan_only {
            return;
        }
        if now - self.last_signal_presence >= SIGNAL_PRESENCE_EVERY {
            self.signal_presence_all();
        }
        // Ask unreachable devices for a simultaneous dial.
        let mine = self.punch_addrs();
        if mine.is_empty() {
            return;
        }
        for (_, device, seeds) in self.unreachable_devices() {
            if now - self.punched.get(&device).copied().unwrap_or(0) < PUNCH_EVERY {
                continue;
            }
            self.punched.insert(device.clone(), now);
            self.signal_to(&device, &seeds, SignalKind::Punch { addrs: mine.clone(), reply: false });
        }
    }

    pub fn on_signal(&mut self, bytes: Vec<u8>) {
        let Ok(s) = serde_json::from_slice::<Signal>(&bytes) else { return };
        if s.to != self.peer_id.to_string() || (now() - s.ts).abs() > 300 || identity::verify_signal(&s).is_err() {
            return;
        }
        // Only from our own devices and authorized contacts.
        let Some(owner) = self.owner_of_peer(&s.from) else { return };
        if owner != "self" && !self.contact(&owner).map(|c| c.authorized && !c.removed && !c.ignored).unwrap_or(false) {
            return;
        }
        match s.kind {
            SignalKind::Presence(p) => {
                // A direct connection is the better source.
                if self.is_connected_peer(&s.from) {
                    return;
                }
                self.on_presence(&owner, &s.from, p);
                if let Some(r) = self.presence.get_mut(&s.from) {
                    r.ttl = REMOTE_PRESENCE_TTL;
                }
                self.emit_contacts();
                self.emit_devices();
            }
            SignalKind::Punch { addrs, reply } => {
                if self.is_connected_peer(&s.from) {
                    return;
                }
                self.register_addrs(&s.from, &addrs);
                if !reply {
                    let mine = self.punch_addrs();
                    if let Some(seeds) = self.seeds_of(&s.from) {
                        self.punched.insert(s.from.clone(), now());
                        self.signal_to(&s.from, &seeds, SignalKind::Punch { addrs: mine, reply: true });
                    }
                }
                self.punch(&s.from, &addrs, 0);
            }
        }
    }

    /// One round of the hole punch. The side with the smaller peer id dials;
    /// the other "dials as listener": its QUIC socket sends packets to open the
    /// router and waits for the incoming connection.
    pub fn punch(&mut self, peer: &str, addrs: &[String], round: u8) {
        let Ok(pid) = peer.parse::<PeerId>() else { return };
        if self.swarm.is_connected(&pid) || round >= PUNCH_ROUNDS {
            return;
        }
        let targets: Vec<Multiaddr> = addrs.iter().filter_map(|a| a.parse::<Multiaddr>().ok()).filter(|a| is_quic(a) && is_public(a)).collect();
        if targets.is_empty() {
            return;
        }
        let opts = DialOpts::peer_id(pid).condition(PeerCondition::Always).addresses(targets);
        let opts = if self.peer_id > pid { opts.override_role().build() } else { opts.build() };
        let _ = self.swarm.dial(opts);
        let tx = self.internal_tx.clone();
        let (peer, addrs) = (peer.to_string(), addrs.to_vec());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let _ = tx.send(Internal::Punch { peer, addrs, round: round + 1 });
        });
    }
}
