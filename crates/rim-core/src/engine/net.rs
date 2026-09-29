//! libp2p: transports, behaviours, swarm events, dialing, NAT traversal.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use libp2p::core::ConnectedPoint;
use libp2p::identity::Keypair;
use libp2p::kad::store::MemoryStore;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{autonat, dcutr, identify, kad, mdns, noise, ping, relay, tcp, upnp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm};

use super::{Engine, Event};
use crate::proto::*;

pub const MSG_PROTOCOL: &str = "/rim/msg/2";
pub const FILE_PROTOCOL: &str = "/rim/file/1";
pub const KAD_PROTOCOL: &str = "/rim/kad/1.0.0";

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub rr: request_response::json::Behaviour<WireReq, WireResp>,
    pub file: request_response::json::Behaviour<ChunkReq, ChunkResp>,
    pub mdns: Toggle<mdns::tokio::Behaviour>,
    pub identify: identify::Behaviour,
    pub ping: ping::Behaviour,
    pub kad: kad::Behaviour<MemoryStore>,
    pub autonat: autonat::Behaviour,
    pub relay_client: relay::client::Behaviour,
    pub relay_server: Toggle<relay::Behaviour>,
    pub dcutr: dcutr::Behaviour,
    pub upnp: Toggle<upnp::tokio::Behaviour>,
}

pub struct NetOpts {
    pub mdns: bool,
    pub upnp: bool,
    pub relay_server: bool,
}

pub fn build_swarm(device: Keypair, opts: NetOpts) -> Result<Swarm<Behaviour>> {
    let swarm = libp2p::SwarmBuilder::with_existing_identity(device)
        .with_tokio()
        .with_tcp(tcp::Config::default().nodelay(true), noise::Config::new, yamux::Config::default)?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            let peer = key.public().to_peer_id();
            let rr = request_response::json::Behaviour::new(
                [(StreamProtocol::new(MSG_PROTOCOL), ProtocolSupport::Full)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(15)),
            );
            let file = request_response::json::Behaviour::new(
                [(StreamProtocol::new(FILE_PROTOCOL), ProtocolSupport::Full)],
                request_response::Config::default()
                    .with_request_timeout(Duration::from_secs(30))
                    .with_max_concurrent_streams(64),
            );
            let mdns = if opts.mdns { Some(mdns::tokio::Behaviour::new(mdns::Config::default(), peer)?) } else { None };
            let mut kcfg = kad::Config::new(StreamProtocol::new(KAD_PROTOCOL));
            kcfg.set_record_ttl(Some(Duration::from_secs(3 * 24 * 3600)));
            let mut kad = kad::Behaviour::with_config(peer, MemoryStore::new(peer), kcfg);
            kad.set_mode(Some(if opts.relay_server { kad::Mode::Server } else { kad::Mode::Client }));
            Ok(Behaviour {
                rr,
                file,
                mdns: mdns.into(),
                identify: identify::Behaviour::new(identify::Config::new("/rim/id/2".into(), key.public()).with_agent_version(format!("rogueim/{}", env!("CARGO_PKG_VERSION")))),
                ping: ping::Behaviour::new(ping::Config::new()),
                kad,
                autonat: autonat::Behaviour::new(peer, autonat::Config::default()),
                relay_client,
                relay_server: if opts.relay_server { Some(relay::Behaviour::new(peer, relay::Config::default())) } else { None }.into(),
                dcutr: dcutr::Behaviour::new(peer),
                upnp: if opts.upnp { Some(upnp::tokio::Behaviour::default()) } else { None }.into(),
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();
    Ok(swarm)
}

pub fn is_relayed(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

fn peer_of(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| if let Protocol::P2p(id) = p { Some(id) } else { None })
}

impl Engine {
    pub fn register_addrs(&mut self, peer: &str, addrs: &[String]) {
        let Ok(peer) = peer.parse::<PeerId>() else { return };
        if peer == self.peer_id {
            return;
        }
        for a in addrs {
            if let Ok(ma) = a.parse::<Multiaddr>() {
                // Never try LAN addresses of ourselves or unspecified ones.
                if ma.iter().any(|p| matches!(p, Protocol::Ip4(ip) if ip.is_unspecified())) {
                    continue;
                }
                self.swarm.add_peer_address(peer, ma);
            }
        }
    }

    pub fn is_connected_peer(&self, peer: &str) -> bool {
        peer.parse::<PeerId>().map(|p| self.swarm.is_connected(&p)).unwrap_or(false)
    }

    /// Dial the configured bootstrap / relay nodes.
    pub fn dial_bootstrap(&mut self) {
        if self.p.net.lan_only {
            return;
        }
        for a in self.p.net.bootstrap.clone() {
            if let Ok(ma) = a.parse::<Multiaddr>() {
                if let Some(pid) = peer_of(&ma) {
                    self.swarm.add_peer_address(pid, ma.clone());
                    self.swarm.behaviour_mut().kad.add_address(&pid, ma.clone());
                }
                let _ = self.swarm.dial(ma);
            }
        }
    }

    pub fn net_heartbeat(&mut self) {
        // Keep links to everyone we know alive: presence only flows over live
        // connections, so offline devices never make our ratchets run ahead.
        let mut peers: HashSet<String> = HashSet::new();
        for c in &self.p.contacts {
            if c.removed || c.ignored {
                continue;
            }
            for d in &c.devices {
                peers.insert(d.entry.peer_id.clone());
            }
        }
        if let Some(l) = &self.p.devices {
            for d in &l.list.devices {
                peers.insert(d.peer_id.clone());
            }
        }
        let mut group_addrs = vec![];
        for g in &self.p.groups {
            if g.left {
                continue;
            }
            for m in &g.state.members {
                for d in &m.devices.list.devices {
                    peers.insert(d.peer_id.clone());
                }
                group_addrs.extend(m.addrs.clone());
            }
        }
        for (peer, addrs) in group_addrs {
            self.register_addrs(&peer, &addrs);
        }
        peers.remove(&self.peer_id.to_string());
        for p in peers {
            if self.is_connected_peer(&p) {
                continue;
            }
            if let Ok(pid) = p.parse::<PeerId>() {
                let _ = self.swarm.dial(pid);
            }
        }
        // Presence to everyone connected, outbox retries.
        self.broadcast_presence();
        self.flush_all_outboxes();
    }

    pub fn emit_net(&self) {
        let v = NetView {
            peer_id: self.peer_id.to_string(),
            listen: self.listen.iter().map(|a| a.to_string()).collect(),
            external: self.external.iter().map(|a| a.to_string()).collect(),
            nat: self.nat.clone(),
            connected_peers: self.swarm.connected_peers().count(),
            relays: self.relay_status.clone(),
            lan_only: self.p.net.lan_only,
            helper: self.p.net.helper || self.node,
            held: self.p.held.len(),
            mailbox_last_fetch: self.last_mail_fetch,
        };
        self.emit(Event::Net(v));
    }

    pub fn on_swarm(&mut self, ev: SwarmEvent<BehaviourEvent>) {
        match ev {
            SwarmEvent::NewListenAddr { address, .. } => {
                if !is_relayed(&address) && !self.listen.contains(&address) {
                    self.listen.push(address);
                } else if is_relayed(&address) && !self.external.contains(&address) {
                    self.external.push(address);
                    self.last_rdv_publish = 0;
                }
                self.link_code_refresh();
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                if !self.external.contains(&address) {
                    self.external.push(address);
                    self.last_rdv_publish = 0;
                }
            }
            SwarmEvent::ExternalAddrExpired { address } => {
                self.external.retain(|a| a != &address);
            }
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                let addr = match &endpoint {
                    ConnectedPoint::Dialer { address, .. } => address.clone(),
                    ConnectedPoint::Listener { send_back_addr, .. } => send_back_addr.clone(),
                };
                self.files_rt.connection_up(peer_id, !is_relayed(&addr));
                let p = peer_id.to_string();
                if self.owner_of_peer(&p).is_some() {
                    self.send_presence_to_peer(&p);
                    self.flush_outbox_for_device(&p);
                    self.ask_held(&p);
                    self.group_catch_up(&p);
                }
            }
            SwarmEvent::ConnectionClosed { peer_id, num_established: 0, .. } => {
                self.files_rt.connection_down(peer_id);
                if self.presence.remove(&peer_id.to_string()).is_some() {
                    self.emit_contacts();
                    self.emit_devices();
                }
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                // Another connection remains; the direct flag may have changed.
                self.files_rt.connection_down(peer_id);
                self.files_rt.connection_up(peer_id, true);
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                let mut seen = HashSet::new();
                for (peer, addr) in list {
                    self.swarm.add_peer_address(peer, addr.clone());
                    self.swarm.behaviour_mut().kad.add_address(&peer, addr);
                    if self.owner_of_peer(&peer.to_string()).is_some() || self.p.linking.is_some() {
                        seen.insert(peer);
                    }
                }
                for peer in seen {
                    if !self.swarm.is_connected(&peer) {
                        let _ = self.swarm.dial(peer);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(_)) => {}
            SwarmEvent::Behaviour(BehaviourEvent::Rr(ev)) => self.on_rr(ev),
            SwarmEvent::Behaviour(BehaviourEvent::File(ev)) => self.on_file_rr(ev),
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                let hop = info.protocols.iter().any(|p| p.as_ref() == "/libp2p/circuit/relay/0.2.0/hop");
                for a in &info.listen_addrs {
                    self.swarm.behaviour_mut().kad.add_address(&peer_id, a.clone());
                }
                // Remember where a contact's device listens, so introductions and
                // contact sync can pass it on before its first presence arrives.
                let pid = peer_id.to_string();
                let heard: Vec<String> = info.listen_addrs.iter().filter(|a| !is_relayed(a)).map(|a| a.to_string()).collect();
                for c in self.p.contacts.iter_mut() {
                    if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == pid) {
                        for a in &heard {
                            if !d.addrs.contains(a) {
                                d.addrs.push(a.clone());
                            }
                        }
                        d.addrs.truncate(16);
                    }
                }
                // Use bootstrap nodes that offer relaying as our circuit relays.
                if hop && self.p.net.bootstrap.iter().any(|b| b.contains(&peer_id.to_string())) && !self.p.net.lan_only {
                    if let Some(a) = info.listen_addrs.iter().find(|a| !is_relayed(a) && !a.iter().any(|p| matches!(p, Protocol::Ip4(ip) if ip.is_loopback() || ip.is_private()))) {
                        let circuit = a.clone().with(Protocol::P2p(peer_id)).with(Protocol::P2pCircuit);
                        let _ = self.swarm.listen_on(circuit);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Autonat(autonat::Event::StatusChanged { new, .. })) => {
                self.nat = match new {
                    autonat::NatStatus::Public(a) => {
                        if !self.external.contains(&a) {
                            self.external.push(a);
                        }
                        "public".into()
                    }
                    autonat::NatStatus::Private => "behind NAT".into(),
                    autonat::NatStatus::Unknown => "unknown".into(),
                };
                self.last_rdv_publish = 0;
            }
            SwarmEvent::Behaviour(BehaviourEvent::Upnp(upnp::Event::NewExternalAddr(a))) => {
                if !self.external.contains(&a) {
                    self.external.push(a);
                }
                self.nat = "UPnP mapped".into();
                self.last_rdv_publish = 0;
            }
            SwarmEvent::Behaviour(BehaviourEvent::Kad(ev)) => self.on_kad(ev),
            _ => {}
        }
    }

    pub fn owner_of_peer(&self, peer: &str) -> Option<String> {
        if let Some(l) = &self.p.devices {
            if l.list.devices.iter().any(|d| d.peer_id == peer) {
                return Some("self".into());
            }
        }
        for c in &self.p.contacts {
            if !c.removed && c.devices.iter().any(|d| d.entry.peer_id == peer) {
                return Some(c.id.clone());
            }
        }
        for g in &self.p.groups {
            for m in &g.state.members {
                if m.devices.list.devices.iter().any(|d| d.peer_id == peer) {
                    return Some(m.account.clone());
                }
            }
        }
        None
    }
}
