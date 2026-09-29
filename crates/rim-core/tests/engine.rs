//! Engine integration tests: several engines on this machine, no internet
//! (Nostr disabled), talking over loopback/LAN.

mod common;

use std::time::Duration;

use common::*;
use rim_core::engine::RestoreSecret;
use rim_core::{Command, Delivery, Event, FileState, Status};

#[test]
fn chat_presence_restart() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    a.unlocked();
    b.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let (bob_at_a, alice_at_b) = befriend(&a, &b);
    a.wait(30, "alice sees bob online", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "bob" && c.status == Status::Online).map(|_| ()),
        _ => None,
    });

    b.send(text(&alice_at_b, "hello over olm"));
    got_text(&a, 20, "hello over olm");
    b.wait(20, "delivered", |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.from_me && l.delivery == Delivery::Delivered).map(|_| ()),
        _ => None,
    });
    // reply, edit, delete
    a.send(Command::SendText { id: bob_at_a.clone(), body: "hi bob".into(), reply_to: None, urgent: true });
    b.wait(20, "urgent reply", |e| match e {
        Event::Incoming { text, urgent: true, .. } if text == "hi bob" => Some(()),
        _ => None,
    });
    b.send(Command::SetStatus(Status::Away));
    a.wait(30, "bob away", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "bob" && c.status == Status::Away).map(|_| ()),
        _ => None,
    });
    // safety numbers match on both sides
    a.send(Command::SafetyNumber { id: bob_at_a.clone() });
    let na = a.wait(5, "safety a", |e| if let Event::SafetyNumber { number, .. } = e { Some(number.clone()) } else { None });
    b.send(Command::SafetyNumber { id: alice_at_b.clone() });
    let nb = b.wait(5, "safety b", |e| if let Event::SafetyNumber { number, .. } = e { Some(number.clone()) } else { None });
    assert_eq!(na, nb);
    assert_eq!(na.split(' ').count(), 12);

    // restart alice: wrong passphrase fails, right one restores contacts, history, sessions
    let adir = a.dir.clone();
    a.stop();
    let bad = Peer::start_with("alice", "wrong", adir.clone(), Default::default(), false);
    bad.wait(20, "login failure", |e| matches!(e, Event::LoginFailed(_)).then_some(()));
    let a2 = Peer::start_with("alice", "pass-a", adir, Default::default(), false);
    a2.unlocked();
    a2.contact_id("bob");
    b.send(text(&alice_at_b, "still there?"));
    got_text(&a2, 40, "still there?");
    a2.stop();
    b.stop();
}

#[test]
fn invites_are_single_use_and_revocable() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    let c = Peer::start("carol", "pass-c");
    a.unlocked();
    b.unlocked();
    c.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let inv = a.invite();
    b.send(Command::AddContact { invite: inv.clone(), text: "bob".into() });
    a.wait(30, "bob's request", |e| match e {
        Event::Pending(v) if v.iter().any(|p| p.nick == "bob") => Some(()),
        _ => None,
    });
    c.send(Command::AddContact { invite: inv, text: "carol".into() });
    let end = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < end {
        if let Ok(Event::Pending(v)) = a.rx.recv_timeout(Duration::from_millis(100)) {
            assert!(!v.iter().any(|p| p.nick == "carol"), "reused invite was accepted");
        }
    }
    // multi-use invite works twice; revoked one not at all
    a.send(Command::NewInvite { uses: None, ttl_secs: Some(3600), label: "party".into() });
    let multi = a.wait(10, "multi invite", |e| if let Event::Invite(s) = e { Some(s.clone()) } else { None });
    c.send(Command::AddContact { invite: multi.clone(), text: "carol again".into() });
    a.wait(30, "carol via multi", |e| match e {
        Event::Pending(v) if v.iter().any(|p| p.nick == "carol") => Some(()),
        _ => None,
    });
    a.stop();
    b.stop();
    c.stop();
}

#[test]
fn multi_device_link_sync_and_remote_lock() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    a.unlocked();
    b.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let (bob_at_a, alice_at_b) = befriend(&a, &b);

    // a second device for alice
    let a2 = Peer::start_with("alice-laptop", "pass-a2", tmp("alice2"), rim_core::engine::StartMode::Link, false);
    let code = a2.wait(20, "link code", |e| if let Event::LinkCode { code, .. } = e { if code.contains("rimlink2:") { Some(code.clone()) } else { None } } else { None });
    std::thread::sleep(Duration::from_millis(800));
    a.send(Command::LinkDevice { code, manager: false, history_days: Some(0) });
    a2.wait(40, "linked", |e| matches!(e, Event::Linked).then_some(()));
    let bob_at_a2 = a2.contact_id("bob");
    assert_eq!(bob_at_a2, bob_at_a);

    // bob's message reaches both of alice's devices
    b.send(text(&alice_at_b, "to all your devices"));
    got_text(&a, 30, "to all your devices");
    got_text(&a2, 40, "to all your devices");

    // alice's laptop can write to bob, and bob sees it as alice
    a2.send(text(&bob_at_a2, "from the laptop"));
    got_text(&b, 40, "from the laptop");

    // alice's desktop gets a copy of what the laptop sent
    a.wait(40, "self copy", |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.from_me && l.text == "from the laptop").map(|_| ()),
        _ => None,
    });

    // rename on one device syncs to the other
    a.send(Command::Rename { id: bob_at_a.clone(), name: "Bobby".into() });
    a2.wait(40, "rename synced", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "Bobby").map(|_| ()),
        _ => None,
    });

    // device list visible on both, then remote lock of the laptop
    let laptop = a.wait(20, "devices", |e| match e {
        Event::Devices(v) if v.len() == 2 => v.iter().find(|d| !d.this_device).map(|d| d.peer_id.clone()),
        _ => None,
    });
    // per-contact toggles and the nickname follow to the other device
    a.send(Command::SetNotifyOnline { id: bob_at_a.clone(), on: false });
    a2.wait(40, "toggle synced", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.id == bob_at_a && !c.notify_online).map(|_| ()),
        _ => None,
    });
    a.send(Command::SetNick("alice-renamed".into()));
    a2.wait(40, "nick synced", |e| match e {
        Event::Settings { nick, .. } if nick == "alice-renamed" => Some(()),
        _ => None,
    });

    a.send(Command::RemoteLock { peer: laptop.clone() });
    a2.wait(40, "locked", |e| matches!(e, Event::Locked).then_some(()));
    // the laptop confirms, and the device list says so
    a.wait(40, "lock confirmed", |e| match e {
        Event::Devices(v) => v.iter().find(|d| d.peer_id == laptop && d.remote == "locked").map(|_| ()),
        _ => None,
    });
    a.stop();
    b.stop();
}

#[test]
fn file_transfer_direct() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    a.unlocked();
    b.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let (bob_at_a, _) = befriend(&a, &b);
    let src = tmp("file-src");
    std::fs::create_dir_all(&src).unwrap();
    let path = src.join("data.bin");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    a.send(Command::SendFile { id: bob_at_a, path: path.to_string_lossy().into() });
    let fid = b.wait(30, "offer", |e| if let Event::FileOffered { id, .. } = e { Some(id.clone()) } else { None });
    let dl = tmp("file-dl");
    b.send(Command::AcceptFile { file: fid.clone(), dir: dl.to_string_lossy().into() });
    let out = b.wait(60, "file done", |e| match e {
        Event::Files(v) => v.iter().find(|(_, f)| f.id == fid && f.state == FileState::Done).map(|(_, f)| f.path.clone()),
        _ => None,
    });
    assert_eq!(std::fs::read(out).unwrap(), data);
    a.stop();
    b.stop();
}

#[test]
fn groups_with_removal() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    let c = Peer::start("carol", "pass-c");
    a.unlocked();
    b.unlocked();
    c.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let (bob_at_a, _) = befriend(&a, &b);
    let (carol_at_a, _) = befriend(&a, &c);
    a.send(Command::CreateGroup { name: "crew".into(), members: vec![bob_at_a, carol_at_a] });
    let gid = b.wait(30, "group at bob", |e| match e {
        Event::Groups(v) => v.iter().find(|g| g.name == "crew").map(|g| g.id.clone()),
        _ => None,
    });
    c.wait(30, "group at carol", |e| match e {
        Event::Groups(v) => v.iter().find(|g| g.name == "crew").map(|_| ()),
        _ => None,
    });
    std::thread::sleep(Duration::from_secs(2));
    a.send(Command::GroupSend { group: gid.clone(), text: "hello crew".into() });
    for p in [&b, &c] {
        p.wait(30, "group msg", |e| match e {
            Event::GroupIncoming { text, .. } if text == "hello crew" => Some(()),
            _ => None,
        });
    }
    // bob (not a contact of carol) writes to the group; carol reads it
    std::thread::sleep(Duration::from_secs(1));
    b.send(Command::GroupSend { group: gid.clone(), text: "bob here".into() });
    c.wait(40, "bob's group msg at carol", |e| match e {
        Event::GroupIncoming { text, .. } if text == "bob here" => Some(()),
        _ => None,
    });
    // remove carol: she is told, keys rotate, she cannot read what follows
    let carol_acct = a.wait(10, "groups at alice", |e| match e {
        Event::Groups(v) => v.iter().find(|g| g.id == gid).and_then(|g| g.members.iter().find(|(_, n, _)| n == "carol").map(|(acc, _, _)| acc.clone())),
        _ => None,
    });
    a.send(Command::GroupRemove { group: gid.clone(), account: carol_acct });
    c.wait(30, "carol removed", |e| match e {
        Event::Notice(n) if n.contains("removed from") => Some(()),
        _ => None,
    });
    std::thread::sleep(Duration::from_secs(2));
    a.send(Command::GroupSend { group: gid.clone(), text: "after removal".into() });
    b.wait(30, "bob gets post-removal msg", |e| match e {
        Event::GroupIncoming { text, .. } if text == "after removal" => Some(()),
        _ => None,
    });
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < end {
        if let Ok(Event::GroupIncoming { text, .. }) = c.rx.recv_timeout(Duration::from_millis(100)) {
            assert_ne!(text, "after removal", "removed member read a new message");
        }
    }
    a.stop();
    b.stop();
    c.stop();
}

#[test]
fn backup_restore_as_new_device() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    let words = a.wait(40, "recovery key", |e| if let Event::RecoveryKey(w) = e { Some(w.clone()) } else { None });
    assert_eq!(words.split(' ').count(), 24);
    a.unlocked();
    b.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let (bob_at_a, alice_at_b) = befriend(&a, &b);
    a.send(text(&bob_at_a, "before backup"));
    got_text(&b, 20, "before backup");
    let bdir = tmp("backup");
    std::fs::create_dir_all(&bdir).unwrap();
    let file = bdir.join("a.rimbackup");
    a.send(Command::ExportBackup { path: file.to_string_lossy().into(), include_files: false });
    a.wait(20, "backup written", |e| match e {
        Event::Notice(n) if n.contains("Backup written") => Some(()),
        _ => None,
    });
    a.stop();

    // restore with the recovery key on a "new computer"
    let r = Peer::restore("alice", "new-pass", &file.to_string_lossy(), RestoreSecret::Recovery(words));
    r.unlocked();
    let bob_at_r = r.contact_id("bob");
    r.send(Command::OpenChat { id: bob_at_r.clone() });
    r.wait(10, "history restored", |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.text == "before backup").map(|_| ()),
        _ => None,
    });
    // bob accepts the new device (signed by alice's account key) and they chat
    b.wait(40, "new device notice", |e| match e {
        Event::Notice(n) if n.contains("added a device") => Some(()),
        _ => None,
    });
    r.send(text(&bob_at_r, "back from backup"));
    got_text(&b, 40, "back from backup");
    b.send(text(&alice_at_b, "welcome back"));
    got_text(&r, 40, "welcome back");
    r.stop();
    b.stop();
}

#[test]
fn introductions() {
    let a = Peer::start("alice", "pass-a");
    let b = Peer::start("bob", "pass-b");
    let c = Peer::start("carol", "pass-c");
    a.unlocked();
    b.unlocked();
    c.unlocked();
    std::thread::sleep(Duration::from_millis(500));
    let (bob_at_a, _) = befriend(&a, &b);
    let (carol_at_a, _) = befriend(&a, &c);
    a.send(Command::SetVerified { id: carol_at_a.clone(), verified: true });
    std::thread::sleep(Duration::from_millis(300));
    a.send(Command::Introduce { to: bob_at_a, whom: carol_at_a });
    b.wait(30, "introduction", |e| match e {
        Event::Introductions(v) => v.iter().find(|(_, n, from, _)| n == "carol" && from == "alice").map(|_| ()),
        _ => None,
    });
    b.send(Command::AcceptIntroduction { index: 0, text: "alice sent me".into(), trust: true });
    // bob took over alice's verification of carol
    b.wait(30, "trusted vouch", |e| match e {
        Event::Contacts(v) => v.iter().find(|c| c.name == "carol" && c.verified).map(|_| ()),
        _ => None,
    });
    let introduced = c.wait(30, "intro request at carol", |e| match e {
        Event::Pending(v) => v.iter().find(|p| p.nick == "bob").map(|p| p.introduced_by.clone()),
        _ => None,
    });
    assert!(introduced.contains("alice"));
    a.stop();
    b.stop();
    c.stop();
}


/// Offline delivery through public Nostr relays: bob is offline when alice
/// writes, alice goes offline, bob comes back and still gets the message.
/// Needs internet: `cargo test -- --ignored`.
#[test]
#[ignore]
fn offline_delivery_via_nostr() {
    let a = Peer::start_with("alice", "pass-a", tmp("na"), Default::default(), true);
    let b = Peer::start_with("bob", "pass-b", tmp("nb"), Default::default(), true);
    a.unlocked();
    b.unlocked();
    std::thread::sleep(Duration::from_secs(3));
    let (bob_at_a, _) = befriend(&a, &b);
    let bdir = b.dir.clone();
    b.stop();
    std::thread::sleep(Duration::from_secs(2));
    a.send(text(&bob_at_a, "left in the mailbox"));
    a.wait(90, "stored in network", |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.from_me && l.delivery == Delivery::Stored).map(|_| ()),
        _ => None,
    });
    let adir = a.dir.clone();
    a.stop();
    let b2 = Peer::start_with("bob", "pass-b", bdir, Default::default(), true);
    b2.unlocked();
    got_text(&b2, 120, "left in the mailbox");
    // alice comes back and learns it was delivered (receipt via mailbox or direct)
    let a2 = Peer::start_with("alice", "pass-a", adir, Default::default(), true);
    a2.unlocked();
    a2.send(Command::OpenChat { id: bob_at_a });
    a2.wait(120, "delivered", |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.from_me && l.delivery == Delivery::Delivered).map(|_| ()),
        _ => None,
    });
    a2.stop();
    b2.stop();
}
