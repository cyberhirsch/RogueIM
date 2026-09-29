//! The P2P engine: libp2p swarm + Olm sessions + persisted state.
//!
//! Prototype scope (see docs/PRD.md milestone M0/M1):
//! - LAN discovery via mDNS, direct dialing via addresses carried in invites and presence.
//! - Single-use invites with a reserved Olm one-time key (CT-10).
//! - Authorization request / accept / deny (CT-3).
//! - 1:1 Olm (Double Ratchet) messages, local outbox with retry until delivered.
//! - Signed-by-session presence with status, OS and device class (PR-1, PR-6, PR-8).
//! Not yet: Nostr mailbox, relays/hole punching, multi-device, groups, files.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::request_response::{self, OutboundRequestId, ProtocolSupport};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{mdns, noise, tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use vodozemac::olm::{Account, AccountPickle, OlmMessage, Session, SessionConfig, SessionPickle};
use vodozemac::Curve25519PublicKey;

use crate::device;
use crate::identity::{self, account_id, b64, pretty_fingerprint, unb64};
use crate::proto::*;
use crate::store::Store;

const PROTOCOL: &str = "/rim/msg/1";
const HEARTBEAT: Duration = Duration::from_secs(10);
const PRESENCE_TTL: Duration = Duration::from_secs(35);
const MAX_ADDRS: usize = 16;

// ---------- public API ----------

#[derive(Default)]
pub struct EngineConfig {
    /// Profile directory holding the encrypted state file.
    pub dir: PathBuf,
    pub passphrase: String,
    /// Required on first run (creates the account); ignored afterwards.
    pub nick: Option<String>,
    /// TCP/QUIC listen port; 0 picks a free one.
    pub port: u16,
    /// Simulation/testing only: report this OS code instead of the detected one.
    pub os_override: Option<String>,
    /// Simulation/testing only: report this device class instead of the detected one.
    pub device_override: Option<DeviceClass>,
}

#[derive(Debug, Clone)]
pub enum Command {
    SetStatus(Status),
    SetAwayMessage(String),
    NewInvite,
    AddContact { invite: String, text: String },
    Accept { id: String },
    Deny { id: String },
    SendText { id: String, body: String },
    OpenChat { id: String },
    CloseChat { id: String },
    Rename { id: String, name: String },
    Remove { id: String },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    Unlocked { nick: String, fingerprint: String, os: String, device_class: DeviceClass, status: Status, away_msg: String },
    LoginFailed(String),
    Contacts(Vec<ContactView>),
    Pending(Vec<PendingView>),
    History { id: String, name: String, fingerprint: String, lines: Vec<LineView> },
    Invite(String),
    Notice(String),
    Status(Status),
    /// A message arrived (for sounds / tray blinking).
    Incoming { id: String, name: String, text: String },
    Stopped,
}

#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl EngineHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

/// Start the engine on its own thread. Events arrive on the returned receiver.
pub fn spawn(cfg: EngineConfig) -> (EngineHandle, std::sync::mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("rim-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                if let Err(e) = run(cfg, rx, ev_tx.clone()).await {
                    let _ = ev_tx.send(Event::LoginFailed(format!("{e:#}")));
                }
                let _ = ev_tx.send(Event::Stopped);
            });
        })
        .expect("engine thread");
    (EngineHandle { tx }, ev_rx)
}

// ---------- persisted state ----------

#[derive(Serialize, Deserialize)]
struct Persist {
    nick: String,
    account_key: String,
    device_key: String,
    olm: AccountPickle,
    status: Status,
    away_msg: String,
    contacts: Vec<ContactRec>,
    pending: Vec<PendingRec>,
    invites: Vec<InviteRec>,
    sessions: HashMap<String, SessionPickle>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ContactRec {
    id: String,
    petname: String,
    card: Card,
    authorized: bool,
    awaiting: bool,
    addrs: Vec<String>,
    last_os: String,
    last_device: DeviceClass,
    history: Vec<LineRec>,
    outbox: Vec<OutRec>,
    unread: u32,
}

#[derive(Serialize, Deserialize, Clone)]
struct PendingRec {
    id: String,
    card: Card,
    text: String,
    addrs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct InviteRec {
    token: String,
    created: i64,
    used: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct LineRec {
    id: u64,
    ts: i64,
    from_me: bool,
    text: String,
    delivery: Delivery,
}

/// Already-encrypted message waiting for delivery. `msg_id` 0 = control message.
#[derive(Serialize, Deserialize, Clone)]
struct OutRec {
    msg_id: u64,
    req: WireReq,
}

// ---------- libp2p ----------

#[derive(NetworkBehaviour)]
struct Behaviour {
    rr: request_response::json::Behaviour<WireReq, WireResp>,
    mdns: mdns::tokio::Behaviour,
}

fn build_swarm(device: Keypair) -> Result<Swarm<Behaviour>> {
    let swarm = libp2p::SwarmBuilder::with_existing_identity(device)
        .with_tokio()
        .with_tcp(tcp::Config::default().nodelay(true), noise::Config::new, yamux::Config::default)?
        .with_quic()
        .with_behaviour(|key| {
            let rr = request_response::json::Behaviour::new(
                [(StreamProtocol::new(PROTOCOL), ProtocolSupport::Full)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(10)),
            );
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?;
            Ok(Behaviour { rr, mdns })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();
    Ok(swarm)
}

// ---------- engine ----------

struct Engine {
    store: Store,
    p: Persist,
    #[allow(dead_code)] // signs device lists once multi-device lands
    account_key: Keypair,
    olm: Account,
    sessions: HashMap<String, Session>,
    swarm: Swarm<Behaviour>,
    ev: std::sync::mpsc::Sender<Event>,
    my_card: Card,
    my_curve: String,
    os: String,
    device_class: DeviceClass,
    listen: Vec<Multiaddr>,
    presence: HashMap<String, (Presence, Instant)>,
    in_flight: HashMap<OutboundRequestId, (String, u64)>,
    in_flight_set: HashSet<(String, u64)>,
    open_chats: HashSet<String>,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn load_or_create(cfg: &EngineConfig) -> Result<(Store, Persist)> {
    if Store::exists(&cfg.dir) {
        let (store, plain) = Store::open(&cfg.dir, &cfg.passphrase)?;
        let p: Persist = serde_json::from_slice(&plain).context("state format")?;
        Ok((store, p))
    } else {
        let nick = cfg.nick.clone().filter(|n| !n.trim().is_empty()).ok_or_else(|| anyhow!("nickname required for a new account"))?;
        if cfg.passphrase.len() < 4 {
            bail!("passphrase too short (min. 4 characters)");
        }
        let store = Store::create(&cfg.dir, &cfg.passphrase)?;
        let p = Persist {
            nick: nick.trim().to_string(),
            account_key: b64(&Keypair::generate_ed25519().to_protobuf_encoding()?),
            device_key: b64(&Keypair::generate_ed25519().to_protobuf_encoding()?),
            olm: Account::new().pickle(),
            status: Status::Online,
            away_msg: String::new(),
            contacts: vec![],
            pending: vec![],
            invites: vec![],
            sessions: HashMap::new(),
        };
        Ok((store, p))
    }
}

async fn run(cfg: EngineConfig, mut rx: mpsc::UnboundedReceiver<Command>, ev: std::sync::mpsc::Sender<Event>) -> Result<()> {
    let (store, mut p) = load_or_create(&cfg)?;
    let account_key = Keypair::from_protobuf_encoding(&unb64(&p.account_key)?)?;
    let device_key = Keypair::from_protobuf_encoding(&unb64(&p.device_key)?)?;
    let olm = Account::from_pickle(std::mem::replace(&mut p.olm, Account::new().pickle()));
    let sessions = std::mem::take(&mut p.sessions)
        .into_iter()
        .map(|(k, v)| (k, Session::from_pickle(v)))
        .collect();
    let my_card = identity::make_card(&p.nick, &account_key, &device_key, &olm.curve25519_key())?;
    let my_curve = olm.curve25519_key().to_base64();

    let mut swarm = build_swarm(device_key)?;
    swarm.listen_on(format!("/ip4/0.0.0.0/tcp/{}", cfg.port).parse()?)?;
    swarm.listen_on(format!("/ip4/0.0.0.0/udp/{}/quic-v1", cfg.port).parse()?)?;

    let mut e = Engine {
        store,
        p,
        account_key,
        olm,
        sessions,
        swarm,
        ev,
        my_card,
        my_curve,
        os: cfg.os_override.clone().unwrap_or_else(device::os_code),
        device_class: cfg.device_override.unwrap_or_else(device::device_class),
        listen: vec![],
        presence: HashMap::new(),
        in_flight: HashMap::new(),
        in_flight_set: HashSet::new(),
        open_chats: HashSet::new(),
    };
    e.save();
    for c in e.p.contacts.clone() {
        e.register_addrs(&c.card.peer_id, &c.addrs);
    }
    let fp = pretty_fingerprint(&account_id(&e.my_card.account_pk)?);
    e.emit(Event::Unlocked {
        nick: e.p.nick.clone(),
        fingerprint: fp,
        os: e.os.clone(),
        device_class: e.device_class,
        status: e.p.status,
        away_msg: e.p.away_msg.clone(),
    });
    e.emit_contacts();
    e.emit_pending();

    let mut tick = tokio::time::interval(HEARTBEAT);
    loop {
        tokio::select! {
            ev = e.swarm.select_next_some() => e.on_swarm(ev),
            cmd = rx.recv() => match cmd {
                None | Some(Command::Shutdown) => {
                    e.go_offline().await;
                    e.save();
                    return Ok(());
                }
                Some(cmd) => {
                    if let Err(err) = e.on_command(cmd) {
                        e.emit(Event::Notice(format!("{err:#}")));
                    }
                }
            },
            _ = tick.tick() => e.heartbeat(),
        }
    }
}

impl Engine {
    fn emit(&self, ev: Event) {
        let _ = self.ev.send(ev);
    }

    fn save(&mut self) {
        self.p.olm = self.olm.pickle();
        self.p.sessions = self.sessions.iter().map(|(k, s)| (k.clone(), s.pickle())).collect();
        let res = serde_json::to_vec(&self.p).map_err(anyhow::Error::from).and_then(|b| self.store.save(&b));
        self.p.sessions.clear();
        if let Err(err) = res {
            self.emit(Event::Notice(format!("saving failed: {err:#}")));
        }
    }

    fn contact(&self, id: &str) -> Option<&ContactRec> {
        self.p.contacts.iter().find(|c| c.id == id)
    }

    fn contact_mut(&mut self, id: &str) -> Option<&mut ContactRec> {
        self.p.contacts.iter_mut().find(|c| c.id == id)
    }

    fn my_addrs(&self) -> Vec<String> {
        self.listen.iter().map(|a| a.to_string()).collect()
    }

    fn register_addrs(&mut self, peer: &str, addrs: &[String]) {
        let Ok(peer) = peer.parse::<PeerId>() else { return };
        for a in addrs {
            if let Ok(ma) = a.parse::<Multiaddr>() {
                self.swarm.add_peer_address(peer, ma);
            }
        }
    }

    fn merge_addrs(&mut self, id: &str, addrs: &[String]) {
        let peer = match self.contact_mut(id) {
            Some(c) => {
                for a in addrs {
                    if !c.addrs.contains(a) {
                        c.addrs.insert(0, a.clone());
                    }
                }
                c.addrs.truncate(MAX_ADDRS);
                c.card.peer_id.clone()
            }
            None => return,
        };
        self.register_addrs(&peer, addrs);
    }

    fn my_presence(&self) -> Presence {
        Presence {
            status: self.p.status,
            away_msg: self.p.away_msg.clone(),
            os: self.os.clone(),
            device_class: self.device_class,
            on_battery: device::on_battery(),
            addrs: self.my_addrs(),
        }
    }

    // ----- views -----

    fn contact_status(&self, c: &ContactRec) -> Status {
        match self.presence.get(&c.id) {
            Some((p, t)) if c.authorized && t.elapsed() < PRESENCE_TTL => p.status,
            _ => Status::Offline,
        }
    }

    fn emit_contacts(&self) {
        let mut v: Vec<ContactView> = self
            .p
            .contacts
            .iter()
            .map(|c| {
                let pres = self.presence.get(&c.id).filter(|(_, t)| t.elapsed() < PRESENCE_TTL);
                ContactView {
                    id: c.id.clone(),
                    name: c.petname.clone(),
                    status: self.contact_status(c),
                    os: pres.map(|(p, _)| p.os.clone()).unwrap_or_else(|| c.last_os.clone()),
                    device_class: pres.map(|(p, _)| p.device_class).unwrap_or(c.last_device),
                    on_battery: pres.map(|(p, _)| p.on_battery).unwrap_or(false),
                    away_msg: pres.map(|(p, _)| p.away_msg.clone()).unwrap_or_default(),
                    awaiting: c.awaiting,
                    unread: c.unread,
                    fingerprint: pretty_fingerprint(&c.id),
                }
            })
            .collect();
        // Online first (by status order), then name.
        v.sort_by_key(|c| (c.status == Status::Offline, c.name.to_lowercase()));
        self.emit(Event::Contacts(v));
    }

    fn emit_pending(&self) {
        let v = self
            .p
            .pending
            .iter()
            .map(|p| PendingView {
                id: p.id.clone(),
                nick: p.card.nick.clone(),
                fingerprint: pretty_fingerprint(&p.id),
                text: p.text.clone(),
            })
            .collect();
        self.emit(Event::Pending(v));
    }

    fn emit_history(&self, id: &str) {
        let Some(c) = self.contact(id) else { return };
        self.emit(Event::History {
            id: id.to_string(),
            name: c.petname.clone(),
            fingerprint: pretty_fingerprint(id),
            lines: c
                .history
                .iter()
                .map(|l| LineView { ts: l.ts, from_me: l.from_me, text: l.text.clone(), delivery: l.delivery })
                .collect(),
        });
    }

    // ----- crypto helpers -----

    fn encrypt_to(&mut self, id: &str, inner: &Inner) -> Result<WireReq> {
        let s = self.sessions.get_mut(id).ok_or_else(|| anyhow!("no session with contact"))?;
        let msg = s.encrypt(serde_json::to_vec(inner)?).map_err(|e| anyhow!("encrypt: {e}"))?;
        Ok(WireReq { sender_curve: self.my_curve.clone(), msg })
    }

    fn decrypt_from(&mut self, id: &str, curve: &str, msg: &OlmMessage) -> Result<Inner> {
        if let Some(s) = self.sessions.get_mut(id) {
            if let Ok(plain) = s.decrypt(msg) {
                return Ok(serde_json::from_slice(&plain)?);
            }
        }
        // Unknown or replaced session: a pre-key message can start a new one.
        if let OlmMessage::PreKey(pk) = msg {
            let curve = Curve25519PublicKey::from_base64(curve).map_err(|e| anyhow!("{e}"))?;
            let r = self
                .olm
                .create_inbound_session(SessionConfig::version_1(), curve, pk)
                .map_err(|e| anyhow!("inbound session: {e}"))?;
            self.sessions.insert(id.to_string(), r.session);
            return Ok(serde_json::from_slice(&r.plaintext)?);
        }
        bail!("cannot decrypt message")
    }

    // ----- sending -----

    fn send_wire(&mut self, id: &str, req: WireReq, msg_id: u64) {
        let Some(peer) = self.contact(id).and_then(|c| c.card.peer_id.parse::<PeerId>().ok()) else { return };
        let rid = self.swarm.behaviour_mut().rr.send_request(&peer, req);
        if msg_id != u64::MAX {
            self.in_flight.insert(rid, (id.to_string(), msg_id));
            self.in_flight_set.insert((id.to_string(), msg_id));
        }
    }

    /// Queue an encrypted message in the contact's outbox and try to send it.
    fn enqueue(&mut self, id: &str, inner: &Inner, msg_id: u64) -> Result<()> {
        let req = self.encrypt_to(id, inner)?;
        self.contact_mut(id).ok_or_else(|| anyhow!("unknown contact"))?.outbox.push(OutRec { msg_id, req: req.clone() });
        self.save();
        self.send_wire(id, req, msg_id);
        Ok(())
    }

    fn flush_outbox(&mut self, id: &str) {
        let Some(c) = self.contact(id) else { return };
        let items: Vec<OutRec> = c
            .outbox
            .iter()
            .filter(|o| !self.in_flight_set.contains(&(id.to_string(), o.msg_id)))
            .cloned()
            .collect();
        for o in items {
            self.send_wire(id, o.req, o.msg_id);
        }
    }

    fn send_presence(&mut self, id: &str) {
        if self.p.status == Status::Invisible {
            return;
        }
        let authorized = self.contact(id).map(|c| c.authorized).unwrap_or(false);
        if !authorized {
            return;
        }
        let pres = Inner::Presence(self.my_presence());
        if let Ok(req) = self.encrypt_to(id, &pres) {
            // Presence is fire-and-forget: never queued, not tracked.
            self.send_wire(id, req, u64::MAX);
        }
    }

    fn is_connected(&self, id: &str) -> bool {
        self.contact(id)
            .and_then(|c| c.card.peer_id.parse::<PeerId>().ok())
            .map(|p| self.swarm.is_connected(&p))
            .unwrap_or(false)
    }

    fn heartbeat(&mut self) {
        let ids: Vec<String> = self.p.contacts.iter().map(|c| c.id.clone()).collect();
        for id in &ids {
            if self.is_connected(id) {
                // Presence only over live connections, so offline contacts
                // don't make our ratchet run ahead.
                self.send_presence(id);
                self.flush_outbox(id);
            } else if let Some(peer) = self.contact(id).and_then(|c| c.card.peer_id.parse::<PeerId>().ok()) {
                let _ = self.swarm.dial(peer);
            }
        }
        let before = self.presence.len();
        self.presence.retain(|_, (_, t)| t.elapsed() < PRESENCE_TTL);
        if self.presence.len() != before {
            self.emit_contacts();
        }
    }

    async fn go_offline(&mut self) {
        let mut off = self.my_presence();
        off.status = Status::Offline;
        let ids: Vec<String> = self.p.contacts.iter().filter(|c| c.authorized).map(|c| c.id.clone()).collect();
        let mut sent = false;
        for id in ids {
            if self.is_connected(&id) {
                if let Ok(req) = self.encrypt_to(&id, &Inner::Presence(off.clone())) {
                    self.send_wire(&id, req, u64::MAX);
                    sent = true;
                }
            }
        }
        if sent {
            // Give the swarm a moment to flush.
            let deadline = tokio::time::sleep(Duration::from_millis(600));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    ev = self.swarm.select_next_some() => { if let SwarmEvent::Behaviour(_) = ev {} }
                }
            }
        }
    }

    // ----- commands -----

    fn on_command(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::SetStatus(s) => {
                self.p.status = s;
                self.save();
                self.emit(Event::Status(s));
                let ids: Vec<String> = self.p.contacts.iter().map(|c| c.id.clone()).collect();
                for id in ids {
                    if self.is_connected(&id) {
                        self.send_presence(&id);
                    }
                }
            }
            Command::SetAwayMessage(m) => {
                self.p.away_msg = m;
                self.save();
            }
            Command::NewInvite => {
                let otk = self.olm.generate_one_time_keys(1).created.into_iter().next().ok_or_else(|| anyhow!("no one-time key"))?;
                self.olm.mark_keys_as_published();
                let token = hex::encode(rand::random::<[u8; 16]>());
                self.p.invites.push(InviteRec { token: token.clone(), created: now(), used: false });
                let inv = Invite { v: 1, card: self.my_card.clone(), otk: otk.to_base64(), token, addrs: self.my_addrs() };
                self.save();
                self.emit(Event::Invite(identity::encode_invite(&inv)?));
            }
            Command::AddContact { invite, text } => self.add_contact(&invite, &text)?,
            Command::Accept { id } => {
                let idx = self.p.pending.iter().position(|p| p.id == id).ok_or_else(|| anyhow!("no such request"))?;
                let pr = self.p.pending.remove(idx);
                self.p.contacts.push(ContactRec {
                    id: pr.id.clone(),
                    petname: pr.card.nick.clone(),
                    card: pr.card.clone(),
                    authorized: true,
                    awaiting: false,
                    addrs: pr.addrs.clone(),
                    last_os: String::new(),
                    last_device: DeviceClass::Desktop,
                    history: vec![],
                    outbox: vec![],
                    unread: 0,
                });
                self.register_addrs(&pr.card.peer_id, &pr.addrs);
                let accept = Inner::AuthAccept { card: self.my_card.clone(), addrs: self.my_addrs() };
                self.enqueue(&id, &accept, 0)?;
                self.send_presence(&id);
                self.emit_pending();
                self.emit_contacts();
            }
            Command::Deny { id } => {
                if self.p.pending.iter().any(|p| p.id == id) {
                    // Tell them, best effort, then forget the session.
                    let pr = self.p.pending.iter().find(|p| p.id == id).cloned().unwrap();
                    if let Ok(req) = self.encrypt_to(&id, &Inner::AuthDeny) {
                        if let Ok(peer) = pr.card.peer_id.parse::<PeerId>() {
                            self.swarm.behaviour_mut().rr.send_request(&peer, req);
                        }
                    }
                    self.p.pending.retain(|p| p.id != id);
                    self.sessions.remove(&id);
                    self.save();
                    self.emit_pending();
                }
            }
            Command::SendText { id, body } => {
                let body = body.trim_end().to_string();
                if body.is_empty() {
                    return Ok(());
                }
                let c = self.contact(&id).ok_or_else(|| anyhow!("unknown contact"))?;
                if !c.authorized {
                    bail!("{} has not authorized you yet", c.petname);
                }
                let msg_id = rand::random::<u64>() | 1;
                let ts = now();
                let inner = Inner::Text { id: msg_id, ts, body: body.clone() };
                self.contact_mut(&id).unwrap().history.push(LineRec { id: msg_id, ts, from_me: true, text: body, delivery: Delivery::Queued });
                self.enqueue(&id, &inner, msg_id)?;
                self.emit_history(&id);
            }
            Command::OpenChat { id } => {
                self.open_chats.insert(id.clone());
                if let Some(c) = self.contact_mut(&id) {
                    c.unread = 0;
                }
                self.emit_history(&id);
                self.emit_contacts();
            }
            Command::CloseChat { id } => {
                self.open_chats.remove(&id);
            }
            Command::Rename { id, name } => {
                if let Some(c) = self.contact_mut(&id) {
                    c.petname = name;
                }
                self.save();
                self.emit_contacts();
            }
            Command::Remove { id } => {
                self.p.contacts.retain(|c| c.id != id);
                self.sessions.remove(&id);
                self.presence.remove(&id);
                self.save();
                self.emit_contacts();
            }
            Command::Shutdown => {}
        }
        Ok(())
    }

    fn add_contact(&mut self, invite: &str, text: &str) -> Result<()> {
        let inv = identity::decode_invite(invite)?;
        let id = account_id(&inv.card.account_pk)?;
        if id == account_id(&self.my_card.account_pk)? {
            bail!("that is your own invite");
        }
        if self.contact(&id).is_some() {
            bail!("{} is already in your list", inv.card.nick);
        }
        let their_curve = Curve25519PublicKey::from_base64(&inv.card.curve).map_err(|e| anyhow!("{e}"))?;
        let otk = Curve25519PublicKey::from_base64(&inv.otk).map_err(|e| anyhow!("{e}"))?;
        let session = self
            .olm
            .create_outbound_session(SessionConfig::version_1(), their_curve, otk)
            .map_err(|e| anyhow!("session: {e}"))?;
        self.sessions.insert(id.clone(), session);
        self.p.contacts.push(ContactRec {
            id: id.clone(),
            petname: inv.card.nick.clone(),
            card: inv.card.clone(),
            authorized: false,
            awaiting: true,
            addrs: inv.addrs.clone(),
            last_os: String::new(),
            last_device: DeviceClass::Desktop,
            history: vec![],
            outbox: vec![],
            unread: 0,
        });
        self.register_addrs(&inv.card.peer_id, &inv.addrs);
        let req = Inner::AuthRequest {
            token: inv.token,
            card: self.my_card.clone(),
            text: text.to_string(),
            addrs: self.my_addrs(),
        };
        self.enqueue(&id, &req, 0)?;
        self.emit_contacts();
        self.emit(Event::Notice(format!("Authorization request sent to {}.", inv.card.nick)));
        Ok(())
    }

    // ----- swarm events -----

    fn on_swarm(&mut self, ev: SwarmEvent<BehaviourEvent>) {
        match ev {
            SwarmEvent::NewListenAddr { address, .. } => {
                if !self.listen.contains(&address) {
                    self.listen.push(address);
                }
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                if let Some(id) = self.id_for_peer(&peer_id) {
                    self.send_presence(&id);
                    self.flush_outbox(&id);
                }
            }
            SwarmEvent::ConnectionClosed { peer_id, num_established: 0, .. } => {
                if let Some(id) = self.id_for_peer(&peer_id) {
                    if self.presence.remove(&id).is_some() {
                        self.emit_contacts();
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                let mut contacts_seen = HashSet::new();
                for (peer, addr) in list {
                    self.swarm.add_peer_address(peer, addr);
                    if let Some(id) = self.id_for_peer(&peer) {
                        contacts_seen.insert((peer, id));
                    }
                }
                for (peer, _) in contacts_seen {
                    if !self.swarm.is_connected(&peer) {
                        let _ = self.swarm.dial(peer);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(_)) => {}
            SwarmEvent::Behaviour(BehaviourEvent::Rr(ev)) => self.on_rr(ev),
            _ => {}
        }
    }

    fn id_for_peer(&self, peer: &PeerId) -> Option<String> {
        let s = peer.to_string();
        self.p.contacts.iter().find(|c| c.card.peer_id == s).map(|c| c.id.clone())
    }

    fn on_rr(&mut self, ev: request_response::Event<WireReq, WireResp>) {
        use request_response::{Event as E, Message as M};
        match ev {
            E::Message { peer, message: M::Request { request, channel, .. }, .. } => {
                let ok = match self.on_request(peer, request) {
                    Ok(()) => true,
                    Err(err) => {
                        tracing::debug!("rejected request from {peer}: {err:#}");
                        false
                    }
                };
                let _ = self.swarm.behaviour_mut().rr.send_response(channel, WireResp { ok });
            }
            E::Message { message: M::Response { request_id, response }, .. } => {
                if let Some((id, msg_id)) = self.in_flight.remove(&request_id) {
                    self.in_flight_set.remove(&(id.clone(), msg_id));
                    self.delivered(&id, msg_id, response.ok);
                }
            }
            E::OutboundFailure { request_id, .. } => {
                // Stays in the outbox; retried on the next connection or heartbeat.
                if let Some((id, msg_id)) = self.in_flight.remove(&request_id) {
                    self.in_flight_set.remove(&(id, msg_id));
                }
            }
            _ => {}
        }
    }

    fn delivered(&mut self, id: &str, msg_id: u64, ok: bool) {
        let Some(c) = self.contact_mut(id) else { return };
        c.outbox.retain(|o| o.msg_id != msg_id);
        if msg_id != 0 {
            if let Some(l) = c.history.iter_mut().find(|l| l.id == msg_id) {
                l.delivery = if ok { Delivery::Delivered } else { Delivery::Failed };
            }
        }
        self.save();
        if msg_id != 0 {
            self.emit_history(id);
        }
    }

    fn on_request(&mut self, peer: PeerId, req: WireReq) -> Result<()> {
        let peer_s = peer.to_string();
        // Known contact?
        if let Some(c) = self.p.contacts.iter().find(|c| c.card.curve == req.sender_curve).cloned() {
            if c.card.peer_id != peer_s {
                bail!("curve key used from wrong device");
            }
            let inner = self.decrypt_from(&c.id, &req.sender_curve, &req.msg)?;
            self.on_inner(&c.id, inner);
            self.save();
            return Ok(());
        }
        // Repeated request from someone already pending: decrypt to keep the ratchet in step.
        if let Some(pr) = self.p.pending.iter().find(|p| p.card.curve == req.sender_curve).cloned() {
            let _ = self.decrypt_from(&pr.id, &req.sender_curve, &req.msg)?;
            return Ok(());
        }
        // Stranger: only a pre-key message carrying a valid invite token gets through.
        let OlmMessage::PreKey(pk) = &req.msg else { bail!("unknown sender") };
        let curve = Curve25519PublicKey::from_base64(&req.sender_curve).map_err(|e| anyhow!("{e}"))?;
        let r = self
            .olm
            .create_inbound_session(SessionConfig::version_1(), curve, pk)
            .map_err(|e| anyhow!("inbound session: {e}"))?;
        let Inner::AuthRequest { token, card, text, addrs } = serde_json::from_slice(&r.plaintext)? else {
            bail!("first message must be an authorization request");
        };
        identity::verify_card(&card)?;
        if card.curve != req.sender_curve || card.peer_id != peer_s {
            bail!("card does not match sender");
        }
        let inv = self
            .p
            .invites
            .iter_mut()
            .find(|i| i.token == token && !i.used)
            .ok_or_else(|| anyhow!("invalid or used invite token"))?;
        inv.used = true;
        let id = account_id(&card.account_pk)?;
        self.sessions.insert(id.clone(), r.session);
        self.p.pending.push(PendingRec { id, card, text, addrs });
        self.save();
        self.emit_pending();
        Ok(())
    }

    fn on_inner(&mut self, id: &str, inner: Inner) {
        match inner {
            Inner::AuthAccept { card, addrs } => {
                let Some(c) = self.contact_mut(id) else { return };
                if c.card.account_pk != card.account_pk {
                    return;
                }
                c.authorized = true;
                c.awaiting = false;
                let name = c.petname.clone();
                self.merge_addrs(id, &addrs);
                self.emit(Event::Notice(format!("{name} accepted your authorization request.")));
                self.send_presence(id);
                self.emit_contacts();
            }
            Inner::AuthDeny => {
                let name = self.contact(id).map(|c| c.petname.clone()).unwrap_or_default();
                self.p.contacts.retain(|c| c.id != id);
                self.sessions.remove(id);
                self.emit(Event::Notice(format!("{name} declined your authorization request.")));
                self.emit_contacts();
            }
            Inner::AuthRequest { .. } => {}
            Inner::Text { id: msg_id, ts, body } => {
                let open = self.open_chats.contains(id);
                let Some(c) = self.contact_mut(id) else { return };
                if !c.authorized || c.history.iter().any(|l| l.id == msg_id && !l.from_me) {
                    return;
                }
                c.history.push(LineRec { id: msg_id, ts, from_me: false, text: body.clone(), delivery: Delivery::Received });
                if !open {
                    c.unread += 1;
                }
                let name = c.petname.clone();
                self.emit(Event::Incoming { id: id.to_string(), name, text: body });
                self.emit_history(id);
                self.emit_contacts();
            }
            Inner::Presence(pres) => {
                let Some(c) = self.contact_mut(id) else { return };
                if !c.authorized {
                    return;
                }
                c.last_os = pres.os.clone();
                c.last_device = pres.device_class;
                let addrs = pres.addrs.clone();
                let first = !self.presence.contains_key(id);
                if pres.status == Status::Offline {
                    self.presence.remove(id);
                } else {
                    self.presence.insert(id.to_string(), (pres, Instant::now()));
                }
                self.merge_addrs(id, &addrs);
                if first {
                    // Answer right away so both sides light up together.
                    self.send_presence(id);
                }
                self.emit_contacts();
            }
        }
    }
}
