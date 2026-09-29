//! Account key, device key, Olm account, cards, invites and fingerprints.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine as _;
use libp2p::identity::{Keypair, PublicKey};
use sha2::{Digest, Sha256};
use vodozemac::Curve25519PublicKey;

use crate::proto::{Card, Invite};

pub const INVITE_PREFIX: &str = "rim1:";

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    Ok(B64.decode(s)?)
}

/// Full fingerprint of an account key: hex SHA-256 of its encoding.
/// Used as the stable contact id.
pub fn account_id(account_pk_b64: &str) -> Result<String> {
    let bytes = unb64(account_pk_b64)?;
    Ok(hex::encode(Sha256::digest(bytes)))
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

fn device_binding(peer_id: &str, curve: &str) -> Vec<u8> {
    let mut m = b"rim-device-v1|".to_vec();
    m.extend_from_slice(peer_id.as_bytes());
    m.push(b'|');
    m.extend_from_slice(curve.as_bytes());
    m
}

/// Build our card: the account key vouches for this device's PeerId and Olm key.
pub fn make_card(nick: &str, account: &Keypair, device: &Keypair, curve: &Curve25519PublicKey) -> Result<Card> {
    let peer_id = device.public().to_peer_id().to_string();
    let curve = curve.to_base64();
    let sig = account.sign(&device_binding(&peer_id, &curve))?;
    Ok(Card {
        nick: nick.to_string(),
        account_pk: b64(&account.public().encode_protobuf()),
        peer_id,
        curve,
        device_sig: b64(&sig),
    })
}

/// Check that the card's device is really signed by its account key.
pub fn verify_card(card: &Card) -> Result<()> {
    let pk = PublicKey::try_decode_protobuf(&unb64(&card.account_pk)?).context("account key")?;
    let sig = unb64(&card.device_sig)?;
    if !pk.verify(&device_binding(&card.peer_id, &card.curve), &sig) {
        bail!("device signature invalid");
    }
    Curve25519PublicKey::from_base64(&card.curve).map_err(|e| anyhow!("curve key: {e}"))?;
    card.peer_id.parse::<libp2p::PeerId>().context("peer id")?;
    Ok(())
}

pub fn encode_invite(inv: &Invite) -> Result<String> {
    Ok(format!("{INVITE_PREFIX}{}", B64URL.encode(serde_json::to_vec(inv)?)))
}

pub fn decode_invite(s: &str) -> Result<Invite> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let body = s.strip_prefix(INVITE_PREFIX).ok_or_else(|| anyhow!("not a RogueIM invite (must start with {INVITE_PREFIX})"))?;
    let inv: Invite = serde_json::from_slice(&B64URL.decode(body).context("invite encoding")?)?;
    if inv.v != 1 {
        bail!("unsupported invite version {}", inv.v);
    }
    verify_card(&inv.card)?;
    Ok(inv)
}
