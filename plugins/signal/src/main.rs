//! Signal plugin (PRD HB-5): your Signal chats in RogueIM's buddy list.
//! RogueIM becomes a linked device of your phone's Signal account, like Signal
//! Desktop, through the unofficial library presage. Level C: Signal encrypts
//! end to end, this computer decrypts. presage keeps keys, contacts, groups
//! and messages in one SQLite file in the plugin's folder; the plugin keeps
//! only its chat list (signal-chats.json) next to it.
//!
//! Everything runs on one thread: presage's store must not cross threads.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{channel::oneshot, Future, Stream, StreamExt};
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::proto::sync_message::{self, Content as SyncContent};
use presage::proto::{AttachmentPointer, EditMessage, SyncMessage};
use presage::store::{ContentExt, ContentsStore, StateStore, Thread};
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use rim_plugin_sdk::{sealed, Item, NetAction, NetContact, NetLine, Plugin, ToHost, ToPlugin};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const WARNING: &str = "Signal encrypts end to end; RogueIM decrypts on this computer as a linked device, through the unofficial library presage.";
const LIST_DAYS: u64 = 60;
const CHAT_LINES: usize = 60;
/// Pictures up to this size are fetched as they are shown.
const AUTO_IMAGE: u32 = 8 << 20;
/// AttachmentPointer.Flags.VOICE_MESSAGE
const VOICE: u32 = 1;

type Mgr = Manager<SqliteStore, Registered>;
type Linking = Pin<Box<dyn Future<Output = Result<Mgr, String>>>>;
type Messages = Pin<Box<dyn Stream<Item = Received>>>;

/// A chat in the buddy list.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Chat {
    id: String,
    /// A contact's service ID, or a group's master key (hex).
    target: String,
    group: bool,
    name: String,
    info: String,
    /// Newest message, milliseconds.
    last: u64,
    /// Incoming messages up to here are read.
    read: u64,
    unread: u32,
    hidden: bool,
}

impl Chat {
    fn thread(&self) -> Option<Thread> {
        if self.group {
            let key: [u8; 32] = hex::decode(&self.target).ok()?.try_into().ok()?;
            Some(Thread::Group(key))
        } else {
            ServiceId::parse_from_service_id_string(&self.target).map(Thread::Contact)
        }
    }
}

fn chat_id(thread: &Thread) -> String {
    match thread {
        Thread::Contact(s) => format!("u:{}", s.service_id_string()),
        // Not the key itself: chat IDs travel to RogueIM.
        Thread::Group(key) => format!("g:{}", &hex::encode(Sha256::digest(key))[..16]),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn data_dir() -> PathBuf {
    std::env::var_os("RIM_PLUGIN_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

struct App {
    host: Plugin,
    dir: PathBuf,
    store: Option<SqliteStore>,
    manager: Option<Mgr>,
    me: String,
    chats: HashMap<String, Chat>,
    open: HashSet<String>,
    /// Lines being sent or that failed, per chat.
    pending: HashMap<String, Vec<NetLine>>,
    link_url: String,
    status: String,
    synced: bool,
    want_contacts: bool,
}

fn main() {
    // presage's futures are large; Windows gives the main thread 1 MB.
    let t = std::thread::Builder::new().stack_size(64 << 20).spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(start());
    });
    let _ = t.expect("thread").join();
}

async fn start() {
    let Ok(host) = Plugin::connect("signal") else {
        eprintln!("rim-plugin-signal is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    let dir = data_dir();
    let _ = std::fs::create_dir_all(dir.join("media"));
    sealed::clear_open_copies();
    let chats: Vec<Chat> = sealed::read(&dir.join("signal-chats.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut app = App {
        host,
        dir,
        store: None,
        manager: None,
        me: String::new(),
        chats: chats.into_iter().map(|c| (c.id.clone(), c)).collect(),
        open: HashSet::new(),
        pending: HashMap::new(),
        link_url: String::new(),
        status: String::new(),
        synced: false,
        want_contacts: false,
    };
    app.host.send(&ToHost::Channel {
        label: "signal".into(),
        level: "C".into(),
        warning: WARNING.into(),
        add_hint: "a name or number from your Signal contacts".into(),
        files: true,
    });
    tokio::task::LocalSet::new()
        .run_until(Box::pin(app.run()))
        .await;
}

impl App {
    async fn run(&mut self) {
        let mut linking: Option<Linking> = None;
        let mut link_rx: Option<oneshot::Receiver<url::Url>> = None;
        let mut messages: Option<Messages> = None;
        let mut retry: Option<Pin<Box<tokio::time::Sleep>>> = None;
        let mut wait = Duration::from_secs(5);
        let mut tick = tokio::time::interval(Duration::from_millis(50));

        match self.open_store().await {
            Ok(store) if store.is_registered().await => match Manager::load_registered(store).await
            {
                Ok(m) => {
                    self.registered(m);
                    retry = Some(Box::pin(tokio::time::sleep(Duration::ZERO)));
                }
                Err(e) => self.status = format!("could not load the link: {e}"),
            },
            Ok(_) => {}
            Err(e) => self.status = format!("database: {e}"),
        }
        self.contacts();
        self.view();

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    while let Some(m) = self.host.recv(Duration::ZERO) {
                        match m {
                            ToPlugin::Stop => return,
                            ToPlugin::Click { id } if id == "link" && linking.is_none() && self.manager.is_none() => {
                                match self.open_store().await {
                                    Ok(store) => {
                                        let (tx, rx) = oneshot::channel();
                                        link_rx = Some(rx);
                                        self.status = "asking Signal for a code…".into();
                                        self.host.network("chat.signal.org", "Signal (unofficial client, presage)");
                                        linking = Some(Box::pin(async move {
                                            Manager::link_secondary_device(store, SignalServers::Production, "RogueIM".into(), tx).await.map_err(|e| e.to_string())
                                        }));
                                    }
                                    Err(e) => self.status = format!("database: {e}"),
                                }
                                self.view();
                            }
                            ToPlugin::Click { id } if id == "cancel" => {
                                linking = None;
                                link_rx = None;
                                self.link_url.clear();
                                self.status.clear();
                                self.view();
                            }
                            ToPlugin::Click { id } if id == "unlink" => {
                                messages = None;
                                retry = None;
                                self.unlink().await;
                            }
                            m => self.handle(m).await,
                        }
                    }
                }
                url = async { link_rx.as_mut().unwrap().await }, if link_rx.is_some() => {
                    link_rx = None;
                    match url {
                        Ok(url) => { self.link_url = url.to_string(); self.status.clear(); }
                        Err(_) => self.status = "Signal sent no code".into(),
                    }
                    self.view();
                }
                done = async { linking.as_mut().unwrap().await }, if linking.is_some() => {
                    linking = None;
                    self.link_url.clear();
                    match done {
                        Ok(m) => {
                            self.status = "linked; waiting for your contacts…".into();
                            self.want_contacts = true;
                            self.registered(m);
                            retry = Some(Box::pin(tokio::time::sleep(Duration::ZERO)));
                        }
                        Err(e) => self.status = format!("linking failed: {e}"),
                    }
                    self.view();
                }
                _ = async { retry.as_mut().unwrap().await }, if retry.is_some() => {
                    retry = None;
                    let Some(m) = self.manager.as_mut() else { continue };
                    match m.receive_messages().await {
                        Ok(s) => {
                            messages = Some(Box::pin(s));
                            wait = Duration::from_secs(5);
                        }
                        Err(e) => {
                            self.status = format!("offline: {e}");
                            retry = Some(Box::pin(tokio::time::sleep(wait)));
                            wait = (wait * 2).min(Duration::from_secs(300));
                        }
                    }
                    self.view();
                }
                item = async { messages.as_mut().unwrap().next().await }, if messages.is_some() => {
                    match item {
                        Some(r) => self.received(r).await,
                        None => {
                            messages = None;
                            self.synced = false;
                            self.status = "disconnected, reconnecting…".into();
                            retry = Some(Box::pin(tokio::time::sleep(wait)));
                            self.view();
                        }
                    }
                }
            }
        }
    }

    async fn open_store(&mut self) -> Result<SqliteStore, String> {
        if let Some(s) = &self.store {
            return Ok(s.clone());
        }
        let path = self.dir.join("signal.db");
        let key = sealed::key().map(hex::encode);
        if let Some(k) = &key {
            encrypt_plain_db(&path, k).await?;
        }
        let mut opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        // SQLCipher with this plugin's key as the raw key (no passphrase stretching needed).
        if let Some(k) = &key {
            opts = opts.pragma("key", format!("\"x'{k}'\""));
        }
        let store = SqliteStore::open_with_options(opts, OnNewIdentity::Trust)
            .await
            .map_err(|e| e.to_string())?;
        self.store = Some(store.clone());
        Ok(store)
    }

    fn registered(&mut self, m: Mgr) {
        let data = m.registration_data();
        self.me = data.phone_number.to_string();
        self.manager = Some(m);
        self.host
            .network("chat.signal.org", "Signal (unofficial client, presage)");
    }

    fn my_aci(&self) -> Option<uuid::Uuid> {
        self.manager
            .as_ref()
            .map(|m| m.registration_data().service_ids.aci)
    }

    fn save_chats(&self) {
        let list: Vec<&Chat> = self.chats.values().collect();
        let path = self.dir.join("signal-chats.json");
        let _ = sealed::write(&path, &serde_json::to_vec_pretty(&list).unwrap_or_default());
    }

    fn view(&mut self) {
        let mut items = vec![];
        if self.manager.is_some() {
            items.push(Item::text(&self.me, 0));
            let unread: u32 = self
                .chats
                .values()
                .filter(|c| !c.hidden)
                .map(|c| c.unread)
                .sum();
            let n = self.listed().len();
            let state = if self.synced {
                "connected"
            } else {
                "connecting"
            };
            items.push(Item::dim(
                &format!("{n} chats · {unread} unread · {state}"),
                1,
            ));
            items.push(Item::button("unlink", "unlink", 2));
        } else if !self.link_url.is_empty() {
            items.push(Item::qr(&self.link_url, 0));
            items.push(Item::dim(
                "On your phone: Signal → Settings → Linked devices → Link new device, then scan.",
                1,
            ));
            items.push(Item::button("cancel", "cancel", 2));
        } else {
            items.push(Item::text("Link Signal", 0));
            items.push(Item::dim("RogueIM becomes a linked device of your Signal account, like Signal Desktop. It uses presage, an unofficial library; Signal does not support it, but tolerates such clients.", 1));
            items.push(Item::dim(
                "Older messages stay on your phone: RogueIM sees what arrives from now on.",
                2,
            ));
            items.push(Item::button("link", "link", 3));
        }
        if !self.status.is_empty() {
            items.push(Item::dim(&self.status, 5));
        }
        items.push(Item::dim(
            "End to end encrypted · decrypted on this computer.",
            9,
        ));
        self.host.view(items);
    }

    fn listed(&self) -> Vec<&Chat> {
        let since = now_ms().saturating_sub(LIST_DAYS * 86_400_000);
        self.chats
            .values()
            .filter(|c| !c.hidden && (c.last >= since || c.last == 0))
            .collect()
    }

    fn contacts(&mut self) {
        let mut list: Vec<NetContact> = self
            .listed()
            .into_iter()
            .map(|c| NetContact {
                id: c.id.clone(),
                name: c.name.clone(),
                status: String::new(),
                info: c.info.clone(),
                unread: c.unread,
            })
            .collect();
        list.sort_by_key(|c| c.name.to_lowercase());
        self.host.send(&ToHost::Contacts { list });
    }

    async fn unlink(&mut self) {
        if let Some(m) = self.manager.take() {
            if let Err(e) = m.unlink_self().await {
                self.status = format!("could not unlink on Signal ({e}); remove RogueIM on your phone under Linked devices");
            } else {
                self.status = "unlinked".into();
            }
        }
        if let Some(store) = self.store.as_mut() {
            let _ = store.clear_registration().await;
            let _ = store.clear_contents().await;
        }
        self.chats.clear();
        self.pending.clear();
        self.save_chats();
        self.synced = false;
        self.contacts();
        self.view();
    }

    // ------------------------------------------------------------ from RogueIM

    async fn handle(&mut self, m: ToPlugin) {
        match m {
            ToPlugin::Open { chat } => {
                self.open.insert(chat.clone());
                self.mark_read(&chat).await;
                self.refresh(&chat).await;
            }
            ToPlugin::Close { chat } => {
                self.open.remove(&chat);
            }
            ToPlugin::Send { chat, text } => self.send_text(&chat, text).await,
            ToPlugin::SendFile { chat, path } => self.send_file(&chat, &path).await,
            ToPlugin::Action { chat, line, id } => self.action(&chat, &line, &id).await,
            ToPlugin::Add { address } => self.add(&address).await,
            ToPlugin::Remove { chat } => {
                if let Some(c) = self.chats.get_mut(&chat) {
                    c.hidden = true;
                }
                self.save_chats();
                self.contacts();
            }
            _ => {}
        }
    }

    /// Adds a contact or group from the synced address book by name or number.
    async fn add(&mut self, address: &str) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let q = address.trim().to_lowercase();
        let digits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
        let mut found: Vec<(Thread, String)> = vec![];
        if let Ok(contacts) = store.contacts().await {
            for c in contacts.flatten() {
                let phone = c
                    .phone_number
                    .as_ref()
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let phone_digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
                if (!c.name.is_empty() && c.name.to_lowercase().contains(&q))
                    || (digits.len() >= 6 && phone_digits.ends_with(&digits))
                {
                    found.push((
                        Thread::Contact(ServiceId::Aci(c.uuid.into())),
                        c.name.clone(),
                    ));
                }
            }
        }
        if let Ok(groups) = store.groups().await {
            for (key, g) in groups.flatten() {
                if g.title.to_lowercase().contains(&q) {
                    found.push((Thread::Group(key), g.title));
                }
            }
        }
        match found.len() {
            0 => self.status = format!("no Signal contact or group matches “{}”", address.trim()),
            1 => {
                let (thread, _) = found.remove(0);
                let id = self.touch(&thread, 0).await;
                if let Some(c) = self.chats.get_mut(&id) {
                    c.hidden = false;
                }
                self.status.clear();
                self.save_chats();
                self.contacts();
            }
            n => {
                let names: Vec<String> = found.iter().take(5).map(|(_, n)| n.clone()).collect();
                self.status = format!("{n} match: {} – be more precise", names.join(", "));
            }
        }
        self.view();
    }

    // ------------------------------------------------------------ from Signal

    async fn received(&mut self, r: Received) {
        match r {
            Received::QueueEmpty => {
                if !self.synced {
                    self.synced = true;
                    if self.status.starts_with("offline") || self.status.starts_with("disconnected")
                    {
                        self.status.clear();
                    }
                    if self.want_contacts {
                        self.want_contacts = false;
                        if let Some(m) = self.manager.as_mut() {
                            let _ = m.request_contacts().await;
                        }
                    }
                    self.view();
                }
            }
            Received::Contacts => {
                if self.status.starts_with("linked") {
                    self.status.clear();
                }
                self.rename_all().await;
                self.contacts();
                self.view();
            }
            Received::DecryptionError(_) => {}
            Received::Content(c) => self.content(*c).await,
        }
    }

    async fn content(&mut self, c: Content) {
        // Read on another device.
        if let ContentBody::SynchronizeMessage(SyncMessage { read, .. }) = &c.body {
            if !read.is_empty() {
                self.read_elsewhere(read).await;
            }
        }
        let Some(data) = shown(&c) else {
            if let (
                ContentBody::EditMessage(_)
                | ContentBody::DataMessage(_)
                | ContentBody::SynchronizeMessage(_),
                Ok(thread),
            ) = (&c.body, Thread::try_from(&c))
            {
                let id = chat_id(&thread);
                if self.open.contains(&id) {
                    self.refresh(&id).await;
                }
            }
            return;
        };
        let Ok(thread) = Thread::try_from(&c) else {
            return;
        };
        let ts = data.timestamp.unwrap_or_else(|| c.timestamp());
        let mine = self.is_mine(&c);
        let id = self.touch(&thread, ts).await;
        let open = self.open.contains(&id);
        if let Some(chat) = self.chats.get_mut(&id) {
            chat.hidden = false;
            if !mine && !open {
                chat.unread += 1;
            }
            if mine || open {
                chat.read = chat.read.max(ts);
            }
        }
        self.save_chats();
        self.contacts();
        if open {
            self.refresh(&id).await;
            if !mine {
                self.mark_read(&id).await;
            }
        }
        if !mine {
            let who = self.name_of(&c.metadata.sender).await;
            let name = match &thread {
                Thread::Group(_) => format!(
                    "{who} in {}",
                    self.chats
                        .get(&id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default()
                ),
                Thread::Contact(_) => who,
            };
            self.host.send(&ToHost::Incoming { chat: id, name });
        }
        self.view();
    }

    fn is_mine(&self, c: &Content) -> bool {
        matches!(
            &c.body,
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(SyncContent::Sent(_)),
                ..
            })
        ) || Some(c.metadata.sender.raw_uuid()) == self.my_aci()
    }

    /// The chat of a thread, created if new, with a fresh name; returns its ID.
    async fn touch(&mut self, thread: &Thread, ts: u64) -> String {
        let id = chat_id(thread);
        let (target, group) = match thread {
            Thread::Contact(s) => (s.service_id_string(), false),
            Thread::Group(key) => (hex::encode(key), true),
        };
        let (name, info) = self.describe(thread).await;
        let chat = self.chats.entry(id.clone()).or_insert_with(|| Chat {
            id: id.clone(),
            target,
            group,
            ..Default::default()
        });
        if !name.is_empty() {
            chat.name = name;
        }
        chat.info = info;
        chat.last = chat.last.max(ts);
        id
    }

    async fn rename_all(&mut self) {
        let threads: Vec<Thread> = self.chats.values().filter_map(|c| c.thread()).collect();
        for t in threads {
            let (name, info) = self.describe(&t).await;
            if let Some(c) = self.chats.get_mut(&chat_id(&t)) {
                if !name.is_empty() {
                    c.name = name;
                }
                c.info = info;
            }
        }
        self.save_chats();
    }

    async fn describe(&mut self, thread: &Thread) -> (String, String) {
        match thread {
            Thread::Contact(s) => {
                if Some(s.raw_uuid()) == self.my_aci() {
                    return ("Note to Self".into(), "notes to yourself".into());
                }
                let name = self.name_of(s).await;
                let phone = self.phone_of(s).await;
                (name, phone)
            }
            Thread::Group(key) => {
                let title = match &self.store {
                    Some(st) => st
                        .group(*key)
                        .await
                        .ok()
                        .flatten()
                        .map(|g| g.title)
                        .unwrap_or_default(),
                    None => String::new(),
                };
                (
                    if title.is_empty() {
                        "Signal group".into()
                    } else {
                        title
                    },
                    "group".into(),
                )
            }
        }
    }

    async fn phone_of(&self, s: &ServiceId) -> String {
        let Some(st) = &self.store else {
            return String::new();
        };
        st.contact_by_id(s)
            .await
            .ok()
            .flatten()
            .and_then(|c| c.phone_number)
            .map(|p| p.to_string())
            .unwrap_or_default()
    }

    /// A person's name: from the phone's address book, else their Signal profile, else their number.
    async fn name_of(&mut self, s: &ServiceId) -> String {
        if Some(s.raw_uuid()) == self.my_aci() {
            return "me".into();
        }
        let Some(st) = self.store.clone() else {
            return "?".into();
        };
        let contact = st.contact_by_id(s).await.ok().flatten();
        if let Some(c) = &contact {
            if !c.name.is_empty() {
                return c.name.clone();
            }
        }
        if let (Some(m), Ok(Some(key))) = (self.manager.as_mut(), st.profile_key(s).await) {
            if let Ok(p) = m.retrieve_profile_by_uuid(s.raw_uuid(), key).await {
                if let Some(n) = p.name {
                    let full = format!("{} {}", n.given_name, n.family_name.unwrap_or_default());
                    if !full.trim().is_empty() {
                        return full.trim().to_string();
                    }
                }
            }
        }
        if let Some(p) = contact.and_then(|c| c.phone_number) {
            return p.to_string();
        }
        if let Some(c) = self.chats.get(&format!("u:{}", s.service_id_string())) {
            if !c.name.is_empty() {
                return c.name.clone();
            }
        }
        "Signal user".into()
    }

    async fn read_elsewhere(&mut self, read: &[sync_message::Read]) {
        let Some(st) = self.store.clone() else { return };
        let mut changed = false;
        for r in read {
            let (Some(aci), Some(ts)) = (r.sender_aci.as_deref(), r.timestamp) else {
                continue;
            };
            let Some(sender) = ServiceId::parse_from_service_id_string(aci) else {
                continue;
            };
            if let Ok(Some(thread)) = st.thread_for_sender_and_timestamp(&sender, ts).await {
                if let Some(c) = self.chats.get_mut(&chat_id(&thread)) {
                    if ts > c.read {
                        c.read = ts;
                    }
                    changed = true;
                }
            }
        }
        if changed {
            let ids: Vec<String> = self.chats.keys().cloned().collect();
            for id in ids {
                self.count_unread(&id).await;
            }
            self.save_chats();
            self.contacts();
            self.view();
        }
    }

    async fn count_unread(&mut self, id: &str) {
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        let (Some(thread), Some(st)) = (chat.thread(), self.store.clone()) else {
            return;
        };
        let me = self.my_aci();
        let n = match st.messages(&thread, chat.read.saturating_add(1)..).await {
            Ok(it) => it
                .flatten()
                .filter(|c| {
                    shown(c).is_some()
                        && !matches!(&c.body, ContentBody::SynchronizeMessage(_))
                        && Some(c.metadata.sender.raw_uuid()) != me
                })
                .count() as u32,
            Err(_) => return,
        };
        if let Some(c) = self.chats.get_mut(id) {
            c.unread = n;
        }
    }

    // ------------------------------------------------------------ a chat

    async fn history(&self, chat: &Chat) -> Vec<Content> {
        let (Some(thread), Some(st)) = (chat.thread(), &self.store) else {
            return vec![];
        };
        let all: Vec<Content> = match st.messages(&thread, ..).await {
            Ok(it) => it
                .flatten()
                .filter(|c| shown(c).is_some() || matches!(c.body, ContentBody::NullMessage(_)))
                .collect(),
            Err(_) => return vec![],
        };
        let skip = all.len().saturating_sub(CHAT_LINES);
        all.into_iter().skip(skip).collect()
    }

    /// Sends the lines of an open chat; pictures load afterwards.
    async fn refresh(&mut self, id: &str) {
        if !self.open.contains(id) {
            return;
        }
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        let history = self.history(&chat).await;
        let (lines, missing) = self.lines(&history).await;
        self.host.send(&ToHost::Lines {
            chat: id.into(),
            lines,
        });
        if missing.is_empty() {
            return;
        }
        let mut got = false;
        for a in missing {
            got |= self.fetch(&a).await.is_ok();
        }
        if got {
            let (lines, _) = self.lines(&history).await;
            self.host.send(&ToHost::Lines {
                chat: id.into(),
                lines,
            });
        }
    }

    async fn lines(&mut self, history: &[Content]) -> (Vec<NetLine>, Vec<AttachmentPointer>) {
        let mut out = vec![];
        let mut missing = vec![];
        for c in history {
            let mine = self.is_mine(c);
            let ts = shown(c)
                .and_then(|d| d.timestamp)
                .unwrap_or_else(|| c.timestamp());
            let who = if mine {
                "me".to_string()
            } else {
                self.name_of(&c.metadata.sender).await
            };
            let mut l = NetLine {
                id: ts.to_string(),
                ts: (ts / 1000) as i64,
                from_me: mine,
                who,
                state: if mine { "sent".into() } else { String::new() },
                ..Default::default()
            };
            let Some(d) = shown(c) else {
                l.text = "[deleted]".into();
                out.push(l);
                continue;
            };
            let mut text = d.body.clone().unwrap_or_default();
            if let Some(q) = d.quote.as_ref().and_then(|q| q.text.clone()) {
                text = format!("> {}\n{text}", q.lines().next().unwrap_or_default());
            }
            if let Some(s) = &d.sticker {
                text = format!("[sticker {}]", s.emoji.clone().unwrap_or_default());
            }
            for a in &d.attachments {
                let path = self.media_path(a);
                let image = a
                    .content_type
                    .as_deref()
                    .is_some_and(|t| t.starts_with("image/"))
                    && !a.content_type.as_deref().is_some_and(|t| t == "image/gif");
                if image && l.image.is_empty() {
                    if path.exists() {
                        l.image = path.to_string_lossy().into();
                    } else if a.size.unwrap_or(0) < AUTO_IMAGE {
                        missing.push(a.clone());
                    } else {
                        l.actions = vec![NetAction {
                            id: format!("fetch:{}", key_of(a)),
                            label: "load picture".into(),
                        }];
                    }
                    if text.is_empty() && l.image.is_empty() {
                        text = "[picture]".into();
                    }
                    continue;
                }
                let name = a.file_name.clone().unwrap_or_default();
                let kind = a.content_type.clone().unwrap_or_default();
                let line = if a.flags.unwrap_or(0) & VOICE != 0 {
                    l.actions.push(NetAction {
                        id: format!("open:{}", key_of(a)),
                        label: "play".into(),
                    });
                    "[voice message]".to_string()
                } else if kind.starts_with("video/") {
                    l.actions.push(NetAction {
                        id: format!("open:{}", key_of(a)),
                        label: "open".into(),
                    });
                    "[video]".to_string()
                } else if kind.starts_with("audio/") {
                    l.actions.push(NetAction {
                        id: format!("open:{}", key_of(a)),
                        label: "play".into(),
                    });
                    "[audio]".to_string()
                } else {
                    l.actions.push(NetAction {
                        id: format!("open:{}", key_of(a)),
                        label: "open".into(),
                    });
                    l.actions.push(NetAction {
                        id: format!("save:{}", key_of(a)),
                        label: "save".into(),
                    });
                    format!("[file: {name}]")
                };
                text = if text.is_empty() {
                    line
                } else {
                    format!("{line}\n{text}")
                };
            }
            l.actions.truncate(2);
            l.text = text;
            out.push(l);
        }
        (out, missing)
    }

    fn media_path(&self, a: &AttachmentPointer) -> PathBuf {
        let ext = a
            .file_name
            .as_deref()
            .and_then(|n| Path::new(n).extension())
            .map(|e| e.to_string_lossy().to_string())
            .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()))
            .or_else(|| ext_for(a.content_type.as_deref().unwrap_or("")).map(String::from))
            .unwrap_or_else(|| "bin".into());
        self.dir.join("media").join(format!("{}.{ext}", key_of(a)))
    }

    async fn fetch(&self, a: &AttachmentPointer) -> Result<PathBuf, String> {
        let path = self.media_path(a);
        if path.exists() {
            return Ok(path);
        }
        let m = self.manager.as_ref().ok_or("not linked")?;
        let data = m.get_attachment(a).await.map_err(|e| e.to_string())?;
        sealed::write(&path, &data).map_err(|e| e.to_string())?;
        Ok(path)
    }

    async fn action(&mut self, id: &str, line: &str, action: &str) {
        let Some((what, key)) = action.split_once(':') else {
            return;
        };
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        let (Some(thread), Some(st)) = (chat.thread(), self.store.clone()) else {
            return;
        };
        let Ok(ts) = line.parse::<u64>() else { return };
        let Ok(Some(c)) = st.message(&thread, ts).await else {
            return;
        };
        let Some(a) =
            shown(&c).and_then(|d| d.attachments.iter().find(|a| key_of(a) == key).cloned())
        else {
            return;
        };
        match self.fetch(&a).await {
            Ok(path) => match what {
                "fetch" => self.refresh(id).await,
                // The cache is sealed: the viewer gets a plain copy in a temp
                // folder emptied when the plugin stops; "save" puts one into Downloads.
                "open" => {
                    if let Ok(out) = sealed::plain_copy(&path, &sealed::open_dir(), a.file_name.as_deref()) {
                        open_path(&out);
                    }
                }
                "save" => match sealed::plain_copy(&path, &sealed::downloads_dir(), a.file_name.as_deref()) {
                    Ok(out) => {
                        self.status = format!("saved to {}", out.display());
                        self.view();
                        open_path(out.parent().unwrap_or(&out));
                    }
                    Err(e) => {
                        self.status = format!("could not save it: {e}");
                        self.view();
                    }
                },
                _ => {}
            },
            Err(e) => {
                self.status = format!("could not load it: {e}");
                self.view();
            }
        }
    }

    /// Marks a chat read here and on the user's other devices (a read sync,
    /// as Signal Desktop sends). No read receipts go to the sender.
    async fn mark_read(&mut self, id: &str) {
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        let history = self.history(&chat).await;
        let me = self.my_aci();
        let unread: Vec<sync_message::Read> = history
            .iter()
            .filter(|c| {
                !matches!(&c.body, ContentBody::SynchronizeMessage(_))
                    && Some(c.metadata.sender.raw_uuid()) != me
            })
            .filter_map(|c| {
                let ts = shown(c)?.timestamp.unwrap_or_else(|| c.timestamp());
                (ts > chat.read).then(|| sync_message::Read {
                    sender_aci: Some(c.metadata.sender.service_id_string()),
                    timestamp: Some(ts),
                    ..Default::default()
                })
            })
            .collect();
        let newest = history.iter().map(|c| c.timestamp()).max().unwrap_or(0);
        if let Some(c) = self.chats.get_mut(id) {
            c.unread = 0;
            c.read = c.read.max(newest);
        }
        self.save_chats();
        self.contacts();
        self.view();
        if unread.is_empty() {
            return;
        }
        if let (Some(m), Some(aci)) = (self.manager.as_mut(), me) {
            let sync = SyncMessage {
                read: unread,
                ..Default::default()
            };
            let _ = m
                .send_message(
                    ServiceId::Aci(aci.into()),
                    ContentBody::SynchronizeMessage(sync),
                    now_ms(),
                )
                .await;
        }
    }

    // ------------------------------------------------------------ sending

    fn pend(&mut self, id: &str, text: &str, ts: u64) {
        self.pending.entry(id.into()).or_default().push(NetLine {
            id: format!("p:{ts}"),
            ts: (ts / 1000) as i64,
            from_me: true,
            who: "me".into(),
            text: text.into(),
            state: "sending".into(),
            ..Default::default()
        });
    }

    async fn show_with_pending(&mut self, id: &str) {
        if !self.open.contains(id) {
            return;
        }
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        let history = self.history(&chat).await;
        let (mut lines, _) = self.lines(&history).await;
        lines.extend(self.pending.get(id).cloned().unwrap_or_default());
        self.host.send(&ToHost::Lines {
            chat: id.into(),
            lines,
        });
    }

    async fn deliver(&mut self, id: &str, ts: u64, label: &str, mut msg: DataMessage) {
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        let result = match (chat.thread(), self.manager.as_mut()) {
            (Some(Thread::Contact(s)), Some(m)) => {
                m.send_message(s, msg, ts).await.map_err(|e| e.to_string())
            }
            (Some(Thread::Group(key)), Some(m)) => {
                msg.group_v2 = Some(GroupContextV2 {
                    master_key: Some(key.to_vec()),
                    revision: Some(0),
                    ..Default::default()
                });
                m.send_message_to_group(&key, msg, ts)
                    .await
                    .map_err(|e| e.to_string())
            }
            (_, None) => Err("not linked".into()),
            (None, _) => Err("unknown chat".into()),
        };
        let pid = format!("p:{ts}");
        match result {
            Ok(()) => {
                if let Some(p) = self.pending.get_mut(id) {
                    p.retain(|l| l.id != pid);
                }
                if let Some(c) = self.chats.get_mut(id) {
                    c.last = c.last.max(ts);
                }
                self.save_chats();
            }
            Err(e) => {
                if let Some(l) = self
                    .pending
                    .get_mut(id)
                    .and_then(|p| p.iter_mut().find(|l| l.id == pid))
                {
                    l.state = "failed".into();
                    l.text = format!("{label}\n[not sent: {e}]");
                }
            }
        }
        self.show_with_pending(id).await;
    }

    async fn send_text(&mut self, id: &str, text: String) {
        let ts = now_ms();
        self.pend(id, &text, ts);
        self.show_with_pending(id).await;
        let msg = DataMessage {
            body: Some(text.clone()),
            timestamp: Some(ts),
            ..Default::default()
        };
        self.deliver(id, ts, &text, msg).await;
    }

    async fn send_file(&mut self, id: &str, path: &str) {
        let ts = now_ms();
        let p = Path::new(path);
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let content_type = type_for(&ext);
        let label = if content_type.starts_with("image/") {
            "[picture]".to_string()
        } else {
            format!("[file: {name}]")
        };
        self.pend(id, &label, ts);
        self.show_with_pending(id).await;
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => return self.fail(id, ts, &label, &e.to_string()).await,
        };
        let spec = AttachmentSpec {
            content_type: content_type.into(),
            length: data.len(),
            file_name: Some(name),
            preview: None,
            voice_note: None,
            borderless: None,
            width: None,
            height: None,
            caption: None,
            blur_hash: None,
        };
        let Some(m) = self.manager.as_ref() else {
            return self.fail(id, ts, &label, "not linked").await;
        };
        let pointer = match m.upload_attachment(spec, data.clone()).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return self.fail(id, ts, &label, &format!("{e:?}")).await,
            Err(e) => return self.fail(id, ts, &label, &e.to_string()).await,
        };
        // A copy, so the chat shows it like an incoming one.
        let _ = sealed::write(&self.media_path(&pointer), &data);
        let msg = DataMessage {
            attachments: vec![pointer],
            timestamp: Some(ts),
            ..Default::default()
        };
        self.deliver(id, ts, &label, msg).await;
    }

    async fn fail(&mut self, id: &str, ts: u64, label: &str, why: &str) {
        let pid = format!("p:{ts}");
        if let Some(l) = self
            .pending
            .get_mut(id)
            .and_then(|p| p.iter_mut().find(|l| l.id == pid))
        {
            l.state = "failed".into();
            l.text = format!("{label}\n[not sent: {why}]");
        }
        self.show_with_pending(id).await;
    }
}

/// The message a stored or received content shows as a chat line, if any:
/// text, pictures, files, stickers; not reactions, deletions, group changes.
fn shown(c: &Content) -> Option<&DataMessage> {
    let d = match &c.body {
        ContentBody::DataMessage(d) => d,
        ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    message: Some(d), ..
                })),
            ..
        }) => d,
        ContentBody::EditMessage(EditMessage {
            data_message: Some(d),
            ..
        }) => d,
        ContentBody::SynchronizeMessage(SyncMessage {
            content:
                Some(SyncContent::Sent(sync_message::Sent {
                    edit_message:
                        Some(EditMessage {
                            data_message: Some(d),
                            ..
                        }),
                    ..
                })),
            ..
        }) => d,
        _ => return None,
    };
    (d.body.as_deref().is_some_and(|b| !b.is_empty())
        || !d.attachments.is_empty()
        || d.sticker.is_some())
    .then_some(d)
}

/// A short, stable name for an attachment's file.
fn key_of(a: &AttachmentPointer) -> String {
    let seed = a
        .digest
        .clone()
        .unwrap_or_else(|| format!("{:?}", a.attachment_identifier).into_bytes());
    hex::encode(&Sha256::digest(seed)[..12])
}

fn ext_for(t: &str) -> Option<&'static str> {
    Some(match t {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "audio/aac" => "aac",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        _ => return None,
    })
}

fn type_for(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// A plain SQLite store from before encryption becomes a SQLCipher one with
/// `key` (hex), via sqlcipher_export; the plain files are removed.
async fn encrypt_plain_db(path: &Path, key: &str) -> Result<(), String> {
    use sqlx::{Connection, Executor};
    let head = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };
    if !head.starts_with(b"SQLite format 3\0") {
        return Ok(());
    }
    let enc = path.with_extension("db.encrypting");
    let _ = std::fs::remove_file(&enc);
    // create_if_missing: ATTACH creates the new file with the connection's flags.
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(path).create_if_missing(true);
    let mut c = sqlx::SqliteConnection::connect_with(&opts).await.map_err(|e| e.to_string())?;
    let target = enc.to_string_lossy().replace('\'', "''");
    let attach = format!("ATTACH DATABASE '{target}' AS enc KEY \"x'{key}'\"");
    c.execute(sqlx::AssertSqlSafe(attach)).await.map_err(|e| e.to_string())?;
    c.execute("SELECT sqlcipher_export('enc')").await.map_err(|e| e.to_string())?;
    c.execute("DETACH DATABASE enc").await.map_err(|e| e.to_string())?;
    c.close().await.map_err(|e| e.to_string())?;
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    std::fs::rename(&enc, path).map_err(|e| e.to_string())
}

/// Hands a file or folder to the system.
fn open_path(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("rundll32")
        .arg("url.dll,FileProtocolHandler")
        .arg(path)
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An old plain store becomes SQLCipher with the plugin key: unreadable
    /// without it, and presage's store opens on it with the key.
    #[tokio::test(flavor = "current_thread")]
    async fn the_store_is_encrypted() {
        use sqlx::{Connection, Executor, Row};
        let dir = std::env::temp_dir().join(format!("rim-signal-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("signal.db");
        let key = "44".repeat(32);

        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).create_if_missing(true);
        let mut c = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        c.execute("CREATE TABLE t (x TEXT); INSERT INTO t VALUES ('secret')").await.unwrap();
        c.close().await.unwrap();

        encrypt_plain_db(&path, &key).await.unwrap();
        let disk = std::fs::read(&path).unwrap();
        assert!(!disk.starts_with(b"SQLite format 3"));
        assert!(!disk.windows(6).any(|w| w == b"secret"));

        let keyed = |k: &str| sqlx::sqlite::SqliteConnectOptions::new().filename(&path).pragma("key", format!("\"x'{k}'\""));
        let mut c = sqlx::SqliteConnection::connect_with(&keyed(&key)).await.unwrap();
        let row = sqlx::query("SELECT x FROM t").fetch_one(&mut c).await.unwrap();
        assert_eq!(row.get::<String, _>(0), "secret");
        c.close().await.unwrap();
        let wrong = sqlx::SqliteConnection::connect_with(&keyed(&"55".repeat(32))).await;
        let failed = match wrong {
            Err(_) => true,
            Ok(mut c) => sqlx::query("SELECT x FROM t").fetch_one(&mut c).await.is_err(),
        };
        assert!(failed, "opened with the wrong key");

        // presage's own store, created fresh under SQLCipher.
        let fresh = dir.join("fresh.db");
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&fresh).create_if_missing(true).foreign_keys(true).pragma("key", format!("\"x'{key}'\""));
        SqliteStore::open_with_options(opts, OnNewIdentity::Trust).await.unwrap();
        assert!(!std::fs::read(&fresh).unwrap().starts_with(b"SQLite format 3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_ids_do_not_carry_the_key() {
        let key = [7u8; 32];
        let id = chat_id(&Thread::Group(key));
        assert!(id.starts_with("g:") && id.len() == 18);
        assert!(!id.contains(&hex::encode(key)[..8]));
        let c = Chat {
            id,
            target: hex::encode(key),
            group: true,
            ..Default::default()
        };
        assert!(matches!(c.thread(), Some(Thread::Group(k)) if k == key));
    }

    #[test]
    fn only_messages_become_lines() {
        let text = DataMessage {
            body: Some("hi".into()),
            ..Default::default()
        };
        let reaction = DataMessage {
            reaction: Some(Default::default()),
            ..Default::default()
        };
        let c = |d: DataMessage| {
            Content::from_body(
                d,
                presage::libsignal_service::content::Metadata {
                    sender: ServiceId::Aci(uuid::Uuid::nil().into()),
                    destination: ServiceId::Aci(uuid::Uuid::nil().into()),
                    sender_device: 1.try_into().unwrap(),
                    pni_verified: None,
                    client_timestamp: Default::default(),
                    server_timestamp: Default::default(),
                    needs_receipt: false,
                    unidentified_sender: false,
                    was_plaintext: false,
                    server_guid: None,
                },
            )
        };
        assert!(shown(&c(text)).is_some());
        assert!(shown(&c(reaction)).is_none());
    }

    #[test]
    fn attachment_files_get_a_sensible_name() {
        let a = AttachmentPointer {
            content_type: Some("image/jpeg".into()),
            digest: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        assert_eq!(key_of(&a).len(), 24);
        assert_eq!(ext_for("image/jpeg"), Some("jpg"));
        assert_eq!(type_for("pdf"), "application/pdf");
    }
}
