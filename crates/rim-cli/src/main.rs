//! rim-cli: headless RogueIM peer — test partner, always-on node, and bot.
//!
//! Modes
//!   rim-cli --dir ./p --pass secret --nick echo-bot --invite --accept-all --echo
//!   rim-cli --node --dir /var/lib/rogueim --pass secret        (relay / DHT / mailbox helper)
//!   rim-cli --bot --api mybot --allow alice,bob --dir ./bot --pass x --nick backupbot
//!   rim-cli send --dir ./bot --pass x --to alice "backup finished"
//!   some-command | rim-cli --dir ./bot --pass x --stdin-to alice
//!
//! Local API (`--api NAME`): JSON lines over a local socket / named pipe
//! `rogueim-NAME` (never a network port).
//!   -> {"cmd":"send","to":"alice","text":"hi"}
//!   -> {"cmd":"contacts"}
//!   -> {"cmd":"status","status":"away"}
//!   <- {"event":"message","from":"alice","text":"hi","urgent":false}
//!   <- {"event":"command","from":"alice","text":"!deploy"}
//!   <- {"event":"contacts","list":[{"name":"alice","status":"Online"}]}
//!
//! `--control FILE` is polled for appended lines (for scripted tests):
//!   say <text> | to <name> <text> | status <s> | away <text> | invite | add <rim2:...> [text]

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rim_core::engine::StartMode;
use rim_core::{spawn, Command, ContactView, DeviceClass, EngineConfig, EngineHandle, Event, Status};
use serde_json::json;

fn parse_status(s: &str) -> Option<Status> {
    Some(match s.to_lowercase().as_str() {
        "online" => Status::Online,
        "ffc" | "free" => Status::FreeForChat,
        "away" => Status::Away,
        "na" | "n/a" => Status::NotAvailable,
        "occupied" | "busy" => Status::Occupied,
        "dnd" => Status::DoNotDisturb,
        "invisible" => Status::Invisible,
        _ => return None,
    })
}

type Contacts = Arc<Mutex<Vec<ContactView>>>;
type Clients = Arc<Mutex<Vec<Box<dyn Write + Send>>>>;

struct Opts {
    invite: bool,
    accept_all: bool,
    echo: bool,
    add: Option<String>,
    say: Option<String>,
    control: Option<PathBuf>,
    api: Option<String>,
    allow: Vec<String>,
    stdin_to: Option<String>,
    send_to: Option<String>,
    send_text: Option<String>,
}

fn main() {
    let mut cfg = EngineConfig { dir: PathBuf::from("./rim-cli-profile"), passphrase: "rim".into(), ..Default::default() };
    let mut o = Opts { invite: false, accept_all: false, echo: false, add: None, say: None, control: None, api: None, allow: vec![], stdin_to: None, send_to: None, send_text: None };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let one_shot_send = args.first().map(|a| a == "send").unwrap_or(false);
    if one_shot_send {
        args.remove(0);
    }
    let mut it = args.into_iter();
    let mut rest = vec![];
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => cfg.dir = it.next().map(PathBuf::from).unwrap_or(cfg.dir),
            "--pass" => cfg.passphrase = it.next().unwrap_or_default(),
            "--nick" => cfg.nick = it.next(),
            "--port" => cfg.port = it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            "--os" => cfg.os_override = it.next(),
            "--device" => {
                cfg.device_override = match it.next().as_deref() {
                    Some("laptop") => Some(DeviceClass::Laptop),
                    Some("mobile") => Some(DeviceClass::Mobile),
                    _ => Some(DeviceClass::Desktop),
                }
            }
            "--node" => {
                cfg.node = true;
                if cfg.nick.is_none() {
                    cfg.nick = Some("rim-node".into());
                }
            }
            "--bot" => cfg.bot = true,
            "--lan-only" => cfg.lan_only = Some(true),
            "--no-nostr" => cfg.relays = Some(vec![]),
            "--link" => cfg.mode = StartMode::Link,
            "--invite" => o.invite = true,
            "--accept-all" => o.accept_all = true,
            "--echo" => o.echo = true,
            "--add" => o.add = it.next(),
            "--say" => o.say = it.next(),
            "--control" => o.control = it.next().map(PathBuf::from),
            "--api" => o.api = it.next(),
            "--allow" => o.allow = it.next().unwrap_or_default().split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
            "--stdin-to" => o.stdin_to = it.next(),
            "--to" => o.send_to = it.next(),
            "--help" | "-h" => {
                println!("{}", include_str!("main.rs").lines().take_while(|l| l.starts_with("//!")).map(|l| l.trim_start_matches("//!").trim_start_matches(' ')).collect::<Vec<_>>().join("\n"));
                return;
            }
            other if one_shot_send && !other.starts_with("--") => rest.push(other.to_string()),
            other => {
                eprintln!("unknown argument {other} (try --help)");
                std::process::exit(2);
            }
        }
    }
    if one_shot_send {
        o.send_text = Some(rest.join(" "));
        if o.send_to.is_none() || o.send_text.as_deref().unwrap_or("").is_empty() {
            eprintln!("usage: rim-cli send --dir D --pass P --to NAME TEXT");
            std::process::exit(2);
        }
    }

    let (h, rx) = spawn(cfg);
    let contacts: Contacts = Arc::new(Mutex::new(vec![]));
    let clients: Clients = Arc::new(Mutex::new(vec![]));
    if let Some(name) = &o.api {
        start_api(name, h.clone(), contacts.clone(), clients.clone());
    }
    if let Some(to) = o.stdin_to.clone() {
        let (h2, c2) = (h.clone(), contacts.clone());
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                send_by_name(&h2, &c2, &to, &line);
            }
        });
    }

    let started = Instant::now();
    let mut said = false;
    let mut sent_once: Option<u64> = None;
    let mut offset = o.control.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);
    loop {
        if let Some(path) = &o.control {
            for line in read_new_lines(path, &mut offset) {
                run_control(&h, &contacts, line.trim());
            }
        }
        if o.send_text.is_some() && started.elapsed() > Duration::from_secs(90) {
            eprintln!("gave up waiting for delivery");
            std::process::exit(1);
        }
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(300)) else { continue };
        match ev {
            Event::Unlocked { nick, fingerprint, os, device_class, .. } => {
                println!("unlocked: {nick} [{os}/{}] fp {fingerprint}", device_class.as_str());
                std::thread::sleep(Duration::from_millis(400));
                if o.invite {
                    h.send(Command::NewInvite { uses: Some(1), ttl_secs: None, label: "rim-cli".into() });
                }
                if let Some(inv) = o.add.take() {
                    h.send(Command::AddContact { invite: inv, text: "hello from rim-cli".into() });
                }
                h.send(Command::NetInfo);
            }
            Event::RecoveryKey(w) => println!("RECOVERY KEY (write it down): {w}"),
            Event::LinkCode { code, words } => println!("LINK {code}\nwords: {words}"),
            Event::Linked => println!("linked to the account"),
            Event::LoginFailed(e) => {
                eprintln!("login failed: {e}");
                std::process::exit(1);
            }
            Event::Invite(s) => println!("INVITE {s}"),
            Event::Net(n) => {
                println!("peer id: {}", n.peer_id);
                for a in n.listen.iter().chain(n.external.iter()) {
                    println!("listening: {a}/p2p/{}", n.peer_id);
                }
                println!("nat: {} · connected peers: {}", n.nat, n.connected_peers);
                for a in &n.observed {
                    println!("seen from outside: {a}");
                }
                for a in &n.circuits {
                    println!("relay circuit: {a}");
                }
                for p in &n.peers {
                    println!("peer: {p}");
                }
            }
            Event::Pending(list) => {
                for p in list {
                    println!("auth request from {} ({}): {}", p.nick, p.fingerprint, p.text);
                    if o.accept_all {
                        h.send(Command::Accept { id: p.id });
                    }
                }
            }
            Event::Contacts(list) => {
                {
                    let mut c = contacts.lock().unwrap();
                    for v in &list {
                        let old = c.iter().find(|x| x.id == v.id).map(|x| (x.status, x.awaiting));
                        if old != Some((v.status, v.awaiting)) {
                            println!("status {} = {}{}", v.name, v.status.label(), if v.awaiting { " (awaiting auth)" } else { "" });
                        }
                    }
                    *c = list.clone();
                }
                broadcast(&clients, json!({"event":"contacts","list": list.iter().map(|c| json!({"name": c.name, "status": c.status.label()})).collect::<Vec<_>>()}));
                if !said {
                    if let Some(text) = o.say.clone() {
                        if let Some(c) = list.iter().find(|c| !c.awaiting && c.status != Status::Offline) {
                            h.send(Command::SendText { id: c.id.clone(), body: text, reply_to: None, urgent: false });
                            said = true;
                        }
                    }
                }
                if let (Some(to), Some(text), None) = (&o.send_to, &o.send_text, sent_once) {
                    if list.iter().any(|c| c.name.eq_ignore_ascii_case(to) && !c.awaiting) {
                        send_by_name(&h, &contacts, to, text);
                        sent_once = Some(0);
                    }
                }
            }
            Event::History { lines, .. } if o.send_text.is_some() && sent_once.is_some() => {
                if let Some(l) = lines.iter().rev().find(|l| l.from_me && Some(&l.text) == o.send_text.as_ref()) {
                    use rim_core::Delivery::*;
                    match l.delivery {
                        Delivered | Read => {
                            println!("delivered");
                            h.send(Command::Shutdown);
                        }
                        Stored => {
                            println!("stored in the network (recipient offline)");
                            h.send(Command::Shutdown);
                        }
                        _ => {}
                    }
                }
            }
            Event::Incoming { id, name, text, urgent } => {
                println!("<{name}> {text}");
                let allowed = o.allow.is_empty() || o.allow.contains(&name.to_lowercase());
                if let Some(cmd) = text.strip_prefix('!') {
                    if !allowed {
                        h.send(Command::SendText { id, body: "not allowed".into(), reply_to: None, urgent: false });
                        continue;
                    }
                    match cmd.trim() {
                        "ping" => h.send(Command::SendText { id, body: "pong".into(), reply_to: None, urgent: false }),
                        "uptime" => h.send(Command::SendText { id, body: format!("up {} min", started.elapsed().as_secs() / 60), reply_to: None, urgent: false }),
                        _ => broadcast(&clients, json!({"event":"command","from": name, "text": text})),
                    }
                } else {
                    broadcast(&clients, json!({"event":"message","from": name, "text": text, "urgent": urgent}));
                    if o.echo {
                        h.send(Command::SendText { id, body: format!("echo: {text}"), reply_to: None, urgent: false });
                    }
                }
            }
            Event::Notice(s) => println!("* {s}"),
            Event::Locked | Event::Wiped => println!("* this device was locked/wiped remotely"),
            Event::Stopped => break,
            _ => {}
        }
    }
}

fn send_by_name(h: &EngineHandle, contacts: &Contacts, to: &str, text: &str) {
    let id = contacts.lock().unwrap().iter().find(|c| c.name.eq_ignore_ascii_case(to)).map(|c| c.id.clone());
    match id {
        Some(id) => h.send(Command::SendText { id, body: text.to_string(), reply_to: None, urgent: false }),
        None => eprintln!("no contact named {to}"),
    }
}

fn broadcast(clients: &Clients, v: serde_json::Value) {
    let line = format!("{v}\n");
    clients.lock().unwrap().retain_mut(|w| w.write_all(line.as_bytes()).and_then(|_| w.flush()).is_ok());
}

fn start_api(name: &str, h: EngineHandle, contacts: Contacts, clients: Clients) {
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
    let sock = format!("rogueim-{name}");
    let Ok(ns) = sock.clone().to_ns_name::<GenericNamespaced>() else {
        eprintln!("local API names are not supported here");
        return;
    };
    let listener = match ListenerOptions::new().name(ns).create_sync() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not start local API {sock}: {e}");
            return;
        }
    };
    println!("local API: {sock}");
    std::thread::spawn(move || {
        for conn in listener.incoming().filter_map(Result::ok) {
            let (recv, send) = conn.split();
            clients.lock().unwrap().push(Box::new(send));
            let (h, contacts, clients) = (h.clone(), contacts.clone(), clients.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(recv).lines().map_while(Result::ok) {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                    match v["cmd"].as_str() {
                        Some("send") => send_by_name(&h, &contacts, v["to"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or("")),
                        Some("status") => {
                            if let Some(s) = v["status"].as_str().and_then(parse_status) {
                                h.send(Command::SetStatus(s));
                            }
                        }
                        Some("contacts") => {
                            let list: Vec<_> = contacts.lock().unwrap().iter().map(|c| json!({"name": c.name, "status": c.status.label()})).collect();
                            broadcast(&clients, json!({"event":"contacts","list": list}));
                        }
                        _ => {}
                    }
                }
            });
        }
    });
}

fn read_new_lines(path: &PathBuf, offset: &mut u64) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0;
    }
    if len == *offset || f.seek(SeekFrom::Start(*offset)).is_err() {
        return vec![];
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return vec![];
    }
    let Some(end) = buf.rfind('\n') else { return vec![] };
    *offset += (end + 1) as u64;
    buf[..end].lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect()
}

fn run_control(h: &EngineHandle, contacts: &Contacts, line: &str) {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "say" => {
            let id = contacts.lock().unwrap().iter().find(|c| !c.awaiting).map(|c| c.id.clone());
            match id {
                Some(id) => h.send(Command::SendText { id, body: rest.to_string(), reply_to: None, urgent: false }),
                None => println!("! no authorized contact"),
            }
        }
        "to" => {
            let (name, text) = rest.split_once(' ').unwrap_or((rest, ""));
            send_by_name(h, contacts, name, text);
        }
        "status" => match parse_status(rest) {
            Some(s) => h.send(Command::SetStatus(s)),
            None => println!("! unknown status {rest}"),
        },
        "away" => h.send(Command::SetAwayMessage(rest.to_string())),
        "net" => h.send(Command::NetInfo),
        "invite" => h.send(Command::NewInvite { uses: Some(1), ttl_secs: None, label: String::new() }),
        "add" => {
            let (inv, text) = rest.split_once(' ').unwrap_or((rest, "Hi, please add me."));
            h.send(Command::AddContact { invite: inv.to_string(), text: text.to_string() });
        }
        _ => println!("! unknown command {cmd}"),
    }
}
