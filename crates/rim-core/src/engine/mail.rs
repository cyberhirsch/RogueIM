//! Store and forward: Nostr mailbox (primary), buddy relays (helpers among our
//! contacts), and the DHT mailbox; plus Nostr address rendezvous.

use libp2p::kad::{self, Record, RecordKey};
use libp2p::PeerId;
use sha2::{Digest, Sha256};

use super::msg::Via;
use super::*;
use crate::identity::now;
use crate::mailbox::{parse_seed, MailItem};

/// Full mailbox check (the live subscription delivers new mail at once).
const MAIL_EVERY: i64 = 120;
const RDV_EVERY: i64 = 600;
const DHT_SLOTS: u32 = 8;

fn dht_key(seed: &[u8; 32], day: i64, slot: u32) -> RecordKey {
    let mut h = Sha256::new();
    h.update(b"rim-dht-mbox");
    h.update(seed);
    h.update(day.to_le_bytes());
    h.update(slot.to_le_bytes());
    RecordKey::new(&h.finalize().to_vec())
}

impl Engine {
    pub fn all_inbox_seeds(&self) -> Vec<[u8; 32]> {
        self.p.own_seeds.get(&self.peer_id.to_string()).and_then(|s| parse_seed(&s.inbox).ok()).into_iter().collect()
    }

    pub fn mail_heartbeat(&mut self, now: i64) {
        if self.p.net.lan_only {
            return;
        }
        // Held items expire.
        self.p.held.retain(|h| h.expires > now);
        let Some(mb) = self.mailbox.clone() else {
            self.dht_poll(now);
            return;
        };
        let today = now.div_euclid(86_400);
        if self.live_day != today {
            self.live_day = today;
            let seeds = self.all_inbox_seeds();
            let tx = self.internal_tx.clone();
            let m = mb.clone();
            tokio::spawn(async move {
                m.listen(&seeds, move |l| {
                    let _ = tx.send(Internal::Live(l));
                })
                .await;
            });
        }
        if !self.mail_fetching && now - self.last_mail_fetch >= MAIL_EVERY {
            self.mail_fetching = true;
            self.last_mail_fetch = now;
            let seeds = self.all_inbox_seeds();
            let tx = self.internal_tx.clone();
            let probe = mb.clone();
            tokio::spawn(async move {
                let items = probe.fetch(&seeds).await.unwrap_or_default();
                let _ = tx.send(Internal::Mail(items));
                let _ = tx.send(Internal::RelayStatus(probe.relay_status().await));
            });
        }
        if now - self.last_rdv_publish >= RDV_EVERY && self.p.linking.is_none() {
            self.last_rdv_publish = now;
            if let Ok(seed) = parse_seed(&self.my_seeds().rdv) {
                let addrs = self.my_addrs();
                let m = mb.clone();
                tokio::spawn(async move {
                    let _ = m.publish_rdv(&seed, &addrs).await;
                });
            }
        }
        // Look up addresses of devices we cannot reach (every 2 minutes each).
        if now % 120 < 10 {
            let mut wanted: Vec<(String, String)> = vec![];
            for c in &self.p.contacts {
                if c.removed || !c.authorized {
                    continue;
                }
                for d in &c.devices {
                    if let Some(s) = &d.seeds {
                        if !self.is_connected_peer(&d.entry.peer_id) {
                            wanted.push((d.entry.peer_id.clone(), s.rdv.clone()));
                        }
                    }
                }
            }
            for (peer, s) in &self.p.own_seeds {
                if peer != &self.peer_id.to_string() && !self.is_connected_peer(peer) {
                    wanted.push((peer.clone(), s.rdv.clone()));
                }
            }
            for g in self.p.groups.iter().filter(|g| !g.left) {
                for m in &g.state.members {
                    for (peer, s) in &m.seeds {
                        if !self.is_connected_peer(peer) && !wanted.iter().any(|(p, _)| p == peer) {
                            wanted.push((peer.clone(), s.rdv.clone()));
                        }
                    }
                }
            }
            for (peer, rdv) in wanted {
                let Ok(seed) = parse_seed(&rdv) else { continue };
                let m = mb.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    if let Ok(Some(addrs)) = m.fetch_rdv(&seed).await {
                        let _ = tx.send(Internal::Rdv { peer, addrs });
                    }
                });
            }
        }
        self.dht_poll(now);
    }

    pub fn on_rdv(&mut self, peer: &str, addrs: Vec<String>) {
        if let Some(owner) = self.owner_of_peer(peer) {
            if owner == "self" {
                self.p.own_addrs.insert(peer.to_string(), addrs.clone());
            } else if let Some(d) = self.contact_mut(&owner).and_then(|c| c.devices.iter_mut().find(|d| d.entry.peer_id == peer)) {
                d.addrs = addrs.clone();
            }
        }
        self.register_addrs(peer, &addrs);
        if let Ok(pid) = peer.parse::<PeerId>() {
            if !self.files_rt.direct.contains(&pid) {
                self.dial_direct(pid);
            }
        }
    }

    pub fn on_mail(&mut self, items: Vec<MailItem>) {
        self.mail_fetching = false;
        self.take_mail(items);
    }

    pub fn take_mail(&mut self, items: Vec<MailItem>) {
        let mut changed = false;
        for item in items {
            let key = item.event_id.to_hex();
            if self.p.seen_mail.contains(&key) {
                continue;
            }
            self.p.seen_mail.push(key);
            changed = true;
            if let Ok(req) = serde_json::from_slice::<WireReq>(&item.payload) {
                if let Err(e) = self.on_wire(None, req, Via::Mailbox) {
                    tracing::debug!("mailbox item rejected: {e:#}");
                }
            }
            if let Some(mb) = self.mailbox.clone() {
                tokio::spawn(async move {
                    let _ = mb.delete(&item).await;
                });
            }
        }
        if self.p.seen_mail.len() > 2000 {
            let n = self.p.seen_mail.len() - 2000;
            self.p.seen_mail.drain(..n);
        }
        if changed {
            self.save();
        }
    }

    /// A reliable message could not be delivered directly: leave it in the
    /// recipient device's mailbox, with helpers, and in the DHT.
    pub fn store_in_network(&mut self, owner: &str, device: &str, msg_id: u64) {
        let Some(item) = self.p.outbox.iter().find(|o| o.rec.device == device && o.rec.msg_id == msg_id).cloned() else { return };
        if item.rec.stored || self.p.net.lan_only {
            return;
        }
        let seeds = if owner == "self" {
            self.p.own_seeds.get(device).cloned()
        } else {
            self.targets(owner).into_iter().find(|t| t.entry.peer_id == device).and_then(|t| t.seeds)
        };
        let Some(seeds) = seeds else { return };
        let Ok(seed) = parse_seed(&seeds.inbox) else { return };
        let Ok(payload) = serde_json::to_vec(&item.rec.req) else { return };
        // Mark as in progress so we don't post twice.
        if let Some(o) = self.p.outbox.iter_mut().find(|o| o.rec.device == device && o.rec.msg_id == msg_id) {
            o.rec.stored = true;
        }
        if let Some(mb) = self.mailbox.clone() {
            let tx = self.internal_tx.clone();
            let (owner, device) = (owner.to_string(), device.to_string());
            let payload2 = payload.clone();
            tokio::spawn(async move {
                let ok = mb.post(&seed, &payload2).await.is_ok();
                let _ = tx.send(Internal::MailPosted { contact: owner, device, msg_id, ok });
            });
        }
        // Buddy relays: helpers among our online contacts (not the recipient).
        let helpers: Vec<String> = self
            .presence
            .iter()
            .filter(|(_, r)| r.pres.helper && r.owner != owner && r.owner != "self")
            .map(|(_, r)| r.owner.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .take(3)
            .collect();
        for h in helpers {
            let _ = self.send_body(&h, Body::Hold { device: device.to_string(), wire: item.rec.req.clone(), expires: now() + crate::mailbox::TTL_SECS as i64 }, None);
        }
        // DHT: a few slots per day.
        let slot = rand::random::<u32>() % DHT_SLOTS;
        let day = now().div_euclid(86_400);
        let Ok(sealed) = crate::mailbox::seal_for(&seed, &payload) else { return };
        let rec = Record::new(dht_key(&seed, day, slot), sealed.into_bytes());
        let _ = self.swarm.behaviour_mut().kad.put_record(rec, kad::Quorum::One);
    }

    /// Connected to a helper: ask for anything it holds for us.
    pub fn ask_held(&mut self, peer: &str) {
        let helper = self.presence.get(peer).map(|r| r.pres.helper).unwrap_or(false);
        if !helper {
            return;
        }
        let Some(owner) = self.owner_of_peer(peer) else { return };
        let targets: Vec<msg::Target> = self.targets(&owner).into_iter().filter(|t| t.entry.peer_id == peer).collect();
        for t in targets {
            let _ = self.send_to_target(&t, &Body::Fetch, None, None);
        }
    }

    /// Forward held wires for a device (raw: they are encrypted to it).
    pub fn deliver_held(&mut self, device: &str) {
        let Ok(pid) = device.parse::<PeerId>() else { return };
        let (mine, rest): (Vec<HeldRec>, Vec<HeldRec>) = std::mem::take(&mut self.p.held).into_iter().partition(|h| h.device == device);
        self.p.held = rest;
        for h in mine {
            self.swarm.behaviour_mut().rr.send_request(&pid, h.wire);
        }
        self.save();
    }

    fn dht_poll(&mut self, now: i64) {
        if now % 300 >= 10 || self.swarm.connected_peers().count() == 0 {
            return;
        }
        let day = now.div_euclid(86_400);
        for seed in self.all_inbox_seeds() {
            for d in [day, day - 1] {
                for slot in 0..DHT_SLOTS {
                    self.swarm.behaviour_mut().kad.get_record(dht_key(&seed, d, slot));
                }
            }
        }
    }

    pub fn on_kad(&mut self, ev: kad::Event) {
        if let kad::Event::OutboundQueryProgressed { result: kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(r))), .. } = ev {
            let Ok(s) = String::from_utf8(r.record.value) else { return };
            let Some(bytes) = self.all_inbox_seeds().iter().find_map(|seed| crate::mailbox::open_for(seed, &s).ok()) else { return };
            let key = hex::encode(Sha256::digest(&bytes));
            if self.p.seen_mail.contains(&key) {
                return;
            }
            self.p.seen_mail.push(key);
            if let Ok(req) = serde_json::from_slice::<WireReq>(&bytes) {
                let _ = self.on_wire(None, req, Via::Relay);
            }
        }
    }
}
