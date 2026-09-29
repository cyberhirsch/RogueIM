//! Wire and view types. Everything inside [`Inner`] travels Olm-encrypted.

use serde::{Deserialize, Serialize};
use vodozemac::olm::OlmMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// Public contact card: who someone is and which device speaks for them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    /// Self-chosen nickname; only a suggestion for the local petname.
    pub nick: String,
    /// Account public key (libp2p protobuf encoding, base64).
    pub account_pk: String,
    /// Device PeerId (string form).
    pub peer_id: String,
    /// Device Olm Curve25519 identity key (base64).
    pub curve: String,
    /// Account-key signature over `peer_id || curve` (base64).
    pub device_sig: String,
}

/// Invite link payload (`rim1:` + base64url JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub v: u8,
    pub card: Card,
    /// Single-use Olm one-time key reserved for this invite.
    pub otk: String,
    /// Single-use token; a request without an unspent token is dropped.
    pub token: String,
    /// Addresses the issuer was listening on when the invite was made.
    pub addrs: Vec<String>,
}

/// Request sent over `/rim/msg/1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireReq {
    /// Sender's Olm Curve25519 identity key (base64).
    pub sender_curve: String,
    pub msg: OlmMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResp {
    pub ok: bool,
}

/// Plaintext inside an Olm message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Inner {
    AuthRequest { token: String, card: Card, text: String, addrs: Vec<String> },
    AuthAccept { card: Card, addrs: Vec<String> },
    AuthDeny,
    Text { id: u64, ts: i64, body: String },
    Presence(Presence),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub status: Status,
    pub away_msg: String,
    pub os: String,
    pub device_class: DeviceClass,
    pub on_battery: bool,
    /// Our current listen addresses, so contacts can redial us.
    pub addrs: Vec<String>,
}

// ---- views handed to the UI ----

#[derive(Debug, Clone)]
pub struct ContactView {
    pub id: String,
    pub name: String,
    pub status: Status,
    pub os: String,
    pub device_class: DeviceClass,
    pub on_battery: bool,
    pub away_msg: String,
    /// Waiting for the other side to accept our authorization request.
    pub awaiting: bool,
    pub unread: u32,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct PendingView {
    pub id: String,
    pub nick: String,
    pub fingerprint: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    Queued,
    Delivered,
    Failed,
    Received,
}

#[derive(Debug, Clone)]
pub struct LineView {
    pub ts: i64,
    pub from_me: bool,
    pub text: String,
    pub delivery: Delivery,
}
