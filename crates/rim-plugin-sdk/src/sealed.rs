//! Plugin data at rest. RogueIM hands every plugin its own key in
//! `RIM_PLUGIN_KEY` (hex); files written with [`write`] are sealed with it
//! (XChaCha20-Poly1305, the file name as associated data, so files cannot be
//! swapped). The key comes from the unlocked profile, so without the passphrase
//! the files are unreadable.
//!
//! Layout: b"RIMS1" | nonce[24] | ciphertext. The same format is written by the
//! Go plugins (sealed.go) and read by the app for pictures.

use std::io;
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

pub const KEY_ENV: &str = "RIM_PLUGIN_KEY";
pub const MAGIC: &[u8; 5] = b"RIMS1";
const HEAD: usize = 5 + 24;

/// This plugin's key, if RogueIM gave one.
pub fn key() -> Option<[u8; 32]> {
    let h = std::env::var(KEY_ENV).ok()?;
    let b = hex::decode(h.trim()).ok()?;
    b.try_into().ok()
}

pub fn is_sealed(data: &[u8]) -> bool {
    data.len() >= HEAD + 16 && &data[..5] == MAGIC
}

fn aad(path: &Path) -> Vec<u8> {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into_bytes()
}

pub fn seal(key: &[u8; 32], name: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut nonce).expect("system randomness");
    let ct = XChaCha20Poly1305::new(key.into())
        .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad: name })
        .expect("encrypt");
    let mut out = Vec::with_capacity(HEAD + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

pub fn open(key: &[u8; 32], name: &[u8], data: &[u8]) -> io::Result<Vec<u8>> {
    if !is_sealed(data) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a sealed file"));
    }
    let nonce = XNonce::try_from(&data[5..HEAD]).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "nonce"))?;
    XChaCha20Poly1305::new(key.into())
        .decrypt(&nonce, Payload { msg: &data[HEAD..], aad: name })
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "wrong key or damaged file"))
}

/// Seal a file's content for `path` (its name is bound to it).
pub fn seal_for(key: &[u8; 32], path: &Path, plain: &[u8]) -> Vec<u8> {
    seal(key, &aad(path), plain)
}

/// Open a sealed file read from `path`.
pub fn open_from(key: &[u8; 32], path: &Path, data: &[u8]) -> io::Result<Vec<u8>> {
    open(key, &aad(path), data)
}

/// Read a plugin file. A file from before encryption is read as it is and
/// sealed on the spot.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    let Some(k) = key() else { return Ok(data) };
    if is_sealed(&data) {
        return open_from(&k, path, &data);
    }
    let _ = write_with(&k, path, &data);
    Ok(data)
}

/// Write a plugin file, sealed when RogueIM gave a key (always, outside tests).
pub fn write(path: &Path, plain: &[u8]) -> io::Result<()> {
    match key() {
        Some(k) => write_with(&k, path, plain),
        None => std::fs::write(path, plain),
    }
}

fn write_with(key: &[u8; 32], path: &Path, plain: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("sealing");
    std::fs::write(&tmp, seal_for(key, path, plain))?;
    std::fs::rename(&tmp, path)
}

/// Where plain copies for the system viewer go ("open"); [`clear_open_copies`]
/// empties it when the plugin starts and stops.
pub fn open_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).unwrap_or_else(|| "rim-plugin".into());
    std::env::temp_dir().join(format!("{exe}-open"))
}

pub fn clear_open_copies() {
    let _ = std::fs::remove_dir_all(open_dir());
}

/// Where "save" puts a decrypted copy.
pub fn downloads_dir() -> std::path::PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| Path::new(&h).join("Downloads")).unwrap_or_else(std::env::temp_dir)
}

/// Decrypt a sealed file into `dir` under `name` (or its own name), with a
/// number added if a different file of that name is there; returns the new path.
pub fn plain_copy(path: &Path, dir: &Path, name: Option<&str>) -> io::Result<std::path::PathBuf> {
    let plain = read(path)?;
    std::fs::create_dir_all(dir)?;
    let name = name.filter(|n| !n.is_empty() && !n.contains(['/', '\\', ':'])).map(str::to_string).unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut out = dir.join(&name);
    let mut i = 2;
    while std::fs::read(&out).is_ok_and(|old| old != plain) {
        out = dir.join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    std::fs::write(&out, plain)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        let k = [7u8; 32];
        let p = Path::new("dir/chats.json");
        let s = seal_for(&k, p, b"hello");
        assert!(is_sealed(&s));
        assert_eq!(open_from(&k, p, &s).unwrap(), b"hello");
        assert!(open_from(&[8u8; 32], p, &s).is_err(), "wrong key");
        assert!(open_from(&k, Path::new("dir/other.json"), &s).is_err(), "swapped file");
        let mut bad = s.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open_from(&k, p, &bad).is_err(), "tampered");
        assert!(!is_sealed(b"{\"plain\":true}"));
    }

    /// The same bytes the Go plugins check against (sealed_test.go).
    #[test]
    fn known_answer() {
        let k = [0x11u8; 32];
        let nonce = [0x22u8; 24];
        let ct = XChaCha20Poly1305::new((&k).into()).encrypt(&XNonce::from(nonce), Payload { msg: b"rim", aad: b"a.json" }).unwrap();
        assert_eq!(hex::encode(ct), "f10e8a369001e04cf03d0880b81b3958d6b2d1");
    }
}
