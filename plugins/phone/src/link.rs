//! The link to the phone app, over the local network.
//!
//! Pairing: RogueIM shows a QR code `rimphone:1:<ip>:<port>:<key>:<name>`
//! (key: 32 random bytes, base64url without padding). The phone connects to
//! <ip>:<port> over TCP:
//!
//! 1. phone → here: "RIMP", version 1, 16 random bytes (nc)
//! 2. here → phone: "RIMP", version 1, 16 random bytes (ns)
//! 3. both: phone-to-here key = HMAC-SHA256(key, "c2s" | nc | ns),
//!    here-to-phone key = HMAC-SHA256(key, "s2c" | nc | ns)
//! 4. then frames both ways: u32 big-endian length, AES-256-GCM ciphertext
//!    of one JSON message; nonce = 4 zero bytes | u64 big-endian counter,
//!    starting at 0 in each direction.
//!
//! Without the key nothing decrypts, so a stranger on the network gets
//! nothing and can send nothing. Fresh nonces per connection: nothing replays.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const MAGIC: &[u8; 4] = b"RIMP";
const VERSION: u8 = 1;
/// Largest frame accepted (a long message with room to spare).
const MAX_FRAME: usize = 1 << 20;

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("random bytes");
    b
}

fn derive(key: &[u8; 32], label: &[u8], nc: &[u8], ns: &[u8]) -> Aes256Gcm {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key");
    mac.update(label);
    mac.update(nc);
    mac.update(ns);
    Aes256Gcm::new_from_slice(&mac.finalize().into_bytes()).expect("AES key")
}

/// One direction of a connection.
pub struct Half {
    cipher: Aes256Gcm,
    counter: u64,
}

impl Half {
    fn nonce(&mut self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        n
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let n = self.nonce();
        let ct = self.cipher.encrypt(Nonce::from_slice(&n), plain).expect("encrypt");
        let mut out = (ct.len() as u32).to_be_bytes().to_vec();
        out.extend(ct);
        out
    }

    pub fn open(&mut self, ct: &[u8]) -> Option<Vec<u8>> {
        let n = self.nonce();
        self.cipher.decrypt(Nonce::from_slice(&n), ct).ok()
    }
}

pub fn read_frame(s: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("frame too big"));
    }
    let mut b = vec![0u8; len];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// The phone's side of the handshake (the app does the same; used in tests).
#[allow(dead_code)]
pub fn client_handshake(s: &mut TcpStream, key: &[u8; 32]) -> std::io::Result<(Half, Half)> {
    let nc: [u8; 16] = random();
    let mut hello = MAGIC.to_vec();
    hello.push(VERSION);
    hello.extend(nc);
    s.write_all(&hello)?;
    let mut reply = [0u8; 21];
    s.read_exact(&mut reply)?;
    if &reply[..4] != MAGIC || reply[4] != VERSION {
        return Err(std::io::Error::other("not RogueIM"));
    }
    let ns = &reply[5..];
    Ok((Half { cipher: derive(key, b"c2s", &nc, ns), counter: 0 }, Half { cipher: derive(key, b"s2c", &nc, ns), counter: 0 }))
}

/// Our side: returns (to the phone, from the phone).
fn server_handshake(s: &mut TcpStream, key: &[u8; 32]) -> std::io::Result<(Half, Half)> {
    let mut hello = [0u8; 21];
    s.read_exact(&mut hello)?;
    if &hello[..4] != MAGIC || hello[4] != VERSION {
        return Err(std::io::Error::other("not the RogueIM phone app"));
    }
    let nc = &hello[5..];
    let ns: [u8; 16] = random();
    let mut reply = MAGIC.to_vec();
    reply.push(VERSION);
    reply.extend(ns);
    s.write_all(&reply)?;
    Ok((Half { cipher: derive(key, b"s2c", nc, &ns), counter: 0 }, Half { cipher: derive(key, b"c2s", nc, &ns), counter: 0 }))
}

pub enum Up {
    /// A phone connected; its first message.
    Connected(serde_json::Value),
    Message(serde_json::Value),
    Gone,
    Error(String),
}

/// The current phone connection, for sending to it.
#[derive(Clone, Default)]
pub struct Link(Arc<Mutex<Option<(TcpStream, Half)>>>);

impl Link {
    pub fn send(&self, v: &serde_json::Value) -> bool {
        let mut g = self.0.lock().expect("link");
        let Some((s, half)) = g.as_mut() else { return false };
        let frame = half.seal(v.to_string().as_bytes());
        if s.write_all(&frame).is_err() {
            *g = None;
            return false;
        }
        true
    }

    pub fn connected(&self) -> bool {
        self.0.lock().expect("link").is_some()
    }
}

/// Accept phones on `listener`; the newest connection replaces an older one.
pub fn serve(listener: TcpListener, key: [u8; 32], link: Link, up: Sender<Up>) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let (key, link, up) = (key, link.clone(), up.clone());
            std::thread::spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(15)));
                let Ok((tx, mut rx)) = server_handshake(&mut s, &key) else { return };
                // The first frame proves the key: it must decrypt.
                let first = read_frame(&mut s).ok().and_then(|f| rx.open(&f)).and_then(|p| serde_json::from_slice::<serde_json::Value>(&p).ok());
                let Some(first) = first else {
                    let _ = up.send(Up::Error("a phone with the wrong key tried to connect".into()));
                    return;
                };
                // Pings come every 30 s; silence for longer means the phone is gone.
                let _ = s.set_read_timeout(Some(Duration::from_secs(95)));
                let Ok(w) = s.try_clone() else { return };
                *link.0.lock().expect("link") = Some((w, tx));
                let _ = up.send(Up::Connected(first));
                while let Ok(f) = read_frame(&mut s) {
                    match rx.open(&f).and_then(|p| serde_json::from_slice(&p).ok()) {
                        Some(v) => {
                            let _ = up.send(Up::Message(v));
                        }
                        None => break,
                    }
                }
                // Only if no newer connection took over.
                let mut g = link.0.lock().expect("link");
                if g.as_ref().is_some_and(|(w, _)| w.peer_addr().ok() == s.peer_addr().ok()) {
                    *g = None;
                    drop(g);
                    let _ = up.send(Up::Gone);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree_and_a_wrong_key_reads_nothing() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let key: [u8; 32] = random();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (mut to_phone, mut from_phone) = server_handshake(&mut s, &key).unwrap();
            let f = read_frame(&mut s).unwrap();
            assert_eq!(from_phone.open(&f).unwrap(), b"hello from the phone");
            s.write_all(&to_phone.seal(b"hello back")).unwrap();
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let (mut up, mut down) = client_handshake(&mut c, &key).unwrap();
        c.write_all(&up.seal(b"hello from the phone")).unwrap();
        let f = read_frame(&mut c).unwrap();
        assert_eq!(down.open(&f).unwrap(), b"hello back");
        t.join().unwrap();

        // Same nonces, other key: nothing opens.
        let mut a = Half { cipher: derive(&key, b"c2s", &[1; 16], &[2; 16]), counter: 0 };
        let mut b = Half { cipher: derive(&[7; 32], b"c2s", &[1; 16], &[2; 16]), counter: 0 };
        let frame = a.seal(b"secret");
        assert!(b.open(&frame[4..]).is_none());
    }

    /// Fixed values, so the phone app can check its own code against them.
    #[test]
    fn known_answer() {
        let key = [0x11u8; 32];
        let mut h = Half { cipher: derive(&key, b"c2s", &[0x22; 16], &[0x33; 16]), counter: 0 };
        let frame = h.seal(b"{\"t\":\"ping\"}");
        let hex: String = frame.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, KNOWN_FRAME);
    }

    // Checked with Python (hmac, cryptography AESGCM); the phone app tests the same value.
    pub const KNOWN_FRAME: &str = "0000001c1852942c95f4b748a154a63284cdad89ff2843616a1d117d0e4fa3f6";
}
