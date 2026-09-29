//! Plugin host (PRD GD-1..11). Every gadget is a separate process, started
//! only when enabled and stopped when disabled. It talks JSON lines over a
//! local socket and can only do what its capabilities allow; it never sees
//! keys, sessions or history.
//!
//! v0.1 ships first-party plugins only (they sit next to the rogueim binary);
//! third-party plugins with signature prompts come later (GD-1b).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use rim_plugin_sdk::{Item, Manifest, ToHost, ToPlugin, SOCKET_ENV};

pub fn first_party() -> Vec<Manifest> {
    let m = |id: &str, name: &str, desc: &str, caps: &[&str]| Manifest {
        id: id.into(),
        name: name.into(),
        description: desc.into(),
        exec: format!("rim-plugin-{id}"),
        capabilities: caps.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        m("pomodoro", "Pomodoro", "Focus timer; sets Occupied while you focus.", &["presence.set", "settings.sync"]),
        m("todo", "Todo", "todo.txt or CalDAV tasks; \"add as task\" in chats.", &["chat.context_menu", "network:caldav"]),
        m("player", "Player", "Controls the active media player; can share what you listen to.", &["presence.now_playing"]),
    ]
}

pub fn exe_for(m: &Manifest) -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let name = if cfg!(windows) { format!("{}.exe", m.exec) } else { m.exec.clone() };
    [dir.join(&name), dir.join("plugins").join(&name)].into_iter().find(|p| p.exists())
}

struct Running {
    child: Child,
    out: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
}

pub struct Host {
    running: HashMap<String, Running>,
    tx: Sender<(String, ToHost)>,
    rx: Receiver<(String, ToHost)>,
    pub views: HashMap<String, Vec<Item>>,
}

impl Host {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Host { running: HashMap::new(), tx, rx, views: HashMap::new() }
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains_key(id)
    }

    pub fn start(&mut self, m: &Manifest, data_dir: &Path) -> Result<(), String> {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        if self.running.contains_key(&m.id) {
            return Ok(());
        }
        let exe = exe_for(m).ok_or_else(|| format!("{} is not installed next to RogueIM", m.exec))?;
        let sock = format!("rogueim-plugin-{}-{}-{}", std::process::id(), m.id, rand_suffix());
        let ns = sock.clone().to_ns_name::<GenericNamespaced>().map_err(|e| e.to_string())?;
        let listener = ListenerOptions::new().name(ns).create_sync().map_err(|e| e.to_string())?;
        let data = data_dir.join("plugins").join(&m.id);
        let _ = std::fs::create_dir_all(&data);
        let mut cmd = Command::new(exe);
        cmd.env(SOCKET_ENV, &sock).env("RIM_PLUGIN_DATA", &data);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        let out: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
        let (out2, tx, id) = (out.clone(), self.tx.clone(), m.id.clone());
        std::thread::spawn(move || {
            // One connection per plugin process.
            let Some(Ok(conn)) = listener.incoming().next() else { return };
            let (recv, send) = conn.split();
            *out2.lock().unwrap() = Some(Box::new(send));
            for line in BufReader::new(recv).lines().map_while(Result::ok) {
                if let Ok(msg) = serde_json::from_str::<ToHost>(&line) {
                    if tx.send((id.clone(), msg)).is_err() {
                        break;
                    }
                }
            }
        });
        self.running.insert(m.id.clone(), Running { child, out });
        Ok(())
    }

    pub fn send(&self, id: &str, msg: &ToPlugin) {
        let Some(r) = self.running.get(id) else { return };
        if let Some(w) = r.out.lock().unwrap().as_mut() {
            if let Ok(s) = serde_json::to_string(msg) {
                let _ = w.write_all(format!("{s}\n").as_bytes());
                let _ = w.flush();
            }
        }
    }

    pub fn stop(&mut self, id: &str) {
        self.send(id, &ToPlugin::Stop);
        if let Some(mut r) = self.running.remove(id) {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(800));
                let _ = r.child.kill();
                let _ = r.child.wait();
            });
        }
        self.views.remove(id);
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            self.stop(&id);
        }
    }

    /// Messages from plugins; also notices plugins that exited on their own.
    pub fn poll(&mut self) -> Vec<(String, ToHost)> {
        let dead: Vec<String> = self.running.iter_mut().filter_map(|(k, r)| matches!(r.child.try_wait(), Ok(Some(_))).then(|| k.clone())).collect();
        for id in dead {
            self.running.remove(&id);
            self.views.remove(&id);
        }
        self.rx.try_iter().collect()
    }
}

fn rand_suffix() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
}

/// May this plugin send this message?
pub fn allowed(m: &Manifest, msg: &ToHost) -> bool {
    let has = |c: &str| m.capabilities.iter().any(|x| x == c);
    match msg {
        ToHost::Status { .. } | ToHost::RestoreStatus => has("presence.set"),
        ToHost::NowPlaying { .. } => has("presence.now_playing"),
        ToHost::State { .. } => has("settings.sync"),
        ToHost::Hello { .. } | ToHost::View { .. } | ToHost::Notify { .. } | ToHost::Sound { .. } => true,
    }
}
