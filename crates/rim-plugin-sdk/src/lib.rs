//! RogueIM plugin SDK.
//!
//! A plugin is its own process. RogueIM starts it only when the user enables
//! it, passing a local socket name in `RIM_PLUGIN_SOCKET`. The plugin and the
//! host exchange JSON lines ([`ToHost`], [`ToPlugin`]).
//!
//! Plugins never render anything themselves: they describe their sidebar
//! section as a list of [`Item`]s (titles, text, buttons, inputs, progress,
//! checkboxes) and RogueIM draws it. Plugins cannot reach RogueIM's keys,
//! sessions or history; what they may ask for is limited by the capabilities
//! in their manifest (see [`Manifest`]).
//!
//! Plugins with the `chats` capability bring another network (email,
//! Telegram …) into RogueIM: they announce it with [`ToHost::Channel`], hand
//! over contacts ([`NetContact`]) and messages ([`NetLine`]), and RogueIM shows
//! them in its own buddy list and chat windows, marked as not RIM-encrypted.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const SOCKET_ENV: &str = "RIM_PLUGIN_SOCKET";

/// One element of a plugin's sidebar section. Items with the same `row` are
/// laid out side by side.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Item {
    /// "title", "text", "dim", "button", "input", "progress", "check"
    pub kind: String,
    pub id: String,
    pub text: String,
    /// Placeholder for inputs.
    #[serde(default)]
    pub hint: String,
    /// 0..1 for progress.
    #[serde(default)]
    pub value: f32,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub row: u32,
}

impl Item {
    fn new(kind: &str, id: &str, text: &str, row: u32) -> Self {
        Item { kind: kind.into(), id: id.into(), text: text.into(), row, ..Default::default() }
    }
    pub fn title(text: &str, row: u32) -> Self {
        Self::new("title", "", text, row)
    }
    pub fn text(text: &str, row: u32) -> Self {
        Self::new("text", "", text, row)
    }
    pub fn dim(text: &str, row: u32) -> Self {
        Self::new("dim", "", text, row)
    }
    pub fn button(id: &str, text: &str, row: u32) -> Self {
        Self::new("button", id, text, row)
    }
    pub fn input(id: &str, value: &str, hint: &str, row: u32) -> Self {
        Item { hint: hint.into(), ..Self::new("input", id, value, row) }
    }
    pub fn progress(value: f32, row: u32) -> Self {
        Item { value, ..Self::new("progress", "", "", row) }
    }
    pub fn check(id: &str, text: &str, checked: bool, row: u32) -> Self {
        Item { checked, ..Self::new("check", id, text, row) }
    }
}

/// A contact of another network (capability `chats`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NetContact {
    /// Stable within the plugin, e.g. the email address.
    pub id: String,
    pub name: String,
    /// "" when the network has no presence, else "online", "away" or "offline".
    #[serde(default)]
    pub status: String,
    /// Shown in the chat header, e.g. the address.
    #[serde(default)]
    pub info: String,
    #[serde(default)]
    pub unread: u32,
}

/// One message in a chat of another network (capability `chats`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NetLine {
    /// Stable within the chat.
    pub id: String,
    /// Unix seconds.
    pub ts: i64,
    pub from_me: bool,
    pub who: String,
    pub text: String,
    /// Own messages: "sending", "sent" or "failed".
    #[serde(default)]
    pub state: String,
}

/// Plugin -> RogueIM.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToHost {
    Hello { id: String },
    View { items: Vec<Item> },
    /// capability `presence.set`
    Status { status: String, away: String },
    /// capability `presence.set`
    RestoreStatus,
    /// capability `presence.now_playing`
    NowPlaying { text: String },
    Notify { title: String, body: String },
    Sound { name: String },
    /// capability `settings.sync`: state shared with the user's other devices.
    State { state: serde_json::Value },
    /// Announce an outbound network destination the plugin talks to (shown in
    /// the host's diagnostics). Send it once per destination, and again when
    /// the configured destination changes.
    Network { host: String, purpose: String },
    /// capability `chats`: the network this plugin brings in. `level` is "C"
    /// (protected only by that network, read by its servers or client) or "D"
    /// (not encrypted at all); `warning` is shown in every chat of it.
    /// A non-empty `add_hint` ("an email address") lets the user add contacts;
    /// it is shown in the add field.
    Channel {
        label: String,
        level: String,
        warning: String,
        #[serde(default)]
        add_hint: String,
    },
    /// capability `chats`: the whole contact list, replacing the last one.
    Contacts { list: Vec<NetContact> },
    /// capability `chats`: the messages of one chat, oldest first, replacing
    /// what is shown.
    Lines { chat: String, lines: Vec<NetLine> },
    /// capability `chats`: a new message arrived (sound, notification, pop-up).
    Incoming { chat: String, name: String },
}

/// RogueIM -> plugin.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToPlugin {
    Click { id: String },
    /// An input was committed with Enter.
    Input { id: String, value: String },
    /// The text of an input while it is being typed (so a button next to it
    /// sees the current value without Enter). Not committed yet.
    Edit { id: String, value: String },
    Check { id: String, checked: bool },
    /// capability `chat.context_menu`: the user chose "add as task" on a message.
    Task { text: String, from: String },
    /// Synced state from another device.
    State { state: serde_json::Value },
    /// capability `chats`: a chat was opened; send its lines and mark it read.
    Open { chat: String },
    /// capability `chats`: the chat window was closed.
    Close { chat: String },
    /// capability `chats`: the user wrote in a chat.
    Send { chat: String, text: String },
    /// capability `chats`: the user added a contact by address (see `add_hint`).
    Add { address: String },
    /// capability `chats`: the user took a contact off the list.
    Remove { chat: String },
    Stop,
}

/// `plugin.toml` equivalent; first-party plugins are described in the host.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub exec: String,
    pub capabilities: Vec<String>,
}

pub struct Plugin {
    out: Box<dyn Write + Send>,
    rx: Receiver<ToPlugin>,
}

impl Plugin {
    /// Connect to the host that started us and say hello.
    pub fn connect(id: &str) -> std::io::Result<Plugin> {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, Stream};
        let name = std::env::var(SOCKET_ENV).map_err(|_| std::io::Error::other("RIM_PLUGIN_SOCKET not set: plugins are started by RogueIM"))?;
        let stream = Stream::connect(name.to_ns_name::<GenericNamespaced>()?)?;
        let (recv, send) = stream.split();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(recv).lines().map_while(Result::ok) {
                if let Ok(m) = serde_json::from_str::<ToPlugin>(&line) {
                    if tx.send(m).is_err() {
                        break;
                    }
                }
            }
            let _ = tx.send(ToPlugin::Stop);
        });
        let mut p = Plugin { out: Box::new(send), rx };
        p.send(&ToHost::Hello { id: id.into() });
        Ok(p)
    }

    pub fn send(&mut self, m: &ToHost) {
        if let Ok(s) = serde_json::to_string(m) {
            let _ = self.out.write_all(format!("{s}\n").as_bytes());
            let _ = self.out.flush();
        }
    }

    pub fn view(&mut self, items: Vec<Item>) {
        self.send(&ToHost::View { items });
    }

    /// Announce an outbound network destination ([`ToHost::Network`]).
    pub fn network(&mut self, host: &str, purpose: &str) {
        self.send(&ToHost::Network { host: host.into(), purpose: purpose.into() });
    }

    /// Next message from the host, or None after `timeout` (use it to tick).
    pub fn recv(&self, timeout: Duration) -> Option<ToPlugin> {
        self.rx.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_serde_shape() {
        let m = ToHost::Lines { chat: "a@b.c".into(), lines: vec![NetLine { id: "i:7".into(), ts: 1, from_me: false, who: "Anna".into(), text: "hi".into(), state: String::new() }] };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"t":"lines","chat":"a@b.c","lines":[{"id":"i:7","ts":1,"from_me":false,"who":"Anna","text":"hi","state":""}]}"#);
        // Optional fields may be left out by plugins.
        let c: ToHost = serde_json::from_str(r#"{"t":"contacts","list":[{"id":"a@b.c","name":"Anna"}]}"#).unwrap();
        match c {
            ToHost::Contacts { list } => assert_eq!((list[0].unread, list[0].status.as_str()), (0, "")),
            other => panic!("unexpected {other:?}"),
        }
        let ch: ToHost = serde_json::from_str(r#"{"t":"channel","label":"mail","level":"D","warning":"not encrypted"}"#).unwrap();
        assert!(matches!(ch, ToHost::Channel { add_hint, .. } if add_hint.is_empty()));
        assert_eq!(serde_json::to_string(&ToPlugin::Send { chat: "a@b.c".into(), text: "yo".into() }).unwrap(), r#"{"t":"send","chat":"a@b.c","text":"yo"}"#);
    }

    #[test]
    fn network_serde_shape() {
        let m = ToHost::Network { host: "dav.example.org".into(), purpose: "CalDAV tasks".into() };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"t":"network","host":"dav.example.org","purpose":"CalDAV tasks"}"#);
        match serde_json::from_str::<ToHost>(&s).unwrap() {
            ToHost::Network { host, purpose } => assert_eq!((host.as_str(), purpose.as_str()), ("dav.example.org", "CalDAV tasks")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
