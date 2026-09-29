//! Encrypted state file: Argon2id(passphrase) -> XChaCha20-Poly1305.
//!
//! Layout: b"RIM1" | salt[16] | nonce[24] | ciphertext.

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

    pub fn exists(dir: &Path) -> bool {
        Self::path_in(dir).exists()
    }

    /// New store with a fresh salt (first run).
    pub fn create(dir: &Path, pass: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let salt: [u8; 16] = rand::random();
        Ok(Self { dir: dir.to_path_buf(), path: Self::path_in(dir), key: derive(pass, &salt)?, salt })
    }

    /// Open and decrypt an existing store.
    pub fn open(dir: &Path, pass: &str) -> Result<(Self, Vec<u8>)> {
        let path = Self::path_in(dir);
        let data = std::fs::read(&path)?;
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
        Ok((Self { dir: dir.to_path_buf(), path, salt, key }, plain))
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.clone()
    }

    /// Encrypt and write atomically (write temp, then rename).
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
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
}
