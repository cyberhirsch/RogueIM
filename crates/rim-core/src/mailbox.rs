//! Nostr relays as a store-and-forward mailbox and address rendezvous
//! (PRD MS-3, NW-8, NW-9, NW-11).
//!
//! * Every device has an `inbox` seed and a `rdv` seed, shared only with
//!   authorized contacts (and own devices). Relays never see a RIM key.
//! * Mail: kind 4333 events, tagged `y` = H(seed, day), signed by a one-time key
//!   derived from (seed, nonce), content = XChaCha20-Poly1305(H(seed), WireReq).
//!   The recipient can re-derive the one-time key and delete the event (NIP-09).
//! * Rendezvous: kind 30078 (NIP-78) replaceable event per device, authored by a
//!   key derived from the rdv seed, content = encrypted listen addresses.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::identity::{b64, unb64};

pub const KIND_MAIL: u16 = 4333;
pub const KIND_RDV: u16 = 30078;
pub const TTL_SECS: u64 = 14 * 24 * 3600;
const DAYS_BACK: i64 = 15;

pub const DEFAULT_RELAYS: [&str; 5] = [
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://relay.nostr.band",
    "wss://offchain.pub",
];

fn h(parts: &[&[u8]]) -> [u8; 32] {
    let mut d = Sha256::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().into()
}

pub fn new_seed() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

pub fn parse_seed(s: &str) -> Result<[u8; 32]> {
    hex::decode(s)?.try_into().map_err(|_| anyhow!("seed must be 32 bytes"))
}

fn day(ts: i64) -> i64 {
    ts.div_euclid(86_400)
}

fn mail_tag(seed: &[u8; 32], day: i64) -> String {
    hex::encode(&h(&[b"rim-mbox-tag", seed, &day.to_le_bytes()])[..16])
}

fn keys_from(material: [u8; 32]) -> Result<Keys> {
    let mut m = material;
    for _ in 0..8 {
        if let Ok(sk) = SecretKey::from_slice(&m) {
            return Ok(Keys::new(sk));
        }
        m = h(&[&m]);
    }
    Err(anyhow!("could not derive a nostr key"))
}

/// Seal a payload for the holder of an inbox seed (used for the DHT copy).
pub fn seal_for(seed: &[u8; 32], plain: &[u8]) -> Result<String> {
    seal(&h(&[b"rim-dht-enc", seed]), plain)
}

pub fn open_for(seed: &[u8; 32], sealed: &str) -> Result<Vec<u8>> {
    open(&h(&[b"rim-dht-enc", seed]), sealed)
}

fn seal(key: &[u8; 32], plain: &[u8]) -> Result<String> {
    let nonce: [u8; 24] = rand::random();
    let c = XChaCha20Poly1305::new(&(*key).into());
    let ct = c
        .encrypt(&XNonce::try_from(&nonce[..]).map_err(|_| anyhow!("nonce"))?, plain)
        .map_err(|_| anyhow!("seal"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(b64(&out))
}

fn open(key: &[u8; 32], sealed: &str) -> Result<Vec<u8>> {
    let data = unb64(sealed)?;
    if data.len() < 24 {
        return Err(anyhow!("short"));
    }
    let c = XChaCha20Poly1305::new(&(*key).into());
    c.decrypt(&XNonce::try_from(&data[..24]).map_err(|_| anyhow!("nonce"))?, &data[24..])
        .map_err(|_| anyhow!("mailbox item does not decrypt"))
}

pub struct MailItem {
    pub seed: [u8; 32],
    pub nonce: String,
    pub event_id: EventId,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
pub struct Mailbox {
    client: Client,
    relays: Vec<String>,
}

impl Mailbox {
    /// Connect to the relays (in the background; returns immediately).
    pub async fn connect(relays: &[String]) -> Self {
        let client = Client::default();
        for r in relays {
            let _ = client.add_relay(r.as_str()).await;
        }
        client.connect().await;
        Self { client, relays: relays.to_vec() }
    }

    pub async fn relay_status(&self) -> Vec<(String, bool)> {
        let map = self.client.relays().await;
        self.relays
            .iter()
            .map(|r| {
                let ok = map.iter().any(|(u, relay)| u.as_str().trim_end_matches('/') == r.trim_end_matches('/') && relay.status().is_connected());
                (r.clone(), ok)
            })
            .collect()
    }

    /// Post a payload (an encrypted WireReq) into the mailbox of the device
    /// owning `seed`.
    pub async fn post(&self, seed: &[u8; 32], payload: &[u8]) -> Result<()> {
        let nonce = hex::encode(rand::random::<[u8; 16]>());
        let keys = keys_from(h(&[b"rim-mbox-key", seed, nonce.as_bytes()]))?;
        let now = Timestamp::now().as_secs();
        let content = seal(&h(&[b"rim-mbox-enc", seed]), payload)?;
        let ev = EventBuilder::new(Kind::Custom(KIND_MAIL), content)
            .tags([
                Tag::custom("y", [mail_tag(seed, day(now as i64))]),
                Tag::custom("n", [nonce]),
                Tag::expiration(Timestamp::from(now + TTL_SECS)),
            ])
            .finalize(&keys)?;
        let out = self.client.send_event(&ev).await.context("mailbox post")?;
        if out.success.is_empty() {
            return Err(anyhow!("no relay accepted the message"));
        }
        Ok(())
    }

    /// Fetch everything waiting for the given inbox seeds.
    pub async fn fetch(&self, seeds: &[[u8; 32]]) -> Result<Vec<MailItem>> {
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        let today = day(Timestamp::now().as_secs() as i64);
        let mut tag_map = std::collections::HashMap::new();
        for s in seeds {
            for d in (today - DAYS_BACK)..=today {
                tag_map.insert(mail_tag(s, d), *s);
            }
        }
        let filter = Filter::new()
            .kind(Kind::Custom(KIND_MAIL))
            .custom_tags(SingleLetterTag::from_char('y').expect("letter"), tag_map.keys().cloned().collect::<Vec<_>>());
        let events = self.client.fetch_events(filter).timeout(Duration::from_secs(8)).await?;
        let mut out = vec![];
        for ev in events {
            let tag_of = |name: &str| ev.tags.iter().find(|t| t.kind() == name).and_then(|t| t.content()).map(str::to_string);
            let (Some(y), Some(nonce)) = (tag_of("y"), tag_of("n")) else { continue };
            let Some(seed) = tag_map.get(&y) else { continue };
            // Only accept events signed by the key derived from our seed and nonce.
            let Ok(keys) = keys_from(h(&[b"rim-mbox-key", seed, nonce.as_bytes()])) else { continue };
            if keys.public_key() != ev.pubkey {
                continue;
            }
            if let Ok(payload) = open(&h(&[b"rim-mbox-enc", seed]), &ev.content) {
                out.push(MailItem { seed: *seed, nonce, event_id: ev.id, payload });
            }
        }
        Ok(out)
    }

    /// Ask relays to delete a fetched item (NIP-09), signed by its one-time key.
    pub async fn delete(&self, item: &MailItem) -> Result<()> {
        let keys = keys_from(h(&[b"rim-mbox-key", &item.seed, item.nonce.as_bytes()]))?;
        let ev = EventDeletionRequest::new().id(item.event_id).into_event_builder().finalize(&keys)?;
        self.client.send_event(&ev).await?;
        Ok(())
    }

    /// Publish this device's current addresses for its contacts.
    pub async fn publish_rdv(&self, seed: &[u8; 32], addrs: &[String]) -> Result<()> {
        let keys = keys_from(h(&[b"rim-rdv-key", seed]))?;
        let d = hex::encode(&h(&[b"rim-rdv-d", seed])[..16]);
        let content = seal(&h(&[b"rim-rdv-enc", seed]), &serde_json::to_vec(addrs)?)?;
        let ev = EventBuilder::new(Kind::Custom(KIND_RDV), content)
            .tags([
                Tag::identifier(d),
                Tag::expiration(Timestamp::from(Timestamp::now().as_secs() + 7 * 24 * 3600)),
            ])
            .finalize(&keys)?;
        self.client.send_event(&ev).await?;
        Ok(())
    }

    /// Latest addresses published under a contact device's rdv seed.
    pub async fn fetch_rdv(&self, seed: &[u8; 32]) -> Result<Option<Vec<String>>> {
        let keys = keys_from(h(&[b"rim-rdv-key", seed]))?;
        let d = hex::encode(&h(&[b"rim-rdv-d", seed])[..16]);
        let filter = Filter::new().kind(Kind::Custom(KIND_RDV)).author(keys.public_key()).identifier(d);
        let events = self.client.fetch_events(filter).timeout(Duration::from_secs(6)).await?;
        let Some(ev) = events.into_iter().max_by_key(|e| e.created_at) else { return Ok(None) };
        let plain = open(&h(&[b"rim-rdv-enc", seed]), &ev.content)?;
        Ok(Some(serde_json::from_slice(&plain)?))
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}
