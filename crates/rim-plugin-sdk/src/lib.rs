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
}

/// RogueIM -> plugin.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToPlugin {
    Click { id: String },
    Input { id: String, value: String },
    Check { id: String, checked: bool },
    /// capability `chat.context_menu`: the user chose "add as task" on a message.
    Task { text: String, from: String },
    /// Synced state from another device.
    State { state: serde_json::Value },
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

    /// Next message from the host, or None after `timeout` (use it to tick).
    pub fn recv(&self, timeout: Duration) -> Option<ToPlugin> {
        self.rx.recv_timeout(timeout).ok()
    }
}
