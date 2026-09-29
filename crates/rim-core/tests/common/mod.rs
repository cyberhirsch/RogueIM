#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rim_core::engine::{RestoreSecret, StartMode};
use rim_core::{spawn, Command, EngineConfig, EngineHandle, Event};

pub fn tmp(name: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("rim-test-{name}-{n}"));
    let _ = std::fs::remove_dir_all(&d);
    d
}

pub struct Peer {
    pub h: EngineHandle,
    pub rx: Receiver<Event>,
    pub dir: PathBuf,
    pub name: String,
}

impl Peer {
    pub fn start(name: &str, pass: &str) -> Peer {
        Self::start_with(name, pass, tmp(name), StartMode::Auto, false)
    }

    pub fn start_with(name: &str, pass: &str, dir: PathBuf, mode: StartMode, internet: bool) -> Peer {
        let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
        let (h, rx) = spawn(EngineConfig {
            dir: dir.clone(),
            passphrase: pass.into(),
            nick: Some(name.into()),
            device_name: Some(format!("{name}-dev")),
            mode,
            relays: if internet { None } else { Some(vec![]) },
            ..Default::default()
        });
        Peer { h, rx, dir, name: name.into() }
    }

    pub fn restore(name: &str, pass: &str, backup: &str, secret: RestoreSecret) -> Peer {
        Self::start_with(name, pass, tmp(name), StartMode::Restore { path: backup.into(), secret }, false)
    }

    pub fn send(&self, c: Command) {
        self.h.send(c);
    }

    /// Wait until `f` returns Some for an event, failing after `secs`.
    pub fn wait<T>(&self, secs: u64, what: &str, mut f: impl FnMut(&Event) -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(100)) {
                if let Some(v) = f(&ev) {
                    return v;
                }
                if let Event::LoginFailed(e) = &ev {
                    panic!("[{}] engine failed while waiting for {what}: {e}", self.name);
                }
            }
        }
        panic!("[{}] timed out waiting for {what}", self.name);
    }

    pub fn unlocked(&self) {
        self.wait(40, "unlock", |e| matches!(e, Event::Unlocked { .. }).then_some(()));
    }

    pub fn invite(&self) -> String {
        self.send(Command::NewInvite { uses: Some(1), ttl_secs: None, label: String::new() });
        self.wait(10, "invite", |e| if let Event::Invite(s) = e { Some(s.clone()) } else { None })
    }

    pub fn contact_id(&self, name: &str) -> String {
        self.wait(30, &format!("contact {name}"), |e| match e {
            Event::Contacts(v) => v.iter().find(|c| c.name == name).map(|c| c.id.clone()),
            _ => None,
        })
    }

    pub fn stop(self) {
        self.h.send(Command::Shutdown);
        let _ = self.wait(10, "stop", |e| matches!(e, Event::Stopped).then_some(()));
    }
}

/// a invites b, a accepts; returns (id of b at a, id of a at b).
pub fn befriend(a: &Peer, b: &Peer) -> (String, String) {
    let inv = a.invite();
    b.send(Command::AddContact { invite: inv, text: format!("hi from {}", b.name) });
    let pid = a.wait(30, "auth request", |e| match e {
        Event::Pending(v) => v.iter().find(|p| p.nick == b.name).map(|p| p.id.clone()),
        _ => None,
    });
    a.send(Command::Accept { id: pid.clone() });
    let a_at_b = b.wait(30, "accepted + online", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == a.name && !c.awaiting && c.status == rim_core::Status::Online).map(|c| c.id.clone()),
        _ => None,
    });
    (pid, a_at_b)
}

pub fn text(to: &str, body: &str) -> Command {
    Command::SendText { id: to.into(), body: body.into(), reply_to: None, urgent: false }
}

pub fn got_text(p: &Peer, secs: u64, body: &str) {
    p.wait(secs, &format!("text '{body}'"), |e| match e {
        Event::Incoming { text, .. } if text == body => Some(()),
        _ => None,
    });
}
