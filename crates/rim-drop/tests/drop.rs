//! The drop server started like on the Pi, with the app's upload against it.
#![cfg(all(feature = "server", feature = "client"))]

use std::io::Read;
use std::time::{Duration, Instant};

const TOKEN: &str = "test-token-0123456789abcdefghij";

struct Server {
    child: std::process::Child,
    dir: std::path::PathBuf,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(tag: &str) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("rim-drop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-drop"))
        .env("RIM_DROP_TOKEN", TOKEN)
        .args(["--dir", dir.to_str().unwrap(), "--listen", &format!("127.0.0.1:{port}"), "--quota-gb", "1"])
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let end = Instant::now() + Duration::from_secs(10);
    while ureq::get(format!("{base}/health")).call().is_err() {
        assert!(Instant::now() < end, "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, dir, base }
}

fn get(url: &str) -> (u16, Vec<u8>) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = agent.get(url).call().unwrap();
    let mut v = vec![];
    r.body_mut().as_reader().read_to_end(&mut v).unwrap();
    (r.status().as_u16(), v)
}

#[test]
fn a_file_goes_up_encrypted_and_comes_down_once() {
    let s = start("updown");
    let path = s.dir.join("../rim-drop-src.bin");
    let data: Vec<u8> = (0..(3 * rim_drop::CHUNK + 1234)).map(|i| (i * 31 % 253) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    let mut seen = 0;
    let up = rim_drop::client::upload(&s.base, TOKEN, "https://page/f/", &path, 3600, 1, |sent, _| seen = sent).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(seen > data.len() as u64);
    // The link: page, then after '#' the file URL and the key.
    let (file_url, key) = up.link.split_once('#').unwrap().1.split_once(',').unwrap();
    assert_eq!(file_url, up.url);
    let key: [u8; 32] = rim_drop::B64.decode(key).unwrap().try_into().unwrap();
    // What the drop keeps is not the file.
    let stored = std::fs::read(s.dir.join(format!("{}.bin", up.id))).unwrap();
    assert!(!stored.windows(64).any(|w| w == &data[1000..1064]));
    let (status, body) = get(&up.url);
    assert_eq!(status, 200);
    let (meta, plain) = rim_drop::open(&key, &body).unwrap();
    assert_eq!(meta.name, "rim-drop-src.bin");
    assert_eq!(plain, data);
    // One download allowed.
    assert_eq!(get(&up.url).0, 410);
}

#[test]
fn uploads_need_the_token() {
    let s = start("token");
    assert!(rim_drop::client::check(&s.base, TOKEN).is_ok());
    assert!(rim_drop::client::check(&s.base, "wrong-token-0123456789abcdef").unwrap_err().contains("token"));
    let path = s.dir.join("../rim-drop-small.txt");
    std::fs::write(&path, b"hi").unwrap();
    let err = rim_drop::client::upload(&s.base, "nope", "p", &path, 60, 0, |_, _| {}).err().unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(err.contains("token"), "{err}");
    assert_eq!(get(&format!("{}/f/AAAAAAAAAAAAAAAAAAAAAA", s.base)).0, 404);
}

use base64::Engine;
