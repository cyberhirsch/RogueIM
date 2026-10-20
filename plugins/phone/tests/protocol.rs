//! The phone plugin started like RogueIM starts it, with a pretend phone:
//! the code from the sidebar, a message in, a reply out.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
use rim_plugin_sdk::{ToHost, ToPlugin};

#[path = "../src/link.rs"]
#[allow(dead_code)]
mod link;

#[test]
fn a_phone_links_and_chats() {
    let sock = format!("rogueim-test-phone-{}", std::process::id());
    let data = std::env::temp_dir().join(format!("rim-phone-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let listener = ListenerOptions::new().name(sock.clone().to_ns_name::<GenericNamespaced>().unwrap()).create_sync().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-plugin-phone")).env("RIM_PLUGIN_SOCKET", &sock).env("RIM_PLUGIN_DATA", &data).spawn().unwrap();
    let conn = listener.incoming().next().unwrap().unwrap();
    let (recv, mut send) = conn.split();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(recv).lines().map_while(Result::ok) {
            if let Ok(m) = serde_json::from_str::<ToHost>(&l) {
                if tx.send(m).is_err() {
                    break;
                }
            }
        }
    });
    let wait = |what: &str, f: &mut dyn FnMut(&ToHost) -> bool| {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let m = rx.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap_or_else(|_| panic!("no {what}"));
            if f(&m) {
                return m;
            }
        }
    };
    let mut tell = |m: ToPlugin| send.write_all(format!("{}\n", serde_json::to_string(&m).unwrap()).as_bytes()).unwrap();

    // The sidebar shows the code a phone scans.
    let ToHost::View { items } = wait("the code", &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.kind == "qr"))) else { unreachable!() };
    let code = items.iter().find(|i| i.kind == "qr").unwrap().text.clone();
    let parts: Vec<&str> = code.splitn(6, ':').collect();
    assert_eq!(&parts[..2], ["rimphone", "1"]);
    use base64::Engine;
    let key: [u8; 32] = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[4]).unwrap().try_into().unwrap();

    // A stranger without the key gets nowhere.
    let mut bad = TcpStream::connect(("127.0.0.1", parts[3].parse::<u16>().unwrap())).unwrap();
    let (mut up, _) = link::client_handshake(&mut bad, &[9u8; 32]).unwrap();
    bad.write_all(&up.seal(br#"{"t":"hello","device":"Mallory"}"#)).unwrap();
    wait("the wrong key noticed", &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.text.contains("wrong key"))));

    // The phone.
    let mut s = TcpStream::connect(("127.0.0.1", parts[3].parse::<u16>().unwrap())).unwrap();
    let (mut up, mut down) = link::client_handshake(&mut s, &key).unwrap();
    let mut say = |s: &mut TcpStream, v: serde_json::Value| s.write_all(&up.seal(v.to_string().as_bytes())).unwrap();
    say(&mut s, serde_json::json!({"t": "hello", "device": "Pixel 9"}));
    wait("the phone's name", &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.text == "Pixel 9")));
    say(&mut s, serde_json::json!({"t": "msg", "chat": "com.whatsapp|Anna", "app": "WhatsApp", "conv": "Anna", "who": "Anna", "text": "Lunch?", "ts": 1_790_000_000_000i64, "id": "m1"}));
    let ToHost::Incoming { chat, name } = wait("incoming", &mut |m| matches!(m, ToHost::Incoming { .. })) else { unreachable!() };
    assert_eq!((chat.as_str(), name.as_str()), ("com.whatsapp|Anna", "Anna (WhatsApp)"));

    // Opening the chat shows it and tells the phone it was read.
    tell(ToPlugin::Open { chat: chat.clone() });
    wait("the line", &mut |m| matches!(m, ToHost::Lines { lines, .. } if lines.len() == 1 && lines[0].text == "Lunch?"));
    let mut next = |s: &mut TcpStream| -> serde_json::Value {
        loop {
            let mut len = [0u8; 4];
            s.read_exact(&mut len).unwrap();
            let mut f = vec![0u8; u32::from_be_bytes(len) as usize];
            s.read_exact(&mut f).unwrap();
            let v: serde_json::Value = serde_json::from_slice(&down.open(&f).unwrap()).unwrap();
            if v["t"] != "hello" && v["t"] != "ping" {
                return v;
            }
        }
    };
    assert_eq!(next(&mut s), serde_json::json!({"t": "read", "chat": "com.whatsapp|Anna"}));

    // A reply goes to the phone; its answer marks it sent.
    tell(ToPlugin::Send { chat: chat.clone(), text: "Yes, 12:00".into() });
    let reply = next(&mut s);
    assert_eq!((reply["t"].as_str(), reply["text"].as_str()), (Some("reply"), Some("Yes, 12:00")));
    say(&mut s, serde_json::json!({"t": "sent", "chat": chat, "ref": reply["ref"]}));
    wait("sent", &mut |m| matches!(m, ToHost::Lines { lines, .. } if lines.iter().any(|l| l.from_me && l.state == "sent")));

    tell(ToPlugin::Stop);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&data);
}

/// With the Android emulator and the RogueIM Phone app (run by hand, see
/// android/README.md): the code is written to RIM_PHONE_CODE_FILE with the
/// emulator's address for this computer; then an SMS sent to the emulator
/// must arrive here, and a reply must go out through Messages.
#[test]
#[ignore]
fn emulator_round_trip() {
    let code_file = std::env::var("RIM_PHONE_CODE_FILE").expect("RIM_PHONE_CODE_FILE");
    let sock = format!("rogueim-test-phone-emu-{}", std::process::id());
    let data = std::env::temp_dir().join(format!("rim-phone-emu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let listener = ListenerOptions::new().name(sock.clone().to_ns_name::<GenericNamespaced>().unwrap()).create_sync().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-plugin-phone")).env("RIM_PLUGIN_SOCKET", &sock).env("RIM_PLUGIN_DATA", &data).spawn().unwrap();
    let conn = listener.incoming().next().unwrap().unwrap();
    let (recv, mut send) = conn.split();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(recv).lines().map_while(Result::ok) {
            eprintln!("plugin: {}", &l[..l.len().min(300)]);
            if let Ok(m) = serde_json::from_str::<ToHost>(&l) {
                if tx.send(m).is_err() {
                    break;
                }
            }
        }
    });
    let wait = |secs: u64, what: &str, f: &mut dyn FnMut(&ToHost) -> bool| {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let m = rx.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap_or_else(|_| panic!("no {what}"));
            if f(&m) {
                return m;
            }
        }
    };
    let ToHost::View { items } = wait(15, "the code", &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.kind == "qr"))) else { unreachable!() };
    let code = items.iter().find(|i| i.kind == "qr").unwrap().text.clone();
    let p: Vec<&str> = code.splitn(6, ':').collect();
    // 10.0.2.2 is this computer, seen from the emulator.
    std::fs::write(&code_file, format!("rimphone:1:10.0.2.2:{}:{}:{}", p[3], p[4], p[5])).unwrap();
    wait(240, "the phone", &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.text == "connected")));
    eprintln!("PHONE CONNECTED");
    let ToHost::Incoming { chat, name } = wait(240, "an SMS", &mut |m| matches!(m, ToHost::Incoming { .. })) else { unreachable!() };
    eprintln!("INCOMING {chat} {name}");
    let mut tell = |m: ToPlugin| send.write_all(format!("{}\n", serde_json::to_string(&m).unwrap()).as_bytes()).unwrap();
    tell(ToPlugin::Open { chat: chat.clone() });
    tell(ToPlugin::Send { chat: chat.clone(), text: "Reply from RogueIM".into() });
    let ToHost::Lines { lines, .. } = wait(60, "the reply's result", &mut |m| matches!(m, ToHost::Lines { lines, .. } if lines.iter().any(|l| l.from_me && l.state != "sending"))) else { unreachable!() };
    let mine = lines.iter().find(|l| l.from_me).unwrap();
    eprintln!("REPLY {} {}", mine.state, mine.text);
    assert_eq!(mine.state, "sent", "{}", mine.text);
    tell(ToPlugin::Stop);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&data);
}
