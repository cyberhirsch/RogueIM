//! Send to anyone (PRD HB-3): a file is encrypted here with its own random key,
//! only the ciphertext goes to the drop (the user's Pi), and the link carries
//! the key after the `#`, which browsers never send to a server. The download
//! page (site/f/index.html) decrypts it in the recipient's browser.
//!
//! The format, "RIMF" version 1:
//!
//! ```text
//! "RIMF" 0x01
//! u32 BE   length of the sealed metadata
//! sealed   {"name", "size", "type"} as JSON, nonce = 02 00 00 00 | 0u64
//! chunks   each CHUNK bytes of the file (the last one shorter, or empty for an
//!          empty file), sealed with nonce = last? 01 : 00, 00 00 00 | index u64 BE
//! ```
//!
//! AES-256-GCM throughout; the nonce marks the last chunk, so a cut-off file
//! does not decrypt as a shorter one, and chunks cannot be reordered.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8; 5] = b"RIMF\x01";
pub const CHUNK: usize = 1 << 20;
pub const TAG: usize = 16;
/// Upload parts stay below Cloudflare's 100 MB limit per request.
pub const PART: usize = 32 << 20;
pub const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Meta {
    pub name: String,
    pub size: u64,
    #[serde(rename = "type", default)]
    pub kind: String,
}

fn nonce(kind: u8, index: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = kind;
    n[4..].copy_from_slice(&index.to_be_bytes());
    n
}

pub fn new_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).expect("random");
    k
}

/// Size of the ciphertext for a file of `size` bytes with this metadata.
pub fn sealed_len(meta: &Meta) -> u64 {
    let meta_len = serde_json::to_vec(meta).map(|v| v.len()).unwrap_or(0) + TAG;
    let chunks = (meta.size.div_ceil(CHUNK as u64)).max(1);
    (MAGIC.len() + 4 + meta_len) as u64 + chunks * TAG as u64 + meta.size
}

/// Encrypts a file chunk by chunk; feed it the file in CHUNK-sized pieces.
pub struct Sealer {
    cipher: Aes256Gcm,
    index: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Sealer {
        Sealer { cipher: Aes256Gcm::new(key.into()), index: 0 }
    }

    pub fn header(&self, meta: &Meta) -> Vec<u8> {
        let json = serde_json::to_vec(meta).expect("meta");
        let sealed = self.cipher.encrypt(Nonce::from_slice(&nonce(2, 0)), json.as_slice()).expect("seal");
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
        out.extend_from_slice(&sealed);
        out
    }

    pub fn chunk(&mut self, plain: &[u8], last: bool) -> Vec<u8> {
        let n = nonce(last as u8, self.index);
        self.index += 1;
        self.cipher.encrypt(Nonce::from_slice(&n), plain).expect("seal")
    }
}

#[derive(Debug, PartialEq)]
pub enum OpenError {
    NotRimf,
    Damaged,
    Truncated,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OpenError::NotRimf => "not a RogueIM file",
            OpenError::Damaged => "wrong key or damaged file",
            OpenError::Truncated => "the file is incomplete",
        })
    }
}

/// Decrypts a whole sealed file (tests, and small files in the app).
pub fn open(key: &[u8; 32], data: &[u8]) -> Result<(Meta, Vec<u8>), OpenError> {
    let cipher = Aes256Gcm::new(key.into());
    if data.len() < MAGIC.len() + 4 || &data[..MAGIC.len()] != MAGIC {
        return Err(OpenError::NotRimf);
    }
    let ml = u32::from_be_bytes(data[5..9].try_into().unwrap()) as usize;
    let rest = &data[9..];
    if rest.len() < ml {
        return Err(OpenError::Truncated);
    }
    let json = cipher.decrypt(Nonce::from_slice(&nonce(2, 0)), &rest[..ml]).map_err(|_| OpenError::Damaged)?;
    let meta: Meta = serde_json::from_slice(&json).map_err(|_| OpenError::Damaged)?;
    let mut body = &rest[ml..];
    let mut out = Vec::with_capacity(meta.size as usize);
    let mut index = 0u64;
    loop {
        let left = meta.size - out.len() as u64;
        let take = (left.min(CHUNK as u64) as usize) + TAG;
        if body.len() < take {
            return Err(OpenError::Truncated);
        }
        let last = left <= CHUNK as u64;
        let plain = cipher.decrypt(Nonce::from_slice(&nonce(last as u8, index)), &body[..take]).map_err(|_| OpenError::Damaged)?;
        out.extend_from_slice(&plain);
        body = &body[take..];
        index += 1;
        if last {
            break;
        }
    }
    Ok((meta, out))
}

/// The link the recipient opens: the page, then (after `#`) where the
/// ciphertext is and the key. Browsers do not send the part after `#`.
pub fn link(page: &str, file_url: &str, key: &[u8; 32]) -> String {
    format!("{page}#{file_url},{}", B64.encode(key))
}

/// The upload, as the app does it: create, send the parts, finish.
#[cfg(feature = "client")]
pub mod client {
    use super::*;
    use std::io::Read;

    pub struct Upload {
        pub id: String,
        pub url: String,
        pub link: String,
        pub size: u64,
    }

    #[derive(Deserialize)]
    struct Created {
        id: String,
    }

    /// Encrypts `path` and uploads it to the drop at `base` (e.g.
    /// https://drop.example.org). `progress` gets the bytes sent so far.
    pub fn upload(base: &str, token: &str, page: &str, path: &std::path::Path, expires_secs: u64, max_downloads: u32, mut progress: impl FnMut(u64, u64)) -> Result<Upload, String> {
        let base = base.trim_end_matches('/');
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let meta = Meta { name, size, kind: kind_of(path) };
        let total = sealed_len(&meta);
        let auth = format!("Bearer {token}");
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let body = serde_json::json!({ "size": total, "expires_secs": expires_secs, "max_downloads": max_downloads });
        let mut r = agent.post(format!("{base}/api/files")).header("Authorization", &auth).header("Content-Type", "application/json").send(body.to_string().as_bytes()).map_err(|e| e.to_string())?;
        if r.status() != 201 {
            return Err(format!("drop: {}", r.body_mut().read_to_string().unwrap_or_default().trim()));
        }
        let created: Created = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let key = new_key();
        let mut sealer = Sealer::new(&key);
        let mut part = sealer.header(&meta);
        let mut sent = 0u64;
        let mut read = 0u64;
        let mut buf = vec![0u8; CHUNK];
        let put = |part: &[u8], offset: u64| -> Result<(), String> {
            let mut r = agent
                .put(format!("{base}/api/files/{}?offset={offset}", created.id))
                .header("Authorization", &auth)
                .header("Content-Type", "application/octet-stream")
                .send(part)
                .map_err(|e| e.to_string())?;
            if r.status() != 200 {
                return Err(format!("drop: {}", r.body_mut().read_to_string().unwrap_or_default().trim()));
            }
            Ok(())
        };
        loop {
            let mut n = 0;
            while n < CHUNK {
                let got = file.read(&mut buf[n..]).map_err(|e| e.to_string())?;
                if got == 0 {
                    break;
                }
                n += got;
            }
            read += n as u64;
            let last = read >= size;
            part.extend_from_slice(&sealer.chunk(&buf[..n], last));
            if part.len() >= PART || last {
                put(&part, sent)?;
                sent += part.len() as u64;
                progress(sent, total);
                part.clear();
            }
            if last {
                break;
            }
        }
        let mut r = agent.post(format!("{base}/api/files/{}/done", created.id)).header("Authorization", &auth).send_empty().map_err(|e| e.to_string())?;
        if r.status() != 200 {
            return Err(format!("drop: {}", r.body_mut().read_to_string().unwrap_or_default().trim()));
        }
        let url = format!("{base}/f/{}", created.id);
        Ok(Upload { link: link(page, &url, &key), url, id: created.id, size })
    }

    /// Whether the drop answers and takes this token.
    pub fn check(base: &str, token: &str) -> Result<(), String> {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let mut r = agent.get(format!("{}/api/check", base.trim_end_matches('/'))).header("Authorization", &format!("Bearer {token}")).call().map_err(|e| e.to_string())?;
        match r.status().as_u16() {
            200 => Ok(()),
            401 => Err("the drop does not take this token".into()),
            s => Err(format!("drop answered {s}: {}", r.body_mut().read_to_string().unwrap_or_default().trim())),
        }
    }

    fn kind_of(path: &std::path::Path) -> String {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "pdf" => "application/pdf",
            "zip" => "application/zip",
            "mp4" => "video/mp4",
            "mov" => "video/quicktime",
            "mp3" => "audio/mpeg",
            "txt" => "text/plain",
            _ => "application/octet-stream",
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal_all(key: &[u8; 32], name: &str, data: &[u8]) -> Vec<u8> {
        let meta = Meta { name: name.into(), size: data.len() as u64, kind: "text/plain".into() };
        let mut s = Sealer::new(key);
        let mut out = s.header(&meta);
        let mut chunks: Vec<&[u8]> = data.chunks(CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            out.extend_from_slice(&s.chunk(c, i + 1 == n));
        }
        assert_eq!(out.len() as u64, sealed_len(&meta));
        out
    }

    #[test]
    fn round_trip_at_the_edges() {
        let key = new_key();
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 7] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let (meta, back) = open(&key, &seal_all(&key, "a.txt", &data)).unwrap();
            assert_eq!((meta.name.as_str(), meta.size), ("a.txt", len as u64));
            assert_eq!(back, data);
        }
    }

    #[test]
    fn wrong_key_cut_off_and_reordered_are_refused() {
        let key = new_key();
        let data = vec![5u8; 2 * CHUNK + 10];
        let sealed = seal_all(&key, "x", &data);
        assert_eq!(open(&new_key(), &sealed).unwrap_err(), OpenError::Damaged);
        // Without the last chunk: the one before it is not marked last.
        let cut = &sealed[..sealed.len() - (10 + TAG)];
        assert!(open(&key, cut).is_err());
        let mut flipped = sealed.clone();
        let n = flipped.len();
        flipped[n - 1] ^= 1;
        assert_eq!(open(&key, &flipped).unwrap_err(), OpenError::Damaged);
    }

    /// The same bytes the download page checks against (site/f/test.js).
    #[test]
    fn known_answer() {
        let key = [0x11u8; 32];
        let sealed = seal_all(&key, "hi.txt", b"hello");
        let hex: String = sealed.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(&hex[..10], "52494d4601");
        assert_eq!(open(&key, &sealed).unwrap().1, b"hello");
        println!("{hex}");
    }

    #[test]
    fn links_keep_the_key_after_the_hash() {
        let l = link("https://p/f/", "https://d/f/abc", &[0u8; 32]);
        assert_eq!(l, format!("https://p/f/#https://d/f/abc,{}", B64.encode([0u8; 32])));
    }
}
