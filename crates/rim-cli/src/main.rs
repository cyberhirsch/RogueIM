//! rim-cli: headless RogueIM peer. A test partner and the seed of bot mode.
//!
//!   rim-cli --dir ./bot --pass secret --nick echo-bot --invite --accept-all --echo
//!   rim-cli --dir ./b --pass x --nick bob --add rim1:... --say "hello"
//!   rim-cli --dir ./f --pass x --nick friend --invite --accept-all --control ./f.cmd
//!
//! `--control FILE` is polled for appended lines; each line is a command:
//!   say <text>              message the first authorized contact
//!   to <name> <text>        message a contact by name
//!   status <online|ffc|away|na|occupied|dnd|invisible>
//!   away <text>             set the away message
//!   invite                  print a new single-use invite
//!   add <rim1:...> [text]   send an authorization request
//! Incoming events are printed to stdout, one per line.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use rim_core::{spawn, Command, ContactView, DeviceClass, EngineConfig, Event, Status};

fn parse_status(s: &str) -> Option<Status> {
    Some(match s {
        "online" => Status::Online,
        "ffc" => Status::FreeForChat,
        "away" => Status::Away,
        "na" => Status::NotAvailable,
        "occupied" => Status::Occupied,
        "dnd" => Status::DoNotDisturb,
        "invisible" => Status::Invisible,
        _ => return None,
    })
}

fn main() {
    let mut cfg = EngineConfig { dir: PathBuf::from("./rim-cli-profile"), passphrase: "rim".into(), ..Default::default() };
    let (mut invite, mut accept_all, mut echo) = (false, false, false);
    let (mut add, mut say, mut control) = (None::<String>, None::<String>, None::<PathBuf>);
    let mut it = std::env::args().skip(1);
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
            "--invite" => invite = true,
            "--accept-all" => accept_all = true,
            "--echo" => echo = true,
            "--add" => add = it.next(),
            "--say" => say = it.next(),
            "--control" => control = it.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let (h, rx) = spawn(cfg);
    let mut said = false;
    let mut contacts: Vec<ContactView> = vec![];
    // Only commands appended after start are executed.
    let mut offset = control.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);

    loop {
        if let Some(path) = &control {
            for line in read_new_lines(path, &mut offset) {
                run_control(&h, &contacts, line.trim());
            }
        }
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(300)) else { continue };
        match ev {
            Event::Unlocked { nick, fingerprint, os, device_class, .. } => {
                println!("unlocked: {nick} [{os}/{}] fp {fingerprint}", device_class.as_str());
                std::thread::sleep(Duration::from_millis(400)); // listeners up
                if invite {
                    h.send(Command::NewInvite);
                }
                if let Some(inv) = add.take() {
                    h.send(Command::AddContact { invite: inv, text: "hello from rim-cli".into() });
                }
            }
            Event::LoginFailed(e) => {
                eprintln!("login failed: {e}");
                std::process::exit(1);
            }
            Event::Invite(s) => println!("INVITE {s}"),
            Event::Pending(list) => {
                for p in list {
                    println!("auth request from {} ({}): {}", p.nick, p.fingerprint, p.text);
                    if accept_all {
                        h.send(Command::Accept { id: p.id });
                    }
                }
            }
            Event::Contacts(list) => {
                for c in &list {
                    let old = contacts.iter().find(|o| o.id == c.id).map(|o| (o.status, o.awaiting));
                    if old != Some((c.status, c.awaiting)) {
                        println!("status {} = {}{}", c.name, c.status.label(), if c.awaiting { " (awaiting auth)" } else { "" });
                    }
                    if !said && !c.awaiting && c.status != Status::Offline {
                        if let Some(text) = say.clone() {
                            h.send(Command::SendText { id: c.id.clone(), body: text });
                            said = true;
                        }
                    }
                }
                contacts = list;
            }
            Event::Incoming { id, name, text } => {
                println!("<{name}> {text}");
                if echo {
                    h.send(Command::SendText { id, body: format!("echo: {text}") });
                }
            }
            Event::Notice(s) => println!("* {s}"),
            Event::Stopped => break,
            _ => {}
        }
    }
}

fn read_new_lines(path: &PathBuf, offset: &mut u64) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0; // file was truncated
    }
    if len == *offset || f.seek(SeekFrom::Start(*offset)).is_err() {
        return vec![];
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return vec![];
    }
    // Only consume complete lines.
    let Some(end) = buf.rfind('\n') else { return vec![] };
    *offset += (end + 1) as u64;
    buf[..end].lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect()
}

fn run_control(h: &rim_core::EngineHandle, contacts: &[ContactView], line: &str) {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "say" => match contacts.iter().find(|c| !c.awaiting) {
            Some(c) => h.send(Command::SendText { id: c.id.clone(), body: rest.to_string() }),
            None => println!("! no authorized contact"),
        },
        "to" => {
            let (name, text) = rest.split_once(' ').unwrap_or((rest, ""));
            match contacts.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                Some(c) => h.send(Command::SendText { id: c.id.clone(), body: text.to_string() }),
                None => println!("! no contact named {name}"),
            }
        }
        "status" => match parse_status(rest) {
            Some(s) => h.send(Command::SetStatus(s)),
            None => println!("! unknown status {rest}"),
        },
        "away" => h.send(Command::SetAwayMessage(rest.to_string())),
        "invite" => h.send(Command::NewInvite),
        "add" => {
            let (inv, text) = rest.split_once(' ').unwrap_or((rest, "Hi, please add me."));
            h.send(Command::AddContact { invite: inv.to_string(), text: text.to_string() });
        }
        _ => println!("! unknown command {cmd}"),
    }
}
