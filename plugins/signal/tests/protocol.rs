//! The Signal plugin started like RogueIM starts it.

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
use rim_plugin_sdk::{ToHost, ToPlugin};

struct Run {
    child: std::process::Child,
    rx: std::sync::mpsc::Receiver<ToHost>,
    send: Box<dyn Write>,
    data: std::path::PathBuf,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn start(tag: &str) -> Run {
    let sock = format!("rogueim-test-signal-{tag}-{}", std::process::id());
    let data = std::env::temp_dir().join(format!("rim-signal-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let listener = ListenerOptions::new()
        .name(sock.clone().to_ns_name::<GenericNamespaced>().unwrap())
        .create_sync()
        .unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-plugin-signal"))
        .env("RIM_PLUGIN_SOCKET", &sock)
        .env("RIM_PLUGIN_DATA", &data)
        .spawn()
        .unwrap();
    let conn = listener.incoming().next().unwrap().unwrap();
    let (recv, send) = conn.split();
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
    Run {
        child,
        rx,
        send: Box::new(send),
        data,
    }
}

impl Run {
    fn wait(&self, secs: u64, what: &str, f: &mut dyn FnMut(&ToHost) -> bool) -> ToHost {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let m = self
                .rx
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("no {what}"));
            if f(&m) {
                return m;
            }
        }
    }

    fn tell(&mut self, m: ToPlugin) {
        self.send
            .write_all(format!("{}\n", serde_json::to_string(&m).unwrap()).as_bytes())
            .unwrap();
    }
}

/// Unlinked, it says what it is and waits for "link" before it talks to Signal.
#[test]
fn asks_before_it_links() {
    let mut r = start("ask");
    let ToHost::Hello { id } = r.wait(15, "hello", &mut |_| true) else {
        panic!("hello first")
    };
    assert_eq!(id, "signal");
    let mut net = false;
    let ToHost::Channel {
        level,
        warning,
        files,
        ..
    } = r.wait(15, "channel", &mut |m| matches!(m, ToHost::Channel { .. }))
    else {
        unreachable!()
    };
    assert_eq!(level, "C");
    assert!(files && warning.contains("unofficial"));
    r.wait(30, "the link button", &mut |m| {
        net |= matches!(m, ToHost::Network { .. });
        matches!(m, ToHost::View { items } if items.iter().any(|i| i.id == "link"))
    });
    assert!(!net, "talked to Signal before being asked");
    r.tell(ToPlugin::Stop);
    let end = Instant::now() + Duration::from_secs(10);
    while r.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < end, "did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Talks to Signal: after "link" the code for the phone arrives.
#[test]
#[ignore]
fn signal_hands_out_a_link_code() {
    let mut r = start("live");
    r.wait(
        30,
        "the link button",
        &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.id == "link")),
    );
    r.tell(ToPlugin::Click { id: "link".into() });
    let ToHost::View { items } = r.wait(
        60,
        "the code",
        &mut |m| matches!(m, ToHost::View { items } if items.iter().any(|i| i.kind == "qr")),
    ) else {
        unreachable!()
    };
    let code = &items.iter().find(|i| i.kind == "qr").unwrap().text;
    assert!(code.starts_with("sgnl://linkdevice?"), "{code}");
    r.tell(ToPlugin::Click {
        id: "cancel".into(),
    });
    r.tell(ToPlugin::Stop);
}
