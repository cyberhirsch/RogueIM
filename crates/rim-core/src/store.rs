//! Encrypted state file: Argon2id(passphrase) -> XChaCha20-Poly1305.
//!
//! Layout: b"RIM1" | salt[16] | nonce[24] | ciphertext.
//!
//! Saving is crash-safe: the new file is flushed to disk before it replaces the
//! old one, and the previous version stays as `state.rim.prev`. A crash or power
//! loss right after a save used to leave a file of zeros (written, never flushed);
//! now the worst case is falling back one save.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const MAGIC: &[u8; 4] = b"RIM1";

pub struct Store {
    dir: PathBuf,
    path: PathBuf,
    salt: [u8; 16],
    key: [u8; 32],
}

impl Drop for Store {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

fn derive(pass: &str, salt: &[u8; 16]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(pass.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("argon2: {e}"))?;
    Ok(key)
}

impl Store {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("state.rim")
    }

    fn prev_in(dir: &Path) -> PathBuf {
        dir.join("state.rim.prev")
    }

    pub fn exists(dir: &Path) -> bool {
        Self::path_in(dir).exists() || Self::prev_in(dir).exists()
    }

    /// New store with a fresh salt (first run).
    pub fn create(dir: &Path, pass: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let salt: [u8; 16] = rand::random();
        Ok(Self { dir: dir.to_path_buf(), path: Self::path_in(dir), key: derive(pass, &salt)?, salt })
    }

    /// Open and decrypt an existing store. If the file is damaged (e.g. zeros
    /// after a power loss), the previous version is used.
    pub fn open(dir: &Path, pass: &str) -> Result<(Self, Vec<u8>)> {
        let path = Self::path_in(dir);
        match Self::open_file(dir, &path, pass) {
            Ok(r) => Ok(r),
            Err(e) => {
                let damaged = std::fs::read(&path).map(|d| !Self::looks_valid(&d)).unwrap_or(true);
                if !damaged {
                    return Err(e);
                }
                let (store, plain) = Self::open_file(dir, &Self::prev_in(dir), pass).map_err(|_| e)?;
                // Put the good version back in place.
                store.save(&plain)?;
                Ok((store, plain))
            }
        }
    }

    fn looks_valid(data: &[u8]) -> bool {
        data.len() >= 4 + 16 + 24 && &data[..4] == MAGIC
    }

    fn open_file(dir: &Path, path: &Path, pass: &str) -> Result<(Self, Vec<u8>)> {
        let data = std::fs::read(path)?;
        if data.len() < 4 + 16 + 24 || &data[..4] != MAGIC {
            bail!("not a RogueIM state file");
        }
        let salt: [u8; 16] = data[4..20].try_into()?;
        let key = derive(pass, &salt)?;
        let nonce = XNonce::try_from(&data[20..44]).map_err(|_| anyhow!("nonce"))?;
        let cipher = XChaCha20Poly1305::new(&key.into());
        let plain = cipher
            .decrypt(&nonce, &data[44..])
            .map_err(|_| anyhow!("wrong passphrase or damaged file"))?;
        Ok((Self { dir: dir.to_path_buf(), path: Self::path_in(dir), salt, key }, plain))
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.clone()
    }

    /// Encrypt and write atomically: temp file flushed to disk, the current file
    /// kept as `.prev`, then the temp file renamed into place.
    pub fn save(&self, plain: &[u8]) -> Result<()> {
        let nonce_bytes: [u8; 24] = rand::random();
        let nonce = XNonce::try_from(&nonce_bytes[..]).map_err(|_| anyhow!("nonce"))?;
        let cipher = XChaCha20Poly1305::new(&self.key.into());
        let ct = cipher.encrypt(&nonce, plain).map_err(|_| anyhow!("encrypt"))?;
        let mut out = Vec::with_capacity(44 + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        let tmp = self.path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&out)?;
            f.sync_all()?;
        }
        // Keep the last good version; never let a damaged file replace it.
        if std::fs::read(&self.path).is_ok_and(|d| Self::looks_valid(&d)) {
            std::fs::rename(&self.path, Self::prev_in(&self.dir))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        if let Ok(d) = std::fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rim-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A state file of zeros (power loss right after a save) falls back to the
    /// previous version and is repaired.
    #[test]
    fn a_zeroed_file_falls_back_to_the_previous_one() {
        let d = dir("zero");
        let s = Store::create(&d, "pass").unwrap();
        s.save(b"one").unwrap();
        s.save(b"two").unwrap();
        let path = Store::path_in(&d);
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        std::fs::write(&path, vec![0u8; len]).unwrap();
        assert!(Store::exists(&d));
        let (_, plain) = Store::open(&d, "pass").unwrap();
        assert_eq!(plain, b"one");
        assert!(Store::looks_valid(&std::fs::read(&path).unwrap()), "repaired in place");
        assert!(Store::open(&d, "wrong").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_wrong_passphrase_is_not_mistaken_for_damage() {
        let d = dir("wrong");
        let s = Store::create(&d, "pass").unwrap();
        s.save(b"one").unwrap();
        s.save(b"two").unwrap();
        assert!(Store::open(&d, "nope").is_err());
        assert_eq!(Store::open(&d, "pass").unwrap().1, b"two");
        let _ = std::fs::remove_dir_all(&d);
    }
}
