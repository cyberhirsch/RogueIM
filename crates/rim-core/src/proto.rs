//! Wire and view types. Everything inside [`Body`] travels Olm-encrypted.
//! See docs/protocol/PROTOCOL.md for the normative description.

use serde::{Deserialize, Serialize};
use vodozemac::olm::OlmMessage;

pub const PROTOCOL_VERSION: u32 = 2;

// ---------------------------------------------------------------- status

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
pub enum Status {
    #[default]
    Online,
    Away,
    NotAvailable,
    Occupied,
    DoNotDisturb,
    FreeForChat,
    Invisible,
    Offline,
}

impl Status {
    /// Statuses the user can pick, in menu order.
    pub const SELECTABLE: [Status; 7] = [
        Status::Online,
        Status::FreeForChat,
        Status::Away,
        Status::NotAvailable,
        Status::Occupied,
        Status::DoNotDisturb,
        Status::Invisible,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Status::Online => "Online",
            Status::Away => "Away",
            Status::NotAvailable => "N/A",
            Status::Occupied => "Occupied",
            Status::DoNotDisturb => "DND",
            Status::FreeForChat => "Free for Chat",
            Status::Invisible => "Invisible",
            Status::Offline => "Offline",
        }
    }

    /// One-character badge so status never depends on colour alone.
    pub fn badge(self) -> &'static str {
        match self {
            Status::Online => "+",
            Status::Away => "a",
            Status::NotAvailable => "n",
            Status::Occupied => "o",
            Status::DoNotDisturb => "x",
            Status::FreeForChat => "!",
            Status::Invisible => "i",
            Status::Offline => "-",
        }
    }

    /// Away-like statuses trigger the away message and auto-reply.
    pub fn is_away(self) -> bool {
        matches!(self, Status::Away | Status::NotAvailable)
    }

    /// Statuses that suppress sounds and pop-ups.
    pub fn is_busy(self) -> bool {
        matches!(self, Status::Occupied | Status::DoNotDisturb)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
pub enum DeviceClass {
    #[default]
    Desktop,
    Laptop,
    Mobile,
}

impl DeviceClass {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceClass::Desktop => "desktop",
            DeviceClass::Laptop => "laptop",
            DeviceClass::Mobile => "mobile",
        }
    }
}

// ---------------------------------------------------------------- identity

/// One device of an account, as listed (and signed) in the account's device list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceEntry {
    pub peer_id: String,
    /// Olm Curve25519 identity key (base64).
    pub curve: String,
    /// Olm fallback key (base64), lets anyone who knows the list open a session.
    pub fallback: String,
    pub name: String,
    pub os: String,
    pub class: DeviceClass,
    pub added: i64,
    pub manager: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceList {
    /// Account public key (libp2p protobuf encoding, base64).
    pub account_pk: String,
    pub version: u64,
    pub devices: Vec<DeviceEntry>,
}

/// A device list signed by the account key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignedDeviceList {
    pub list: DeviceList,
    pub sig: String,
}

/// Public contact card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    /// Self-chosen nickname; only a suggestion for the local petname.
    pub nick: String,
    pub devices: SignedDeviceList,
}

/// Per-device secrets shared with authorized contacts (and own devices):
/// they locate the device's Nostr mailbox and rendezvous record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeviceSeeds {
    pub inbox: String,
    pub rdv: String,
}

/// Invite link payload (`rim2:` + base64url JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub v: u8,
    pub card: Card,
    /// Device the invite points at (peer id) and a one-time key reserved for it
    /// (single-use invites) or empty (multi-use: the fallback key is used).
    pub device: String,
    pub otk: String,
    pub token: String,
    /// Seeds of the issuing device, so a request can wait in its mailbox.
    pub seeds: DeviceSeeds,
    pub addrs: Vec<String>,
    /// Proof-of-work difficulty (leading zero bits) the request must meet.
    pub pow_bits: u8,
    pub expires: Option<i64>,
}

// ---------------------------------------------------------------- wire

/// Request sent over `/rim/msg/2` (and posted to mailboxes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireReq {
    /// Sender device's Olm Curve25519 identity key (base64).
    pub sender_curve: String,
    pub msg: OlmMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResp {
    pub ok: bool,
}

/// File chunk protocol `/rim/file/1` (direct connections only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkReq {
    pub file_id: String,
    pub index: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkResp {
    pub file_id: String,
    pub index: u64,
    /// XChaCha20-Poly1305 ciphertext of the chunk (base64), empty on refusal.
    pub data: String,
    pub error: Option<String>,
}

/// Plaintext inside an Olm message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Sender account key and device (checked against the device list).
    pub account: String,
    pub device: String,
    pub body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Body {
    // --- authorization
    AuthRequest { token: String, card: Card, seeds: DeviceSeeds, text: String, pow: u64, intro: Option<Introduction> },
    AuthAccept { card: Card, seeds: DeviceSeeds },
    AuthDeny,
    /// Seeds of the sending device (after authorization or when they change).
    Seeds(DeviceSeeds),
    DeviceList(SignedDeviceList),
    Profile(Profile),

    // --- messaging
    Text(TextMsg),
    Edit { id: u64, text: String },
    Delete { id: u64 },
    Typing(bool),
    /// Delivery receipt for messages that went through a mailbox or relay.
    Receipt { ids: Vec<u64> },
    /// Read marker: everything up to this timestamp has been seen.
    Read { up_to: i64 },
    Presence(Presence),
    Introduce(Introduction),

    // --- files
    FileOffer(FileOffer),
    FileAccept { id: String },
    FileDecline { id: String },
    FileCancel { id: String },

    // --- groups
    GroupInvite(GroupState),
    GroupUpdate(GroupState),
    GroupKey { group: String, key: String },
    GroupMsg { group: String, session: String, ciphertext: String },
    GroupLeave { group: String },
    GroupSyncReq { group: String, since: i64 },
    GroupSync { group: String, lines: Vec<GroupLine> },

    // --- own devices only
    SelfCopy { contact: String, msg: TextMsg },
    SelfNote(TextMsg),
    SyncContacts(Vec<ContactSync>),
    SyncSettings(SettingsSync),
    SyncRead { contact: String, up_to: i64 },
    LinkGrant(Box<LinkGrant>),
    History { contact: String, lines: Vec<HistoryLine> },
    Remote(RemoteCommand),
    /// The target device confirms it carried out a remote command.
    RemoteAck { action: RemoteAction, ts: i64 },
    Call(CallSignal),
    /// Plugin state shared between own devices (e.g. the pomodoro timer).
    PluginState { plugin: String, state: String, updated: i64 },

    // --- store and forward
    Hold { device: String, wire: WireReq, expires: i64 },
    Fetch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMsg {
    pub id: u64,
    pub ts: i64,
    pub text: String,
    pub reply_to: Option<u64>,
    pub urgent: bool,
    /// Disappearing messages: seconds after reading/sending.
    pub ttl: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Presence {
    pub status: Status,
    pub away_msg: String,
    pub os: String,
    pub device_class: DeviceClass,
    pub on_battery: bool,
    /// The device's app is in the background (mobile), replies may be late.
    pub background: bool,
    /// This device stores messages for others (help-the-network).
    pub helper: bool,
    pub bot: bool,
    pub addrs: Vec<String>,
    /// "Artist – Title" when the user shares what they listen to (PRD GD-10).
    #[serde(default)]
    pub now_playing: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Profile {
    pub about: String,
    pub location: String,
    pub homepage: String,
    pub interests: String,
    pub updated: i64,
}

/// "A introduces B": a signed statement by the introducer's account key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Introduction {
    pub card: Card,
    pub seeds: Vec<(String, DeviceSeeds)>,
    pub introducer: String,
    /// Device of the introducer that signed this (must be in its device list).
    pub introducer_device: String,
    pub introducer_nick: String,
    /// Account key of the person this introduction is given to.
    pub to: String,
    pub verified: bool,
    pub ts: i64,
    pub sig: String,
    /// Last known addresses (hints, not signed).
    #[serde(default)]
    pub addrs: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOffer {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub chunk: u64,
    /// BLAKE3 of the plaintext (hex).
    pub hash: String,
    /// Per-file XChaCha20-Poly1305 key (base64).
    pub key: String,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupMember {
    pub account: String,
    pub nick: String,
    pub devices: SignedDeviceList,
    pub seeds: Vec<(String, DeviceSeeds)>,
    /// Last known addresses per device (peer id, addrs), to reach non-contacts.
    #[serde(default)]
    pub addrs: Vec<(String, Vec<String>)>,
    pub admin: bool,
}

/// Group state, signed by the admin who made the last change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupState {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub members: Vec<GroupMember>,
    /// Admin account and device that signed this version.
    pub by: String,
    pub by_device: String,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupLine {
    pub id: u64,
    pub ts: i64,
    pub from: String,
    pub nick: String,
    pub text: String,
}

/// Contact entry shared between own devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactSync {
    pub id: String,
    pub petname: String,
    pub folder: String,
    pub verified: bool,
    pub visibility: Visibility,
    pub ignored: bool,
    pub removed: bool,
    pub card: Card,
    pub seeds: Vec<(String, DeviceSeeds)>,
    pub authorized: bool,
    pub updated: i64,
    /// Last known addresses per device (hints).
    #[serde(default)]
    pub addrs: Vec<(String, Vec<String>)>,
    #[serde(default)]
    pub notify_online: Option<bool>,
    #[serde(default)]
    pub auto_accept: Option<bool>,
    #[serde(default)]
    pub urgent_allowed: Option<bool>,
}

/// Voice call signalling (inside the encrypted channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CallSignal {
    /// Ring: `key` (base64, 32 bytes) seals the audio of this call.
    Invite { call: String, key: String },
    Accept { call: String },
    Decline { call: String },
    Busy { call: String },
    Hangup { call: String },
    /// To our own devices: this call was answered or declined elsewhere.
    Answered { call: String },
}

/// One sealed audio frame of a call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceReq {
    pub call: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceResp {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// We ring them.
    Calling,
    /// They ring us.
    Ringing,
    Active,
    Ended,
}

#[derive(Debug, Clone)]
pub struct CallView {
    pub call: String,
    pub contact: String,
    pub name: String,
    pub outgoing: bool,
    pub state: CallState,
    /// Why it ended ("declined", "no answer", ...).
    pub reason: String,
    /// Ringing since / active since (unix seconds).
    pub since: i64,
    /// Audio flows over a direct connection (not a relay).
    pub direct: bool,
}

/// Short-lived, signed messages to one device via Nostr, for when no direct
/// connection exists: presence, and coordination of a NAT hole punch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignalKind {
    Presence(Presence),
    /// "Dial me at these addresses now." `reply` = answer to a punch request.
    Punch { addrs: Vec<String>, reply: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// Sending device (peer id; its key signs the signal).
    pub from: String,
    /// Receiving device.
    pub to: String,
    pub ts: i64,
    pub kind: SignalKind,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsSync {
    pub status: Option<Status>,
    pub away_msg: Option<String>,
    pub auto_reply: Option<bool>,
    pub profile: Option<Profile>,
    pub folders: Option<Vec<String>>,
    pub updated: i64,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub send_typing: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryLine {
    pub id: u64,
    pub ts: i64,
    pub from_me: bool,
    pub text: String,
}

/// Everything a newly linked device needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkGrant {
    pub nick: String,
    pub devices: SignedDeviceList,
    /// Present only when the new device is made a manager.
    pub account_key: Option<String>,
    pub contacts: Vec<ContactSync>,
    pub own_seeds: Vec<(String, DeviceSeeds)>,
    pub settings: SettingsSync,
    pub groups: Vec<GroupState>,
}

/// Commands between own devices, signed by the account key where needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteCommand {
    pub target: String,
    pub action: RemoteAction,
    pub ts: i64,
    pub sig: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoteAction {
    Lock,
    Wipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Visibility {
    /// Sees presence as usual.
    #[default]
    Normal,
    /// Always sees us online, even when Invisible.
    Visible,
    /// Never sees us online.
    Invisible,
}

// ---------------------------------------------------------------- views

#[derive(Debug, Clone)]
pub struct ContactView {
    pub id: String,
    pub name: String,
    pub status: Status,
    pub os: String,
    pub device_class: DeviceClass,
    pub on_battery: bool,
    pub background: bool,
    pub away_msg: String,
    /// Waiting for the other side to accept our authorization request.
    pub awaiting: bool,
    pub unread: u32,
    pub fingerprint: String,
    pub verified: bool,
    pub folder: String,
    pub visibility: Visibility,
    pub ignored: bool,
    pub typing: bool,
    pub bot: bool,
    pub devices: Vec<DeviceView>,
    pub introduced_by: String,
    pub profile: Profile,
    pub now_playing: String,
    pub disappearing: Option<u64>,
    pub urgent_allowed: bool,
    pub notify_online: bool,
    pub auto_accept: bool,
}

#[derive(Debug, Clone)]
pub struct DeviceView {
    pub peer_id: String,
    pub name: String,
    pub os: String,
    pub class: DeviceClass,
    pub status: Status,
    pub manager: bool,
    pub this_device: bool,
    pub last_seen: i64,
    /// Last remote command sent to this device and whether it was carried out.
    pub remote: String,
}

#[derive(Debug, Clone)]
pub struct PendingView {
    pub id: String,
    pub nick: String,
    pub fingerprint: String,
    pub text: String,
    pub introduced_by: String,
    /// Set when the nickname matches an existing contact with a different key.
    pub warning: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    Queued,
    /// Waiting in a mailbox or with a relay.
    Stored,
    Delivered,
    /// The other side opened the chat (read receipts on).
    Read,
    Failed,
    Received,
}

#[derive(Debug, Clone)]
pub struct LineView {
    pub id: u64,
    pub ts: i64,
    pub from_me: bool,
    pub who: String,
    pub text: String,
    pub delivery: Delivery,
    pub edited: bool,
    pub deleted: bool,
    pub reply_to: Option<(u64, String)>,
    pub urgent: bool,
    pub expires: Option<i64>,
    pub file: Option<FileView>,
}

#[derive(Debug, Clone)]
pub struct FileView {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub done: u64,
    pub state: FileState,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileState {
    Offered,
    Waiting,
    Transferring,
    Paused,
    Done,
    Declined,
    Failed,
    NoDirect,
}

#[derive(Debug, Clone)]
pub struct InviteView {
    pub token: String,
    pub label: String,
    pub uses_left: Option<u32>,
    pub expires: Option<i64>,
    pub code: String,
}

#[derive(Debug, Clone)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub members: Vec<(String, String, bool)>,
    pub admin: bool,
    pub unread: u32,
}

#[derive(Debug, Clone, Default)]
pub struct NetView {
    pub peer_id: String,
    pub listen: Vec<String>,
    pub external: Vec<String>,
    pub nat: String,
    pub connected_peers: usize,
    pub relays: Vec<(String, bool)>,
    pub lan_only: bool,
    pub helper: bool,
    pub held: usize,
    pub mailbox_last_fetch: i64,
    /// Connected peers, named where they are contacts or own devices.
    pub peers: Vec<String>,
    /// Relay circuits we can be reached through.
    pub circuits: Vec<String>,
    /// Our address as public nodes see it (what hole punching aims at).
    pub observed: Vec<String>,
    pub bandwidth_kbps: u32,
}
