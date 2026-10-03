//! Persisted state (inside the encrypted store) and its records.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use vodozemac::megolm::{GroupSessionPickle, InboundGroupSessionPickle};
use vodozemac::olm::{AccountPickle, SessionPickle};

use crate::proto::*;

pub const STATE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct Persist {
    pub version: u32,
    pub nick: String,
    /// Account public key (base64 protobuf).
    pub account_pk: String,
    /// Account private key, only on manager devices.
    pub account_key: Option<String>,
    pub device_key: String,
    pub device_name: String,
    pub olm: AccountPickle,
    pub devices: Option<SignedDeviceList>,
    /// Own devices' seeds by peer id (including this device).
    pub own_seeds: HashMap<String, DeviceSeeds>,
    pub own_addrs: HashMap<String, Vec<String>>,
    pub status: Status,
    pub away_msg: String,
    pub auto_reply: bool,
    pub read_receipts: bool,
    pub profile: Profile,
    pub folders: Vec<String>,
    pub contacts: Vec<ContactRec>,
    pub pending: Vec<PendingRec>,
    pub intros: Vec<IntroRec>,
    pub invites: Vec<InviteRec>,
    pub groups: Vec<GroupRec>,
    pub notes: Vec<LineRec>,
    pub sessions: HashMap<String, Vec<SessionPickle>>,
    pub files: Vec<FileRec>,
    pub held: Vec<HeldRec>,
    /// Encrypted messages waiting for delivery, for any owner (contact id, "self", or group member account id).
    pub outbox: Vec<OutboxItem>,
    pub net: NetSettings,
    pub backup: BackupSettings,
    /// Recovery key entropy (hex), wraps backups together with the passphrase.
    pub recovery: Option<String>,
    pub settings_updated: i64,
    /// Waiting for a LinkGrant (a freshly started "link to existing account" device).
    pub linking: Option<LinkingState>,
    pub seen_mail: Vec<String>,
    pub lock_requested: bool,
    #[serde(default = "yes")]
    pub send_typing: bool,
    /// Remote command status per own device ("lock sent", "locked", ...).
    #[serde(default)]
    pub remote_status: HashMap<String, String>,
    #[serde(default)]
    pub own_last_seen: HashMap<String, i64>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LinkingState {
    pub code: String,
    pub started: i64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct NetSettings {
    pub relays: Vec<String>,
    pub bootstrap: Vec<String>,
    pub lan_only: bool,
    pub helper: bool,
    pub upnp: bool,
    pub bandwidth_kbps: u32,
    /// Ask public libp2p nodes for our outside address and relay help, so two
    /// peers behind routers can connect directly (hole punching).
    #[serde(default = "yes")]
    pub public_helpers: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct BackupSettings {
    pub dir: String,
    pub every_hours: u32,
    pub keep: u32,
    pub last: i64,
    pub include_files: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RemoteDevice {
    pub entry: DeviceEntry,
    pub seeds: Option<DeviceSeeds>,
    pub addrs: Vec<String>,
    pub last_seen: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ContactRec {
    pub id: String,
    pub petname: String,
    pub nick: String,
    pub account_pk: String,
    pub devices: Vec<RemoteDevice>,
    pub list_version: u64,
    /// Last verified, signed device list (re-shared in introductions, sync, groups).
    #[serde(default)]
    pub signed: Option<SignedDeviceList>,
    pub authorized: bool,
    pub awaiting: bool,
    pub verified: bool,
    pub folder: String,
    pub visibility: Visibility,
    pub ignored: bool,
    pub removed: bool,
    pub updated: i64,
    pub introduced_by: String,
    pub profile: Profile,
    pub history: Vec<LineRec>,
    pub unread: u32,
    pub last_os: String,
    pub last_device: DeviceClass,
    pub disappearing: Option<u64>,
    pub urgent_allowed: bool,
    pub urgent_log: Vec<i64>,
    pub last_auto_reply: i64,
    #[serde(default = "yes")]
    pub notify_online: bool,
    #[serde(default)]
    pub auto_accept: bool,
}

impl ContactRec {
    pub fn new(id: String, card: &Card, seeds: Vec<(String, DeviceSeeds)>) -> Self {
        let mut c = ContactRec {
            id,
            petname: card.nick.clone(),
            nick: card.nick.clone(),
            account_pk: card.devices.list.account_pk.clone(),
            devices: vec![],
            list_version: 0,
            signed: None,
            authorized: false,
            awaiting: false,
            verified: false,
            folder: String::new(),
            visibility: Visibility::Normal,
            ignored: false,
            removed: false,
            updated: crate::identity::now(),
            introduced_by: String::new(),
            profile: Profile::default(),
            history: vec![],
            unread: 0,
            last_os: String::new(),
            last_device: DeviceClass::Desktop,
            disappearing: None,
            urgent_allowed: true,
            urgent_log: vec![],
            last_auto_reply: 0,
            notify_online: true,
            auto_accept: false,
        };
        c.apply_devices(&card.devices);
        for (peer, s) in seeds {
            if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == peer) {
                d.seeds = Some(s);
            }
        }
        c
    }

    /// Replace the device set with a (verified) newer list, keeping what we
    /// know about devices that remain. Returns (added, removed) peer ids.
    pub fn apply_devices(&mut self, list: &SignedDeviceList) -> (Vec<String>, Vec<String>) {
        let old: Vec<String> = self.devices.iter().map(|d| d.entry.peer_id.clone()).collect();
        let mut next = vec![];
        for e in &list.list.devices {
            let prev = self.devices.iter().find(|d| d.entry.peer_id == e.peer_id);
            next.push(RemoteDevice {
                entry: e.clone(),
                seeds: prev.and_then(|p| p.seeds.clone()),
                addrs: prev.map(|p| p.addrs.clone()).unwrap_or_default(),
                last_seen: prev.map(|p| p.last_seen).unwrap_or(0),
            });
        }
        let new: Vec<String> = next.iter().map(|d| d.entry.peer_id.clone()).collect();
        self.devices = next;
        self.list_version = list.list.version;
        if !list.sig.is_empty() {
            self.signed = Some(list.clone());
        }
        (
            new.iter().filter(|p| !old.contains(p)).cloned().collect(),
            old.iter().filter(|p| !new.contains(p)).cloned().collect(),
        )
    }

    pub fn card(&self) -> Card {
        Card {
            nick: self.nick.clone(),
            devices: SignedDeviceList {
                list: DeviceList {
                    account_pk: self.account_pk.clone(),
                    version: self.list_version,
                    devices: self.devices.iter().map(|d| d.entry.clone()).collect(),
                },
                sig: String::new(),
            },
        }
    }

    pub fn set_addrs(&mut self, addrs: &[(String, Vec<String>)]) {
        for (peer, a) in addrs {
            if let Some(d) = self.devices.iter_mut().find(|d| &d.entry.peer_id == peer) {
                for x in a {
                    if !d.addrs.contains(x) {
                        d.addrs.push(x.clone());
                    }
                }
                d.addrs.truncate(16);
            }
        }
    }

    pub fn seeds(&self) -> Vec<(String, DeviceSeeds)> {
        self.devices.iter().filter_map(|d| d.seeds.clone().map(|s| (d.entry.peer_id.clone(), s))).collect()
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PendingRec {
    pub id: String,
    pub card: Card,
    pub seeds: Vec<(String, DeviceSeeds)>,
    pub text: String,
    pub introduced_by: String,
    pub ts: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct IntroRec {
    pub intro: Introduction,
    pub from: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct InviteRec {
    pub token: String,
    pub label: String,
    pub created: i64,
    pub uses_left: Option<u32>,
    pub expires: Option<i64>,
    pub revoked: bool,
    pub code: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LineRec {
    pub id: u64,
    pub ts: i64,
    pub from_me: bool,
    pub text: String,
    pub delivery: Delivery,
    #[serde(default)]
    pub edited: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub reply_to: Option<u64>,
    #[serde(default)]
    pub urgent: bool,
    #[serde(default)]
    pub expires: Option<i64>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
}

impl LineRec {
    pub fn text(id: u64, ts: i64, from_me: bool, text: String, delivery: Delivery) -> Self {
        LineRec { id, ts, from_me, text, delivery, edited: false, deleted: false, reply_to: None, urgent: false, expires: None, file: None, image: None }
    }
}

/// Encrypted message waiting for delivery to one device.
#[derive(Serialize, Deserialize, Clone)]
pub struct OutRec {
    /// Message id this belongs to (0 = control message).
    pub msg_id: u64,
    pub device: String,
    pub req: WireReq,
    pub created: i64,
    pub stored: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct OutboxItem {
    pub owner: String,
    pub rec: OutRec,
}

#[derive(Serialize, Deserialize)]
pub struct GroupRec {
    pub state: GroupState,
    pub outbound: Option<GroupSessionPickle>,
    pub inbound: HashMap<String, InboundGroupSessionPickle>,
    pub history: Vec<GroupLine>,
    pub unread: u32,
    pub left: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FileRec {
    pub offer: FileOffer,
    /// Contact id, or "self" for own devices.
    pub peer: String,
    pub outgoing: bool,
    /// Local path: source for outgoing, destination for incoming.
    pub path: String,
    pub done_chunks: Vec<u64>,
    pub state: FileState,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HeldRec {
    pub device: String,
    pub wire: WireReq,
    pub expires: i64,
}
