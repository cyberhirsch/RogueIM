//! The engine: libp2p swarm + Olm/Megolm sessions + Nostr mailbox + persisted
//! state. One per profile, on its own thread; the UI talks to it through
//! [`Command`]s and receives [`Event`]s.

mod account;
mod call;
mod screen;
pub use call::MediaPipe;
pub use screen::{ScreenFrame, ScreenPipe};
mod files;
mod groups;
mod mail;
mod msg;
mod net;
mod signal;
mod state;
mod views;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::request_response::OutboundRequestId;
use libp2p::{Multiaddr, PeerId, Swarm};
use tokio::sync::mpsc;
use vodozemac::megolm::{GroupSession, InboundGroupSession};
use vodozemac::olm::{Account, Session};

use crate::mailbox::Mailbox;
use crate::proto::*;
use crate::store::Store;

pub(crate) use state::*;

// ---------------------------------------------------------------- public API

#[derive(Default, Clone)]
pub enum StartMode {
    /// Open the profile, or create a new account if there is none.
    #[default]
    Auto,
    /// New device that will be linked to an existing account.
    Link,
    /// Restore an account from a `.rimbackup` file as a new device.
    Restore { path: PathBuf, secret: RestoreSecret },
}

#[derive(Clone)]
pub enum RestoreSecret {
    Passphrase(String),
    Recovery(String),
}

impl std::fmt::Debug for RestoreSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RestoreSecret(..)")
    }
}

#[derive(Default)]
pub struct EngineConfig {
    /// Profile directory holding the encrypted state file.
    pub dir: PathBuf,
    pub passphrase: String,
    /// Required on first run (creates the account); ignored afterwards.
    pub nick: Option<String>,
    /// TCP/QUIC listen port; 0 picks a free one.
    pub port: u16,
    pub mode: StartMode,
    pub device_name: Option<String>,
    /// Simulation/testing only: report this OS code instead of the detected one.
    pub os_override: Option<String>,
    /// Simulation/testing only: report this device class instead of the detected one.
    pub device_override: Option<DeviceClass>,
    /// Report as a bot (badge in contacts' lists).
    pub bot: bool,
    /// Headless node: relay server, DHT server, stores mail for others.
    pub node: bool,
    /// Override the Nostr relay list (tests: `Some(vec![])` disables Nostr).
    pub relays: Option<Vec<String>>,
    /// Override LAN-only mode.
    pub lan_only: Option<bool>,
    /// Disable mDNS (tests that must go through another path).
    pub no_mdns: bool,
    /// Listen on 127.0.0.1 only (tests: no firewall prompts, no LAN exposure).
    pub loopback: bool,
    /// Tests only: drop every direct connection to contacts, as if two
    /// routers stood in between, so only the Nostr paths remain.
    #[doc(hidden)]
    pub no_direct: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    // presence & profile
    SetStatus(Status),
    SetAwayMessage(String),
    SetAutoReply(bool),
    SetReadReceipts(bool),
    SetProfile(Profile),
    /// Seconds since the last user input (auto-away is decided by the UI and
    /// arrives as SetStatus; this only feeds "background" for presence).
    SetBackground(bool),
    /// Share what is playing (empty = nothing).
    SetNowPlaying(String),
    /// Share a plugin's state with our other devices.
    SyncPluginState { plugin: String, state: String },
    // invites & contacts
    NewInvite { uses: Option<u32>, ttl_secs: Option<u64>, label: String },
    RevokeInvite { token: String },
    AddContact { invite: String, text: String },
    Accept { id: String },
    Deny { id: String },
    Rename { id: String, name: String },
    Remove { id: String },
    SetFolder { id: String, folder: String },
    SetFolders(Vec<String>),
    SetVisibility { id: String, visibility: Visibility },
    SetIgnored { id: String, ignored: bool },
    SetVerified { id: String, verified: bool },
    SafetyNumber { id: String },
    Introduce { to: String, whom: String },
    /// `trust`: take over the introducer's verification if they had verified the contact.
    AcceptIntroduction { index: usize, text: String, trust: bool },
    DismissIntroduction { index: usize },
    SetUrgentAllowed { id: String, allowed: bool },
    SetNotifyOnline { id: String, on: bool },
    SetAutoAccept { id: String, on: bool },
    SetNick(String),
    SetSendTyping(bool),
    SetBandwidth(u32),
    /// Use public libp2p nodes for our outside address and relay help.
    SetPublicHelpers(bool),
    /// Voice call (1:1).
    CallStart { id: String },
    CallAccept,
    CallDecline,
    CallHangup,
    /// Share our screen in the current call (true) or stop (false).
    ScreenShare(bool),
    /// The viewer needs a keyframe.
    ScreenWantKey,
    // messaging
    SendText { id: String, body: String, reply_to: Option<u64>, urgent: bool },
    EditText { id: String, msg: u64, body: String },
    DeleteText { id: String, msg: u64 },
    Typing { id: String, typing: bool },
    SetDisappearing { id: String, ttl: Option<u64> },
    OpenChat { id: String },
    CloseChat { id: String },
    SendNote(String),
    OpenNotes,
    Search(String),
    // files
    SendFile { id: String, path: String },
    AcceptFile { file: String, dir: String },
    DeclineFile { file: String },
    CancelFile { file: String },
    PauseFile { file: String },
    ResumeFile { file: String },
    // groups
    CreateGroup { name: String, members: Vec<String> },
    GroupAdd { group: String, id: String },
    GroupRemove { group: String, account: String },
    GroupRename { group: String, name: String },
    GroupLeave { group: String },
    GroupSend { group: String, text: String },
    OpenGroup { group: String },
    CloseGroup { group: String },
    // devices
    LinkDevice { code: String, manager: bool, history_days: Option<u32> },
    RenameDevice { peer: String, name: String },
    RevokeDevice { peer: String, wipe: bool },
    RemoteLock { peer: String },
    // backup
    ExportBackup { path: String, include_files: bool },
    ImportHistory { path: String, secret: RestoreSecret },
    SetBackupSchedule { dir: String, every_hours: u32, keep: u32 },
    ShowRecoveryKey,
    // network
    SetRelays(Vec<String>),
    SetBootstrap(Vec<String>),
    SetLanOnly(bool),
    SetHelper(bool),
    NetInfo,
    // account
    WipeAccount,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    Unlocked { nick: String, fingerprint: String, os: String, device_class: DeviceClass, status: Status, away_msg: String },
    LoginFailed(String),
    /// A new account was created: show these 24 words once.
    RecoveryKey(String),
    /// Link mode: show this code and words on the new device.
    LinkCode { code: String, words: String },
    Linked,
    Contacts(Vec<ContactView>),
    Pending(Vec<PendingView>),
    Introductions(Vec<(usize, String, String, bool)>),
    Invites(Vec<InviteView>),
    Folders(Vec<String>),
    History { id: String, name: String, fingerprint: String, lines: Vec<LineView> },
    Notes(Vec<LineView>),
    Groups(Vec<GroupView>),
    GroupHistory { group: String, name: String, lines: Vec<LineView> },
    Devices(Vec<DeviceView>),
    Invite(String),
    Notice(String),
    Status(Status),
    SafetyNumber { id: String, number: String },
    SearchResults(Vec<(String, String, LineView)>),
    /// A message arrived (for sounds / tray blinking / pop-ups).
    Incoming { id: String, name: String, text: String, urgent: bool },
    GroupIncoming { group: String, name: String, from: String, text: String },
    ContactOnline { id: String, name: String },
    AuthRequested { name: String },
    FileOffered { id: String, from: String, name: String, size: u64 },
    Files(Vec<(String, FileView)>),
    Net(NetView),
    Profile(Profile),
    PluginState { plugin: String, state: String },
    Settings { auto_reply: bool, read_receipts: bool, relays: Vec<String>, bootstrap: Vec<String>, lan_only: bool, helper: bool, backup_dir: String, backup_hours: u32, backup_keep: u32, nick: String, send_typing: bool, bandwidth_kbps: u32, public_helpers: bool },
    Call(CallView),
    /// A call became active: the audio path for the app.
    CallMedia(std::sync::Arc<call::MediaPipe>),
    /// We started sharing: where the app puts encoded frames.
    ScreenOut(std::sync::Arc<screen::ScreenPipe>),
    /// They started sharing: where the app gets frames to show.
    ScreenIn(std::sync::Arc<screen::ScreenPipe>),
    /// Another of our devices asked this one to lock.
    Locked,
    /// This device was wiped (remotely or by the user); the profile is gone.
    Wiped,
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

// ---------------------------------------------------------------- engine

pub(crate) const HEARTBEAT: Duration = Duration::from_secs(10);
pub(crate) const PRESENCE_TTL: Duration = Duration::from_secs(35);
pub(crate) const MAX_SESSIONS: usize = 4;

pub(crate) struct PresenceRec {
    pub owner: String,
    pub pres: Presence,
    pub at: Instant,
    /// How long this record counts without a refresh (longer when it came
    /// over Nostr, which is refreshed less often).
    pub ttl: Duration,
}

/// Messages from background tasks (Nostr, file IO) back into the loop.
pub(crate) enum Internal {
    Mail(Vec<crate::mailbox::MailItem>),
    MailPosted { contact: String, device: String, msg_id: u64, ok: bool },
    Rdv { peer: String, addrs: Vec<String> },
    RelayStatus(Vec<(String, bool)>),
    Live(crate::mailbox::Live),
    /// Try the hole punch to this device again.
    Punch { peer: String, addrs: Vec<String>, round: u8 },
}

pub(crate) struct Engine {
    pub store: Store,
    pub p: Persist,
    pub passphrase: String,
    pub account_key: Option<Keypair>,
    pub device_key: Keypair,
    pub peer_id: PeerId,
    pub olm: Account,
    pub sessions: HashMap<String, Vec<Session>>,
    pub group_out: HashMap<String, GroupSession>,
    pub group_in: HashMap<String, HashMap<String, InboundGroupSession>>,
    pub swarm: Swarm<net::Behaviour>,
    pub ev: std::sync::mpsc::Sender<Event>,
    pub os: String,
    pub device_class: DeviceClass,
    pub bot: bool,
    pub node: bool,
    pub background: bool,
    pub now_playing: String,
    pub listen: Vec<Multiaddr>,
    pub external: Vec<Multiaddr>,
    pub presence: HashMap<String, PresenceRec>,
    /// Listen addresses peers told us via identify, by peer id.
    pub seen_addrs: HashMap<String, Vec<String>>,
    pub typing: HashMap<String, Instant>,
    pub in_flight: HashMap<OutboundRequestId, (String, String, u64)>,
    pub in_flight_set: HashSet<(String, u64)>,
    pub open_chats: HashSet<String>,
    pub open_groups: HashSet<String>,
    pub mailbox: Option<Mailbox>,
    /// Loopback-only (tests): no public helper nodes.
    pub loopback: bool,
    pub no_direct: bool,
    /// Our public address as the internet sees it (from public peers' identify).
    pub observed: Vec<Multiaddr>,
    /// Day of the current live Nostr subscription.
    pub live_day: i64,
    pub last_signal_presence: i64,
    /// When we last asked a device to hole-punch.
    pub punched: HashMap<String, i64>,
    /// When we last reset the sessions with a device (by curve key).
    pub unwedged: HashMap<String, i64>,
    pub call: Option<call::CallRec>,
    /// Encoded frames from the app while a call is active.
    pub call_out: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    pub screen_out: Option<mpsc::UnboundedReceiver<screen::ScreenFrame>>,
    pub internal_tx: mpsc::UnboundedSender<Internal>,
    pub relay_status: Vec<(String, bool)>,
    pub nat: String,
    pub last_rdv_publish: i64,
    pub last_mail_fetch: i64,
    pub mail_fetching: bool,
    pub files_rt: files::FilesRuntime,
    /// Group traffic that arrived before its group or key: (owner, device, body, when).
    pub deferred: Vec<(String, String, Body, Instant)>,
    pub quitting: bool,
}

impl Engine {
    pub fn emit(&self, ev: Event) {
        let _ = self.ev.send(ev);
    }

    pub fn notice(&self, s: impl Into<String>) {
        self.emit(Event::Notice(s.into()));
    }

    pub fn save(&mut self) {
        self.p.olm = self.olm.pickle();
        self.p.sessions = self
            .sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().map(|s| s.pickle()).collect()))
            .collect();
        for g in &mut self.p.groups {
            if let Some(o) = self.group_out.get(&g.state.id) {
                g.outbound = Some(o.pickle());
            }
            if let Some(ins) = self.group_in.get(&g.state.id) {
                g.inbound = ins.iter().map(|(k, s)| (k.clone(), s.pickle())).collect();
            }
        }
        let res = serde_json::to_vec(&self.p).map_err(anyhow::Error::from).and_then(|b| self.store.save(&b));
        self.p.sessions.clear();
        if let Err(err) = res {
            self.notice(format!("saving failed: {err:#}"));
        }
    }

    pub fn my_account_id(&self) -> String {
        crate::identity::account_id(&self.p.account_pk).unwrap_or_default()
    }

    pub fn contact(&self, id: &str) -> Option<&ContactRec> {
        self.p.contacts.iter().find(|c| c.id == id && !c.removed)
    }

    pub fn contact_mut(&mut self, id: &str) -> Option<&mut ContactRec> {
        self.p.contacts.iter_mut().find(|c| c.id == id && !c.removed)
    }

    pub fn my_addrs(&self) -> Vec<String> {
        // Direct routes first, relay circuits last.
        let relayed = |a: &&Multiaddr| net::is_relayed(a);
        let mut v: Vec<String> = self
            .listen
            .iter()
            .chain(self.observed.iter())
            .chain(self.external.iter().filter(|a| !relayed(a)))
            .chain(self.external.iter().filter(relayed))
            .map(|a| a.to_string())
            .collect();
        v.dedup();
        v
    }

    pub fn my_seeds(&self) -> DeviceSeeds {
        self.p.own_seeds.get(&self.peer_id.to_string()).cloned().unwrap_or_default()
    }
}

async fn run(cfg: EngineConfig, mut rx: mpsc::UnboundedReceiver<Command>, ev: std::sync::mpsc::Sender<Event>) -> Result<()> {
    let (itx, mut irx) = mpsc::unbounded_channel();
    let mut e = account::open(cfg, ev, itx).await?;
    e.after_start();

    let mut tick = tokio::time::interval(HEARTBEAT);
    let mut fast = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            ev = e.swarm.select_next_some() => e.on_swarm(ev),
            cmd = rx.recv() => match cmd {
                None | Some(Command::Shutdown) => {
                    e.go_offline().await;
                    e.save();
                    if let Some(m) = &e.mailbox { m.shutdown().await; }
                    return Ok(());
                }
                Some(cmd) => {
                    if let Err(err) = e.on_command(cmd) {
                        e.notice(format!("{err:#}"));
                    }
                    if e.quitting {
                        e.flush_before_exit().await;
                        if let Some(m) = &e.mailbox { m.shutdown().await; }
                        return Ok(());
                    }
                }
            },
            Some(msg) = irx.recv() => e.on_internal(msg),
            frame = async {
                match e.call_out.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match frame {
                Some(f) => e.call_send_frame(f),
                None => e.call_out = None,
            },
            shot = async {
                match e.screen_out.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match shot {
                Some(f) => e.screen_send_frame(f),
                None => e.screen_stop_local(),
            },
            _ = tick.tick() => e.heartbeat(),
            _ = fast.tick() => e.files_tick(),
        }
        if e.quitting {
            e.flush_before_exit().await;
            if let Some(m) = &e.mailbox {
                m.shutdown().await;
            }
            return Ok(());
        }
    }
}

impl Engine {
    /// Give requests already on their way (e.g. the confirmation of a remote
    /// lock) a moment to leave before the swarm is dropped.
    async fn flush_before_exit(&mut self) {
        let end = tokio::time::Instant::now() + Duration::from_millis(1500);
        while !self.in_flight.is_empty() && tokio::time::Instant::now() < end {
            tokio::select! {
                ev = self.swarm.select_next_some() => self.on_swarm(ev),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    fn after_start(&mut self) {
        if let Some(l) = &self.p.linking {
            let code = l.code.clone();
            self.emit(Event::LinkCode { words: crate::identity::sas_words(&code), code });
            return;
        }
        self.emit_unlocked();
        self.emit_all();
    }

    pub fn emit_unlocked(&self) {
        let fp = crate::identity::pretty_fingerprint(&self.my_account_id());
        self.emit(Event::Unlocked {
            nick: self.p.nick.clone(),
            fingerprint: fp,
            os: self.os.clone(),
            device_class: self.device_class,
            status: self.p.status,
            away_msg: self.p.away_msg.clone(),
        });
    }

    pub fn emit_all(&self) {
        self.emit_contacts();
        self.emit_pending();
        self.emit_intros();
        self.emit_invites();
        self.emit_groups();
        self.emit_devices();
        self.emit_files();
        self.emit_settings();
        self.emit(Event::Folders(self.p.folders.clone()));
        self.emit(Event::Profile(self.p.profile.clone()));
    }

    fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Mail(items) => self.on_mail(items),
            Internal::MailPosted { contact, device, msg_id, ok } => self.on_mail_posted(&contact, &device, msg_id, ok),
            Internal::Rdv { peer, addrs } => self.on_rdv(&peer, addrs),
            Internal::RelayStatus(s) => self.relay_status = s,
            Internal::Live(crate::mailbox::Live::Mail(item)) => self.take_mail(vec![item]),
            Internal::Live(crate::mailbox::Live::Signal(bytes)) => self.on_signal(bytes),
            Internal::Punch { peer, addrs, round } => self.punch(&peer, &addrs, round),
        }
    }

    fn heartbeat(&mut self) {
        let now = crate::identity::now();
        // Presence and typing advance the ratchets without saving; persist them
        // regularly so a crash cannot leave us with keys the other side has
        // already seen used (which would break the sessions).
        if now % 30 < 10 {
            self.save();
        }
        self.net_heartbeat();
        self.mail_heartbeat(now);
        self.expire_disappearing(now);
        self.expire_typing();
        self.backup_heartbeat(now);
        let before = self.presence.len();
        self.presence.retain(|_, r| r.at.elapsed() < r.ttl);
        self.signal_heartbeat(now);
        self.call_heartbeat(now);
        if self.presence.len() != before {
            self.emit_contacts();
            self.emit_devices();
        }
    }

    fn on_command(&mut self, cmd: Command) -> Result<()> {
        if self.p.linking.is_some() && !matches!(cmd, Command::Shutdown | Command::NetInfo) {
            return Err(anyhow!("this device is waiting to be linked"));
        }
        match cmd {
            Command::SetStatus(s) => self.set_status(s),
            Command::SetAwayMessage(m) => {
                self.p.away_msg = m;
                self.settings_changed();
                self.broadcast_presence();
            }
            Command::SetAutoReply(b) => {
                self.p.auto_reply = b;
                self.settings_changed();
            }
            Command::SetReadReceipts(b) => {
                self.p.read_receipts = b;
                self.save();
                self.emit_settings();
            }
            Command::SetProfile(pr) => self.set_profile(pr),
            Command::SetBackground(b) => {
                self.background = b;
                self.broadcast_presence();
            }
            Command::SetNowPlaying(s) => {
                self.now_playing = s;
                self.broadcast_presence();
            }
            Command::SyncPluginState { plugin, state } => {
                let _ = self.send_body("self", Body::PluginState { plugin, state, updated: crate::identity::now() }, Some(0));
            }
            Command::NewInvite { uses, ttl_secs, label } => self.new_invite(uses, ttl_secs, label)?,
            Command::RevokeInvite { token } => {
                if let Some(i) = self.p.invites.iter_mut().find(|i| i.token == token) {
                    i.revoked = true;
                }
                self.save();
                self.emit_invites();
            }
            Command::AddContact { invite, text } => self.add_contact(&invite, &text)?,
            Command::Accept { id } => self.accept(&id)?,
            Command::Deny { id } => self.deny(&id),
            Command::Rename { id, name } => self.update_contact(&id, |c| c.petname = name)?,
            Command::Remove { id } => self.remove_contact(&id),
            Command::SetFolder { id, folder } => {
                if !folder.is_empty() && !self.p.folders.contains(&folder) {
                    self.p.folders.push(folder.clone());
                    self.emit(Event::Folders(self.p.folders.clone()));
                }
                self.update_contact(&id, |c| c.folder = folder)?
            }
            Command::SetFolders(f) => {
                self.p.folders = f;
                for c in &mut self.p.contacts {
                    if !c.folder.is_empty() && !self.p.folders.contains(&c.folder) {
                        c.folder.clear();
                    }
                }
                self.settings_changed();
                self.emit(Event::Folders(self.p.folders.clone()));
                self.emit_contacts();
            }
            Command::SetVisibility { id, visibility } => {
                self.update_contact(&id, |c| c.visibility = visibility)?;
                self.send_presence_to(&id);
            }
            Command::SetIgnored { id, ignored } => self.update_contact(&id, |c| c.ignored = ignored)?,
            Command::SetVerified { id, verified } => self.update_contact(&id, |c| c.verified = verified)?,
            Command::SafetyNumber { id } => {
                let c = self.contact(&id).ok_or_else(|| anyhow!("unknown contact"))?;
                let number = crate::identity::safety_number(&self.p.account_pk, &c.account_pk)?;
                self.emit(Event::SafetyNumber { id, number });
            }
            Command::Introduce { to, whom } => self.introduce(&to, &whom)?,
            Command::AcceptIntroduction { index, text, trust } => self.accept_introduction(index, &text, trust)?,
            Command::DismissIntroduction { index } => {
                if index < self.p.intros.len() {
                    self.p.intros.remove(index);
                }
                self.save();
                self.emit_intros();
            }
            Command::SetUrgentAllowed { id, allowed } => self.update_contact(&id, |c| c.urgent_allowed = allowed)?,
            Command::SetNotifyOnline { id, on } => self.update_contact(&id, |c| c.notify_online = on)?,
            Command::SetAutoAccept { id, on } => self.update_contact(&id, |c| c.auto_accept = on)?,
            Command::SetNick(n) => {
                let n = n.trim().to_string();
                if n.is_empty() || n.chars().count() > 40 {
                    anyhow::bail!("nickname must be 1-40 characters");
                }
                self.p.nick = n;
                self.settings_changed();
                self.emit_contacts();
            }
            Command::SetSendTyping(b) => {
                self.p.send_typing = b;
                self.settings_changed();
            }
            Command::SetPublicHelpers(b) => {
                self.p.net.public_helpers = b;
                self.save();
                if b {
                    self.dial_bootstrap();
                } else {
                    self.observed.clear();
                    for a in net::PUBLIC_HELPERS {
                        if let Some(pid) = a.parse::<libp2p::Multiaddr>().ok().and_then(|m| m.iter().find_map(|p| if let libp2p::multiaddr::Protocol::P2p(id) = p { Some(id) } else { None })) {
                            let _ = self.swarm.disconnect_peer_id(pid);
                        }
                    }
                }
                self.emit_settings();
            }
            Command::CallStart { id } => self.call_start(&id)?,
            Command::CallAccept => self.call_accept()?,
            Command::CallDecline => self.call_decline(),
            Command::CallHangup => self.call_hangup(),
            Command::ScreenShare(on) => self.screen_share(on)?,
            Command::ScreenWantKey => self.screen_want_key(),
            Command::SetBandwidth(k) => {
                self.p.net.bandwidth_kbps = k;
                self.save();
                self.emit_settings();
                self.notice("The bandwidth limit applies after a restart.");
            }
            Command::SendText { id, body, reply_to, urgent } => self.send_text(&id, body, reply_to, urgent)?,
            Command::EditText { id, msg, body } => self.edit_text(&id, msg, body)?,
            Command::DeleteText { id, msg } => self.delete_text(&id, msg)?,
            Command::Typing { id, typing } => self.send_typing(&id, typing),
            Command::SetDisappearing { id, ttl } => {
                self.update_contact(&id, |c| c.disappearing = ttl)?;
                self.notice(match ttl {
                    Some(s) => format!("Messages in this chat now disappear after {}.", human_secs(s)),
                    None => "Disappearing messages off.".into(),
                });
            }
            Command::OpenChat { id } => self.open_chat(&id),
            Command::CloseChat { id } => {
                self.open_chats.remove(&id);
            }
            Command::SendNote(text) => self.send_note(text),
            Command::OpenNotes => self.emit_notes(),
            Command::Search(q) => self.search(&q),
            Command::SendFile { id, path } => self.send_file(&id, &path)?,
            Command::AcceptFile { file, dir } => self.accept_file(&file, &dir)?,
            Command::DeclineFile { file } => self.decline_file(&file),
            Command::CancelFile { file } => self.cancel_file(&file),
            Command::PauseFile { file } => self.set_file_state(&file, FileState::Paused),
            Command::ResumeFile { file } => self.set_file_state(&file, FileState::Waiting),
            Command::CreateGroup { name, members } => self.create_group(&name, &members)?,
            Command::GroupAdd { group, id } => self.group_add(&group, &id)?,
            Command::GroupRemove { group, account } => self.group_remove(&group, &account)?,
            Command::GroupRename { group, name } => self.group_rename(&group, &name)?,
            Command::GroupLeave { group } => self.group_leave(&group)?,
            Command::GroupSend { group, text } => self.group_send(&group, &text)?,
            Command::OpenGroup { group } => self.open_group(&group),
            Command::CloseGroup { group } => {
                self.open_groups.remove(&group);
            }
            Command::LinkDevice { code, manager, history_days } => self.link_device(&code, manager, history_days)?,
            Command::RenameDevice { peer, name } => self.rename_device(&peer, &name)?,
            Command::RevokeDevice { peer, wipe } => self.revoke_device(&peer, wipe)?,
            Command::RemoteLock { peer } => self.remote(&peer, RemoteAction::Lock)?,
            Command::ExportBackup { path, include_files } => self.export_backup(&path, include_files)?,
            Command::ImportHistory { path, secret } => self.import_history(&path, secret)?,
            Command::SetBackupSchedule { dir, every_hours, keep } => {
                self.p.backup.dir = dir;
                self.p.backup.every_hours = every_hours;
                self.p.backup.keep = keep.max(1);
                self.save();
                self.emit_settings();
            }
            Command::ShowRecoveryKey => self.show_recovery_key()?,
            Command::SetRelays(r) => {
                self.p.net.relays = r;
                self.save();
                self.restart_mailbox();
                self.emit_settings();
            }
            Command::SetBootstrap(b) => {
                self.p.net.bootstrap = b;
                self.save();
                self.dial_bootstrap();
                self.emit_settings();
            }
            Command::SetLanOnly(b) => {
                self.p.net.lan_only = b;
                self.save();
                self.restart_mailbox();
                self.emit_settings();
            }
            Command::SetHelper(b) => {
                self.p.net.helper = b;
                self.save();
                self.broadcast_presence();
                self.emit_settings();
            }
            Command::NetInfo => self.emit_net(),
            Command::WipeAccount => self.wipe_local(),
            Command::Shutdown => {}
        }
        Ok(())
    }

    pub fn update_contact(&mut self, id: &str, f: impl FnOnce(&mut ContactRec)) -> Result<()> {
        let c = self.contact_mut(id).ok_or_else(|| anyhow!("unknown contact"))?;
        f(c);
        c.updated = crate::identity::now();
        let id = id.to_string();
        self.save();
        self.sync_contact(&id);
        self.emit_contacts();
        Ok(())
    }

    pub fn settings_changed(&mut self) {
        self.p.settings_updated = crate::identity::now();
        self.save();
        self.sync_settings();
        self.emit_settings();
    }
}

pub(crate) fn human_secs(s: u64) -> String {
    match s {
        s if s % 86_400 == 0 => format!("{} day(s)", s / 86_400),
        s if s % 3_600 == 0 => format!("{} hour(s)", s / 3_600),
        s if s % 60 == 0 => format!("{} minute(s)", s / 60),
        s => format!("{s} seconds"),
    }
}
