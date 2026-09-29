//! Two engines on this machine: invite, authorize, presence, encrypted chat, restart.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rim_core::proto::Delivery;
use rim_core::{spawn, Command, EngineConfig, EngineHandle, Event, Status};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-test-{name}-{}", rand_suffix()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
}

fn start(dir: &PathBuf, nick: &str, pass: &str) -> (EngineHandle, Receiver<Event>) {
    spawn(EngineConfig { dir: dir.clone(), passphrase: pass.into(), nick: Some(nick.into()), port: 0, ..Default::default() })
}

/// Wait until `f` returns Some for an event, failing after `secs`.
fn wait<T>(rx: &Receiver<Event>, secs: u64, what: &str, mut f: impl FnMut(&Event) -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
            if let Some(v) = f(&ev) {
                return v;
            }
            if let Event::LoginFailed(e) = &ev {
                panic!("engine failed while waiting for {what}: {e}");
            }
        }
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn invite_authorize_chat_and_restart() {
    let (da, db) = (tmp("alice"), tmp("bob"));
    let (a, arx) = start(&da, "alice", "pass-a");
    let (b, brx) = start(&db, "bob", "pass-b");
    wait(&arx, 10, "alice unlocked", |e| matches!(e, Event::Unlocked { .. }).then_some(()));
    wait(&brx, 10, "bob unlocked", |e| matches!(e, Event::Unlocked { .. }).then_some(()));
    std::thread::sleep(Duration::from_millis(500)); // let listeners come up

    // Alice makes a single-use invite; Bob uses it.
    a.send(Command::NewInvite);
    let invite = wait(&arx, 5, "invite", |e| if let Event::Invite(s) = e { Some(s.clone()) } else { None });
    assert!(invite.starts_with("rim1:"));
    b.send(Command::AddContact { invite: invite.clone(), text: "hi, it's bob".into() });

    // Alice sees the request and accepts.
    let pending_id = wait(&arx, 20, "auth request", |e| match e {
        Event::Pending(v) if !v.is_empty() => {
            assert_eq!(v[0].nick, "bob");
            assert_eq!(v[0].text, "hi, it's bob");
            Some(v[0].id.clone())
        }
        _ => None,
    });
    a.send(Command::Accept { id: pending_id.clone() });

    // Both see each other online with OS info.
    wait(&brx, 30, "bob sees alice online", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "alice" && c.status == Status::Online && !c.os.is_empty()).map(|_| ()),
        _ => None,
    });
    wait(&arx, 30, "alice sees bob online", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "bob" && c.status == Status::Online).map(|_| ()),
        _ => None,
    });

    // Bob -> Alice encrypted text, delivered.
    b.send(Command::SetStatus(Status::Away));
    let alice_id = wait(&brx, 10, "alice id", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "alice").map(|c| c.id.clone()),
        _ => None,
    });
    b.send(Command::SendText { id: alice_id.clone(), body: "hello over olm".into() });
    wait(&arx, 20, "alice receives text", |e| match e {
        Event::Incoming { text, .. } if text == "hello over olm" => Some(()),
        _ => None,
    });
    wait(&brx, 20, "bob sees delivered", |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.from_me && l.delivery == Delivery::Delivered).map(|_| ()),
        _ => None,
    });
    // Alice sees Bob's Away status.
    wait(&arx, 30, "alice sees bob away", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "bob" && c.status == Status::Away).map(|_| ()),
        _ => None,
    });

    // Reply the other way.
    a.send(Command::SendText { id: pending_id.clone(), body: "hi bob".into() });
    wait(&brx, 20, "bob receives reply", |e| match e {
        Event::Incoming { text, .. } if text == "hi bob" => Some(()),
        _ => None,
    });

    // Restart Alice: wrong passphrase fails, right one restores contacts and history.
    a.send(Command::Shutdown);
    wait(&arx, 10, "alice stopped", |e| matches!(e, Event::Stopped).then_some(()));
    let (_a2, a2rx) = spawn(EngineConfig { dir: da.clone(), passphrase: "wrong".into(), nick: None, port: 0, ..Default::default() });
    wait(&a2rx, 20, "login failure", |e| matches!(e, Event::LoginFailed(_)).then_some(()));
    let (a3, a3rx) = spawn(EngineConfig { dir: da.clone(), passphrase: "pass-a".into(), nick: None, port: 0, ..Default::default() });
    wait(&a3rx, 20, "alice restored contact", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "bob").map(|_| ()),
        _ => None,
    });
    a3.send(Command::OpenChat { id: pending_id.clone() });
    wait(&a3rx, 10, "alice restored history", |e| match e {
        Event::History { lines, .. } if lines.len() == 2 => Some(()),
        _ => None,
    });
    // Session survives restart: bob can still message the restarted alice.
    b.send(Command::SendText { id: alice_id, body: "still there?".into() });
    wait(&a3rx, 40, "alice (restarted) receives", |e| match e {
        Event::Incoming { text, .. } if text == "still there?" => Some(()),
        _ => None,
    });

    a3.send(Command::Shutdown);
    b.send(Command::Shutdown);
    std::thread::sleep(Duration::from_millis(800));
    let _ = std::fs::remove_dir_all(&da);
    let _ = std::fs::remove_dir_all(&db);
}

#[test]
fn invite_is_single_use() {
    let (da, db, dc) = (tmp("a1"), tmp("b1"), tmp("c1"));
    let (a, arx) = start(&da, "alice", "pass-a");
    let (b, brx) = start(&db, "bob", "pass-b");
    let (c, crx) = start(&dc, "carol", "pass-c");
    for rx in [&arx, &brx, &crx] {
        wait(rx, 30, "unlocked", |e| matches!(e, Event::Unlocked { .. }).then_some(()));
    }
    std::thread::sleep(Duration::from_millis(500));
    a.send(Command::NewInvite);
    let invite = wait(&arx, 5, "invite", |e| if let Event::Invite(s) = e { Some(s.clone()) } else { None });

    b.send(Command::AddContact { invite: invite.clone(), text: "bob".into() });
    wait(&arx, 20, "bob's request", |e| match e {
        Event::Pending(v) if v.iter().any(|p| p.nick == "bob") => Some(()),
        _ => None,
    });

    // Carol reuses the same invite: Alice must never see her request.
    c.send(Command::AddContact { invite, text: "carol".into() });
    let end = Instant::now() + Duration::from_secs(12);
    while Instant::now() < end {
        if let Ok(Event::Pending(v)) = arx.recv_timeout(Duration::from_millis(100)) {
            assert!(!v.iter().any(|p| p.nick == "carol"), "reused invite was accepted");
        }
    }
    let _ = brx;
    let _ = crx;
    for h in [a, b, c] {
        h.send(Command::Shutdown);
    }
    std::thread::sleep(Duration::from_millis(800));
    for d in [da, db, dc] {
        let _ = std::fs::remove_dir_all(d);
    }
}
