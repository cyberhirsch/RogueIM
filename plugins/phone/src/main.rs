//! Phone plugin: the messages on your Android phone (WhatsApp, Signal, SMS …)
//! in RogueIM, without touching any of their protocols. The RogueIM Phone app
//! reads their notifications and answers through the notification's reply
//! button; this plugin is the other end, on the local network (see link.rs).
//! Limits: only what arrives as a notification, no history, the phone must be
//! on and in the same network.

mod link;

use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use rim_plugin_sdk::{Item, NetContact, NetLine, Plugin, ToHost, ToPlugin};
use serde::{Deserialize, Serialize};
use serde_json::json;

const WARNING: &str = "copies of your phone's notifications, encrypted between phone and RogueIM over your network. How private the message itself is depends on the app it came from; SMS is not encrypted at all.";
const PORT: u16 = 47812;
const KEEP_LINES: usize = 60;
const KEEP_DAYS: i64 = 30;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Config {
    port: u16,
    /// base64url, 32 bytes
    key: String,
    /// The phone that connected last.
    device: String,
    apps: Vec<String>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Chat {
    id: String,
    name: String,
    app: String,
    group: bool,
    last: i64,
    unread: u32,
    lines: Vec<NetLine>,
}

fn data_dir() -> PathBuf {
    std::env::var_os("RIM_PLUGIN_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn load<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    rim_plugin_sdk::sealed::read(&data_dir().join(name)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save<T: Serialize>(name: &str, v: &T) {
    let _ = std::fs::create_dir_all(data_dir());
    let _ = rim_plugin_sdk::sealed::write(&data_dir().join(name), &serde_json::to_vec_pretty(v).unwrap_or_default());
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// This computer's address in the local network (no packet is sent).
fn local_ip() -> String {
    UdpSocket::bind("0.0.0.0:0").and_then(|s| s.connect("192.0.2.1:9").map(|_| s)).and_then(|s| s.local_addr()).map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".into())
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "RogueIM".into()).replace(':', " ")
}

struct State {
    cfg: Config,
    key: [u8; 32],
    chats: Vec<Chat>,
    open: Vec<String>,
    link: link::Link,
    status: String,
    show_code: bool,
    pending: u32,
}

impl State {
    fn code(&self) -> String {
        format!("rimphone:1:{}:{}:{}:{}", local_ip(), self.cfg.port, B64.encode(self.key), computer_name())
    }

    fn view(&self, p: &mut Plugin) {
        let mut items = vec![];
        let connected = self.link.connected();
        if self.cfg.device.is_empty() || self.show_code {
            items.push(Item::text("Link your Android phone", 0));
            items.push(Item::qr(&self.code(), 1));
            items.push(Item::dim("Install the RogueIM Phone app (an APK with each release), open it and scan this code. Phone and computer must be in the same network.", 2));
            items.push(Item::dim(&format!("waiting on {}:{}", local_ip(), self.cfg.port), 3));
            if self.show_code {
                items.push(Item::button("hide", "done", 4));
            }
        } else {
            items.push(Item::text(&self.cfg.device, 0));
            let state = if connected { "connected" } else { "not connected: phone away, off or asleep" };
            items.push(Item::dim(state, 1));
            if !self.cfg.apps.is_empty() {
                items.push(Item::dim(&format!("forwards {}", self.cfg.apps.join(", ")), 2));
            }
            items.push(Item::button("code", "show code", 3));
            items.push(Item::button("forget", "forget phone", 3));
        }
        if !self.status.is_empty() {
            items.push(Item::dim(&self.status, 5));
        }
        items.push(Item::dim("Only what arrives as a notification; replies go through the app's reply button.", 9));
        p.view(items);
    }

    fn contacts(&mut self, p: &mut Plugin) {
        let cutoff = now() - KEEP_DAYS * 86400;
        self.chats.retain(|c| c.last >= cutoff);
        let mut list: Vec<NetContact> = self
            .chats
            .iter()
            .map(|c| NetContact { id: c.id.clone(), name: c.name.clone(), status: String::new(), info: if c.group { format!("{} · group", c.app) } else { c.app.clone() }, unread: c.unread })
            .collect();
        list.sort_by_key(|c| c.name.to_lowercase());
        p.send(&ToHost::Contacts { list });
    }

    fn lines(&self, p: &mut Plugin, chat: &str) {
        if !self.open.iter().any(|c| c == chat) {
            return;
        }
        if let Some(c) = self.chats.iter().find(|c| c.id == chat) {
            p.send(&ToHost::Lines { chat: chat.to_string(), lines: c.lines.clone() });
        }
    }

    fn save_chats(&self) {
        save("phone-chats.json", &self.chats);
    }

    /// A message from the phone.
    fn from_phone(&mut self, p: &mut Plugin, v: serde_json::Value) {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        match s("t").as_str() {
            "msg" => {
                let (id, chat) = (s("id"), s("chat"));
                if chat.is_empty() || s("text").is_empty() {
                    return;
                }
                let ts = v.get("ts").and_then(|x| x.as_i64()).map(|ms| ms / 1000).unwrap_or_else(now);
                let open = self.open.iter().any(|c| *c == chat);
                let idx = match self.chats.iter().position(|c| c.id == chat) {
                    Some(i) => i,
                    None => {
                        self.chats.push(Chat { id: chat.clone(), ..Default::default() });
                        self.chats.len() - 1
                    }
                };
                let c = &mut self.chats[idx];
                c.name = s("conv");
                c.app = s("app");
                c.group = v.get("group").and_then(|x| x.as_bool()).unwrap_or(false);
                if c.lines.iter().any(|l| l.id == id) {
                    return;
                }
                c.last = c.last.max(ts);
                c.lines.push(NetLine { id, ts, from_me: false, who: s("who"), text: s("text"), ..Default::default() });
                c.lines.sort_by_key(|l| l.ts);
                let skip = c.lines.len().saturating_sub(KEEP_LINES);
                c.lines.drain(..skip);
                let name = format!("{} ({})", if c.group { format!("{} in {}", s("who"), c.name) } else { c.name.clone() }, c.app);
                if open {
                    self.link.send(&json!({ "t": "read", "chat": chat }));
                } else {
                    c.unread += 1;
                }
                self.save_chats();
                self.contacts(p);
                self.lines(p, &chat);
                p.send(&ToHost::Incoming { chat, name });
            }
            "sent" | "failed" => {
                let (chat, r) = (s("chat"), s("ref"));
                if let Some(l) = self.chats.iter_mut().find(|c| c.id == chat).and_then(|c| c.lines.iter_mut().find(|l| l.id == r)) {
                    l.state = s("t");
                    if l.state == "failed" {
                        l.text = format!("{}\n[not sent: {}]", l.text, s("why"));
                    }
                }
                self.save_chats();
                self.lines(p, &chat);
            }
            "apps" => {
                self.cfg.apps = v.get("list").and_then(|l| l.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
                save("phone.json", &self.cfg);
                self.view(p);
            }
            _ => {}
        }
    }

    fn reply(&mut self, p: &mut Plugin, chat: &str, text: &str) {
        self.pending += 1;
        let r = format!("p:{}:{}", now(), self.pending);
        let sent = self.link.send(&json!({ "t": "reply", "chat": chat, "text": text, "ref": r }));
        let Some(c) = self.chats.iter_mut().find(|c| c.id == chat) else { return };
        let mut l = NetLine { id: r, ts: now(), from_me: true, who: "me".into(), text: text.to_string(), state: "sending".into(), ..Default::default() };
        if !sent {
            l.state = "failed".into();
            l.text.push_str("\n[not sent: the phone is not connected]");
        }
        c.last = now();
        c.lines.push(l);
        self.save_chats();
        self.lines(p, chat);
    }

    fn new_key(&mut self) {
        self.key = link::random();
        self.cfg.key = B64.encode(self.key);
        self.cfg.device.clear();
        self.cfg.apps.clear();
        save("phone.json", &self.cfg);
    }
}

fn main() {
    let Ok(mut p) = Plugin::connect("phone") else {
        eprintln!("rim-plugin-phone is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    p.send(&ToHost::Channel { label: "phone".into(), level: "C".into(), warning: WARNING.into(), add_hint: String::new(), files: false });
    let mut cfg: Config = load("phone.json");
    let key = B64.decode(&cfg.key).ok().and_then(|k| <[u8; 32]>::try_from(k).ok());
    let mut st = State { key: key.unwrap_or_else(link::random), cfg: Config::default(), chats: load("phone-chats.json"), open: vec![], link: link::Link::default(), status: String::new(), show_code: false, pending: 0 };
    if key.is_none() {
        cfg.key = B64.encode(st.key);
    }
    // The same port as last time, so the phone's saved code keeps working.
    let listener = TcpListener::bind(("0.0.0.0", if cfg.port == 0 { PORT } else { cfg.port })).or_else(|_| TcpListener::bind(("0.0.0.0", 0)));
    match listener {
        Ok(l) => {
            cfg.port = l.local_addr().map(|a| a.port()).unwrap_or(PORT);
            st.cfg = cfg;
            save("phone.json", &st.cfg);
            let (up_tx, up_rx) = mpsc::channel();
            link::serve(l, st.key, st.link.clone(), up_tx);
            p.network(&format!("{} (local network only)", local_ip()), "phone link");
            run(&mut p, &mut st, up_rx);
        }
        Err(e) => {
            st.cfg = cfg;
            st.status = format!("cannot listen for the phone: {e}");
            st.view(&mut p);
            while !matches!(p.recv(Duration::from_secs(1)), Some(ToPlugin::Stop)) {}
        }
    }
}

fn run(p: &mut Plugin, st: &mut State, up: mpsc::Receiver<link::Up>) {
    st.contacts(p);
    st.view(p);
    let mut ping = Instant::now();
    loop {
        while let Ok(u) = up.try_recv() {
            match u {
                link::Up::Connected(hello) => {
                    let device = hello.get("device").and_then(|d| d.as_str()).unwrap_or("phone").to_string();
                    st.cfg.device = device;
                    st.show_code = false;
                    st.status.clear();
                    save("phone.json", &st.cfg);
                    st.link.send(&json!({ "t": "hello", "name": computer_name() }));
                    st.view(p);
                }
                link::Up::Message(v) => st.from_phone(p, v),
                link::Up::Gone => st.view(p),
                link::Up::Error(e) => {
                    st.status = e;
                    st.view(p);
                }
            }
        }
        if ping.elapsed() > Duration::from_secs(30) {
            ping = Instant::now();
            st.link.send(&json!({ "t": "ping" }));
        }
        match p.recv(Duration::from_millis(150)) {
            Some(ToPlugin::Stop) => break,
            Some(ToPlugin::Click { id }) => {
                match id.as_str() {
                    "code" => st.show_code = true,
                    "hide" => st.show_code = false,
                    "forget" => {
                        // A new key: the old phone can't connect any more.
                        st.new_key();
                        st.status = "forgotten; scan the new code to link a phone".into();
                    }
                    _ => {}
                }
                st.view(p);
            }
            Some(ToPlugin::Open { chat }) => {
                st.open.push(chat.clone());
                if let Some(c) = st.chats.iter_mut().find(|c| c.id == chat) {
                    if c.unread > 0 {
                        c.unread = 0;
                        st.link.send(&json!({ "t": "read", "chat": chat }));
                        st.save_chats();
                        st.contacts(p);
                    }
                }
                st.lines(p, &chat);
            }
            Some(ToPlugin::Close { chat }) => st.open.retain(|c| *c != chat),
            Some(ToPlugin::Send { chat, text }) => st.reply(p, &chat, &text),
            Some(ToPlugin::Remove { chat }) => {
                st.chats.retain(|c| c.id != chat);
                st.save_chats();
                st.contacts(p);
            }
            _ => {}
        }
    }
}
