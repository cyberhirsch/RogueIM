//! Start the player plugin like RogueIM does and read its first view.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};

#[test]
fn player_reports_its_view() {
    let sock = format!("rogueim-test-player-{}", std::process::id());
    let listener = ListenerOptions::new().name(sock.clone().to_ns_name::<GenericNamespaced>().unwrap()).create_sync().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-plugin-player")).env("RIM_PLUGIN_SOCKET", &sock).env("RIM_PLUGIN_DATA", std::env::temp_dir()).spawn().unwrap();
    let conn = listener.incoming().next().unwrap().unwrap();
    let (recv, mut send) = conn.split();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(recv).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let view = loop {
        let l = rx.recv_timeout(Duration::from_secs(15)).expect("a view");
        if l.contains("\"view\"") {
            break l;
        }
    };
    eprintln!("player view: {view}");
    assert!(view.contains("toggle"));
    send.write_all(b"{\"t\":\"stop\"}\n").unwrap();
    let _ = child.wait();
}
