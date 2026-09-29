//! Act as RogueIM: start the plugin, read its first view, click start, and
//! expect it to ask for the focus status.

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};

#[test]
fn pomodoro_talks_the_plugin_protocol() {
    let sock = format!("rogueim-test-pomodoro-{}", std::process::id());
    let listener = ListenerOptions::new().name(sock.clone().to_ns_name::<GenericNamespaced>().unwrap()).create_sync().unwrap();
    let data = std::env::temp_dir().join(format!("rim-plugin-test-{}", std::process::id()));
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-plugin-pomodoro"))
        .env("RIM_PLUGIN_SOCKET", &sock)
        .env("RIM_PLUGIN_DATA", &data)
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    let conn = listener.incoming().next().unwrap().unwrap();
    eprintln!("connected after {:?}", t0.elapsed());
    let (recv, mut send) = conn.split();
    // Read on a thread like the real host does, so the plugin never blocks writing.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(recv).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let next = || serde_json::from_str::<serde_json::Value>(&rx.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
    assert_eq!(next()["t"], "hello");
    let view = next();
    assert_eq!(view["t"], "view");
    assert!(view["items"].as_array().unwrap().iter().any(|i| i["id"] == "start"));
    for id in ["long", "every", "sound"] {
        assert!(view["items"].as_array().unwrap().iter().any(|i| i["id"] == id), "view lacks {id}");
    }
    let sound = view["items"].as_array().unwrap().iter().find(|i| i["id"] == "sound").unwrap();
    assert_eq!(sound["checked"], true, "sound at phase end is on by default");
    send.write_all(b"{\"t\":\"click\",\"id\":\"start\"}\n").unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let mut got_status = false;
    while Instant::now() < end {
        let m = next();
        if m["t"] == "status" {
            assert_eq!(m["status"], "occupied");
            assert!(m["away"].as_str().unwrap().starts_with("Focusing"));
            got_status = true;
            break;
        }
    }
    assert!(got_status, "plugin never asked for the focus status");
    eprintln!("status after {:?}", t0.elapsed());
    send.write_all(b"{\"t\":\"stop\"}\n").unwrap();
    let _ = child.wait();
    eprintln!("exited after {:?}", t0.elapsed());
}
