//! rim-cli: headless RogueIM peer. Handy as a test partner and the seed of bot mode.
//!
//!   rim-cli --dir ./bot --pass secret --nick echo-bot --invite --accept-all --echo
//!   rim-cli --dir ./b --pass x --nick bob --add rim1:... --say "hello"

use std::path::PathBuf;
use std::time::Duration;

use rim_core::{spawn, Command, EngineConfig, Event, Status};

fn main() {
    let mut dir = PathBuf::from("./rim-cli-profile");
    let (mut pass, mut nick) = (String::from("rim"), None::<String>);
    let (mut invite, mut accept_all, mut echo) = (false, false, false);
    let (mut add, mut say, mut port) = (None::<String>, None::<String>, 0u16);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = it.next().map(PathBuf::from).unwrap_or(dir),
            "--pass" => pass = it.next().unwrap_or(pass),
            "--nick" => nick = it.next(),
            "--port" => port = it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            "--invite" => invite = true,
            "--accept-all" => accept_all = true,
            "--echo" => echo = true,
            "--add" => add = it.next(),
            "--say" => say = it.next(),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let (h, rx) = spawn(EngineConfig { dir, passphrase: pass, nick, port });
    let mut said = false;
    loop {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) else { continue };
        match ev {
            Event::Unlocked { nick, fingerprint, os, device_class, .. } => {
                println!("unlocked: {nick} [{os}/{}] fp {fingerprint}", device_class.as_str());
                if invite {
                    std::thread::sleep(Duration::from_millis(400)); // listeners up
                    h.send(Command::NewInvite);
                }
                if let Some(inv) = add.take() {
                    std::thread::sleep(Duration::from_millis(400));
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
                    println!("  {:<16} {:<13} {}/{}{}", c.name, c.status.label(), c.os, c.device_class.as_str(), if c.awaiting { " (awaiting auth)" } else { "" });
                    if !said && !c.awaiting && c.status != Status::Offline {
                        if let Some(text) = say.clone() {
                            h.send(Command::SendText { id: c.id.clone(), body: text });
                            said = true;
                        }
                    }
                }
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
