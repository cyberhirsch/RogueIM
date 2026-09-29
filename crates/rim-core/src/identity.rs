//! Account key, device lists, cards, invites, link codes, fingerprints,
//! safety numbers, proof of work and the small signed statements.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine as _;
use libp2p::identity::{Keypair, PublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use vodozemac::Curve25519PublicKey;

use crate::proto::*;

pub const INVITE_PREFIX: &str = "rim2:";
pub const LINK_PREFIX: &str = "rimlink2:";

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    Ok(B64.decode(s)?)
}

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Full fingerprint of an account key: hex SHA-256 of its encoding.
/// Used as the stable contact id.
pub fn account_id(account_pk_b64: &str) -> Result<String> {
    Ok(hex::encode(Sha256::digest(unb64(account_pk_b64)?)))
}

/// Human-readable fingerprint: first 20 bytes in groups of 4 hex chars.
pub fn pretty_fingerprint(id: &str) -> String {
    id.chars()
        .take(40)
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

/// Signal-style safety number: 60 digits (12 groups of 5) derived from both
/// account keys, identical on both sides.
pub fn safety_number(a_pk: &str, b_pk: &str) -> Result<String> {
    let mut parts = [a_pk.to_string(), b_pk.to_string()];
    parts.sort();
    let mut groups = vec![];
    for pk in &parts {
        let mut h = Sha512::digest(unb64(pk)?).to_vec();
        for _ in 0..512 {
            h = Sha512::digest(&h).to_vec();
        }
        for i in 0..6 {
            let chunk = &h[i * 5..i * 5 + 5];
            let v = chunk.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64) % 100_000;
            groups.push(format!("{v:05}"));
        }
    }
    Ok(groups.join(" "))
}

fn sign(account: &Keypair, domain: &str, payload: &[u8]) -> Result<String> {
    let mut m = domain.as_bytes().to_vec();
    m.push(0);
    m.extend_from_slice(payload);
    Ok(b64(&account.sign(&m)?))
}

fn verify(account_pk: &str, domain: &str, payload: &[u8], sig: &str) -> Result<()> {
    let pk = PublicKey::try_decode_protobuf(&unb64(account_pk)?).context("account key")?;
    let mut m = domain.as_bytes().to_vec();
    m.push(0);
    m.extend_from_slice(payload);
    if !pk.verify(&m, &unb64(sig)?) {
        bail!("signature invalid ({domain})");
    }
    Ok(())
}

pub fn account_pk_of(kp: &Keypair) -> String {
    b64(&kp.public().encode_protobuf())
}

// ---------------------------------------------------------------- device lists

pub fn sign_device_list(account: &Keypair, list: DeviceList) -> Result<SignedDeviceList> {
    if list.account_pk != account_pk_of(account) {
        bail!("device list belongs to another account");
    }
    let sig = sign(account, "rim-devices-v2", &serde_json::to_vec(&list)?)?;
    Ok(SignedDeviceList { list, sig })
}

pub fn verify_device_list(s: &SignedDeviceList) -> Result<()> {
    verify(&s.list.account_pk, "rim-devices-v2", &serde_json::to_vec(&s.list)?, &s.sig)?;
    if s.list.devices.is_empty() {
        bail!("empty device list");
    }
    for d in &s.list.devices {
        Curve25519PublicKey::from_base64(&d.curve).map_err(|e| anyhow!("device curve: {e}"))?;
        Curve25519PublicKey::from_base64(&d.fallback).map_err(|e| anyhow!("device fallback: {e}"))?;
        d.peer_id.parse::<libp2p::PeerId>().context("device peer id")?;
    }
    Ok(())
}

pub fn verify_card(card: &Card) -> Result<()> {
    verify_device_list(&card.devices)
}

// ---------------------------------------------------------------- invites

pub fn encode_invite(inv: &Invite) -> Result<String> {
    Ok(format!("{INVITE_PREFIX}{}", B64URL.encode(serde_json::to_vec(inv)?)))
}

pub fn decode_invite(s: &str) -> Result<Invite> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.starts_with("rim1:") {
        bail!("this invite is from an older RogueIM version; ask for a new one");
    }
    let body = s.strip_prefix(INVITE_PREFIX).ok_or_else(|| anyhow!("not a RogueIM invite (must start with {INVITE_PREFIX})"))?;
    // A damaged code is nearly always a copy problem: cut off by a messenger,
    // or a character added or lost on the way.
    const DAMAGED: &str = "this invite is incomplete or damaged; it was probably cut off or changed while being copied. Ask for it again (e.g. as a text file)";
    let bytes = B64URL.decode(body).map_err(|_| anyhow!(DAMAGED))?;
    let inv: Invite = serde_json::from_slice(&bytes).map_err(|_| anyhow!(DAMAGED))?;
    if inv.v != 2 {
        bail!("unsupported invite version {}", inv.v);
    }
    verify_card(&inv.card)?;
    if !inv.card.devices.list.devices.iter().any(|d| d.peer_id == inv.device) {
        bail!("invite device not in the card");
    }
    if let Some(exp) = inv.expires {
        if exp < now() {
            bail!("this invite has expired");
        }
    }
    Ok(inv)
}

// ---------------------------------------------------------------- linking

/// What a new device shows so an existing device can link it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkCode {
    pub v: u8,
    pub peer_id: String,
    pub curve: String,
    pub fallback: String,
    pub name: String,
    pub os: String,
    pub class: DeviceClass,
    pub seeds: DeviceSeeds,
    pub addrs: Vec<String>,
    pub nonce: String,
}

pub fn encode_link(code: &LinkCode) -> Result<String> {
    Ok(format!("{LINK_PREFIX}{}", B64URL.encode(serde_json::to_vec(code)?)))
}

pub fn decode_link(s: &str) -> Result<LinkCode> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let body = s.strip_prefix(LINK_PREFIX).ok_or_else(|| anyhow!("not a RogueIM link code (must start with {LINK_PREFIX})"))?;
    let code: LinkCode = serde_json::from_slice(&B64URL.decode(body).context("link code encoding")?)?;
    Curve25519PublicKey::from_base64(&code.curve).map_err(|e| anyhow!("{e}"))?;
    Curve25519PublicKey::from_base64(&code.fallback).map_err(|e| anyhow!("{e}"))?;
    code.peer_id.parse::<libp2p::PeerId>().context("peer id")?;
    Ok(code)
}

/// Six words both devices display during linking; they must match.
/// Derived from the stable part of the link code (not the addresses).
pub fn link_words(code: &LinkCode) -> String {
    sas_words(&format!("{}|{}|{}|{}", code.peer_id, code.curve, code.fallback, code.nonce))
}

pub fn sas_words(code: &str) -> String {
    let h = Sha256::digest(code.trim().as_bytes());
    let list = bip39::Language::English.word_list();
    (0..6)
        .map(|i| {
            let idx = (((h[i * 2] as usize) << 8) | h[i * 2 + 1] as usize) % 2048;
            list[idx]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- proof of work

fn pow_hash(token: &str, account_pk: &str, nonce: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"rim-pow-v2");
    h.update(token.as_bytes());
    h.update(account_pk.as_bytes());
    h.update(nonce.to_le_bytes());
    h.finalize().into()
}

fn leading_zero_bits(h: &[u8]) -> u32 {
    let mut n = 0;
    for b in h {
        if *b == 0 {
            n += 8;
        } else {
            n += b.leading_zeros();
            break;
        }
    }
    n
}

pub fn pow_solve(token: &str, account_pk: &str, bits: u8) -> u64 {
    (0u64..).find(|n| leading_zero_bits(&pow_hash(token, account_pk, *n)) >= bits as u32).unwrap()
}

pub fn pow_check(token: &str, account_pk: &str, nonce: u64, bits: u8) -> bool {
    leading_zero_bits(&pow_hash(token, account_pk, nonce)) >= bits as u32
}

// ---------------------------------------------------------------- signed statements

/// Public key of an Ed25519 PeerId (the key is inlined in the id).
pub fn peer_public_key(peer: &str) -> Result<PublicKey> {
    let pid: libp2p::PeerId = peer.parse().context("peer id")?;
    let mh = pid.as_ref();
    if mh.code() != 0 {
        bail!("peer id does not inline its key");
    }
    Ok(PublicKey::try_decode_protobuf(mh.digest())?)
}

fn device_sign(device: &Keypair, domain: &str, payload: &[u8]) -> Result<String> {
    let mut m = domain.as_bytes().to_vec();
    m.push(0);
    m.extend_from_slice(payload);
    Ok(b64(&device.sign(&m)?))
}

/// Sign a Nostr signal with this device's key.
pub fn sign_signal(device: &Keypair, mut s: Signal) -> Result<Signal> {
    s.sig = String::new();
    let payload = serde_json::to_vec(&(&s.from, &s.to, s.ts, &s.kind))?;
    s.sig = device_sign(device, "rim-signal-v1", &payload)?;
    Ok(s)
}

/// Check a signal's signature against the key inside its sender's peer id.
pub fn verify_signal(s: &Signal) -> Result<()> {
    let pk = peer_public_key(&s.from)?;
    let mut m = b"rim-signal-v1".to_vec();
    m.push(0);
    m.extend_from_slice(&serde_json::to_vec(&(&s.from, &s.to, s.ts, &s.kind))?);
    if !pk.verify(&m, &unb64(&s.sig)?) {
        bail!("signal signature invalid");
    }
    Ok(())
}

/// Verify a device signature and that the device belongs to `list`.
fn device_verify(list: &SignedDeviceList, device: &str, domain: &str, payload: &[u8], sig: &str) -> Result<()> {
    if !list.list.devices.iter().any(|d| d.peer_id == device) {
        bail!("signing device not in the account's device list ({domain})");
    }
    let pk = peer_public_key(device)?;
    let mut m = domain.as_bytes().to_vec();
    m.push(0);
    m.extend_from_slice(payload);
    if !pk.verify(&m, &unb64(sig)?) {
        bail!("signature invalid ({domain})");
    }
    Ok(())
}

#[derive(Serialize)]
struct IntroPayload<'a> {
    card: &'a Card,
    to: &'a str,
    introducer: &'a str,
    device: &'a str,
    verified: bool,
    ts: i64,
}

/// Sign an introduction with this device's key.
pub fn sign_introduction(device: &Keypair, account_pk: &str, mut intro: Introduction) -> Result<Introduction> {
    intro.introducer = account_pk.to_string();
    intro.introducer_device = device.public().to_peer_id().to_string();
    let p = IntroPayload { card: &intro.card, to: &intro.to, introducer: &intro.introducer, device: &intro.introducer_device, verified: intro.verified, ts: intro.ts };
    intro.sig = device_sign(device, "rim-intro-v2", &serde_json::to_vec(&p)?)?;
    Ok(intro)
}

/// Verify an introduction against the introducer's (known) device list.
pub fn verify_introduction(intro: &Introduction, introducer_devices: &SignedDeviceList) -> Result<()> {
    verify_card(&intro.card)?;
    if introducer_devices.list.account_pk != intro.introducer {
        bail!("introduction from an unknown account");
    }
    let p = IntroPayload { card: &intro.card, to: &intro.to, introducer: &intro.introducer, device: &intro.introducer_device, verified: intro.verified, ts: intro.ts };
    device_verify(introducer_devices, &intro.introducer_device, "rim-intro-v2", &serde_json::to_vec(&p)?, &intro.sig)
}

#[derive(Serialize)]
struct GroupPayload<'a> {
    id: &'a str,
    name: &'a str,
    version: u64,
    members: &'a [GroupMember],
    by: &'a str,
    by_device: &'a str,
}

/// Sign a group state with this device's key (the account must be an admin).
pub fn sign_group(device: &Keypair, account_pk: &str, mut g: GroupState) -> Result<GroupState> {
    g.by = account_pk.to_string();
    g.by_device = device.public().to_peer_id().to_string();
    let p = GroupPayload { id: &g.id, name: &g.name, version: g.version, members: &g.members, by: &g.by, by_device: &g.by_device };
    g.sig = device_sign(device, "rim-group-v2", &serde_json::to_vec(&p)?)?;
    Ok(g)
}

pub fn verify_group(g: &GroupState) -> Result<()> {
    for m in &g.members {
        verify_device_list(&m.devices)?;
    }
    let admin = g
        .members
        .iter()
        .find(|m| m.devices.list.account_pk == g.by && m.admin)
        .ok_or_else(|| anyhow!("group change not signed by an admin"))?;
    let p = GroupPayload { id: &g.id, name: &g.name, version: g.version, members: &g.members, by: &g.by, by_device: &g.by_device };
    device_verify(&admin.devices, &g.by_device, "rim-group-v2", &serde_json::to_vec(&p)?, &g.sig)
}

pub fn sign_remote(account: &Keypair, target: &str, action: RemoteAction, ts: i64) -> Result<RemoteCommand> {
    let sig = sign(account, "rim-remote-v2", format!("{target}|{action:?}|{ts}").as_bytes())?;
    Ok(RemoteCommand { target: target.to_string(), action, ts, sig })
}

pub fn verify_remote(account_pk: &str, c: &RemoteCommand) -> Result<()> {
    verify(account_pk, "rim-remote-v2", format!("{}|{:?}|{}", c.target, c.action, c.ts).as_bytes(), &c.sig)
}

// ---------------------------------------------------------------- recovery key

/// 24 BIP-39 words encoding 32 random bytes.
pub fn new_recovery_words() -> (String, [u8; 32]) {
    let entropy: [u8; 32] = rand::random();
    let m = bip39::Mnemonic::from_entropy(&entropy).expect("32 bytes of entropy");
    (m.to_string(), entropy)
}

pub fn recovery_key_from_words(words: &str) -> Result<[u8; 32]> {
    let m = bip39::Mnemonic::parse_normalized(&words.trim().to_lowercase()).map_err(|e| anyhow!("recovery key: {e}"))?;
    let e = m.to_entropy();
    e.try_into().map_err(|_| anyhow!("recovery key must be 24 words"))
}
