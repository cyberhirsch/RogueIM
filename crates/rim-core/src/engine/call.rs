//! 1:1 voice calls (no video, no groups).
//!
//! Signalling rides the normal end-to-end encrypted channel (`Body::Call`):
//! the caller rings every connected device of the contact with a fresh random
//! call key; the device that answers wins and tells its siblings to stop.
//! Audio frames (Opus, from the app) travel on their own request-response
//! protocol over the direct connection, each sealed with the call key.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use libp2p::{request_response, PeerId};

use super::msg::Target;
use super::*;
use crate::identity::{b64, now, unb64};

/// No answer after this long: give up.
const RING_SECS: i64 = 45;
/// Peer gone for this long during a call: it ended.
const LOST_SECS: i64 = 15;

pub(crate) struct CallRec {
    pub id: String,
    pub contact: String,
    /// The remote device in the call (known once it answers).
    pub device: Option<String>,
    pub key: [u8; 32],
    pub outgoing: bool,
    pub state: CallState,
    pub reason: String,
    pub started: i64,
    pub seq: u32,
    pub last_heard: i64,
    pub to_app: Option<std::sync::mpsc::Sender<(u32, Vec<u8>)>>,
}

/// The audio path between engine and app while a call is active.
pub struct MediaPipe {
    pub call: String,
    /// Encoded frames from the microphone, to the peer.
    pub send: mpsc::UnboundedSender<Vec<u8>>,
    /// (sequence number, encoded frame) from the peer. Taken once by the app.
    pub recv: Mutex<Option<std::sync::mpsc::Receiver<(u32, Vec<u8>)>>>,
}

impl std::fmt::Debug for MediaPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MediaPipe({})", self.call)
    }
}

fn seal(key: &[u8; 32], seq: u32, frame: &[u8]) -> Option<String> {
    let nonce: [u8; 24] = rand::random();
    let mut plain = seq.to_le_bytes().to_vec();
    plain.extend_from_slice(frame);
    let ct = XChaCha20Poly1305::new(&(*key).into()).encrypt(&XNonce::try_from(&nonce[..]).ok()?, plain.as_slice()).ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Some(b64(&out))
}

fn open(key: &[u8; 32], data: &str) -> Option<(u32, Vec<u8>)> {
    let raw = unb64(data).ok()?;
    if raw.len() < 24 + 4 {
        return None;
    }
    let plain = XChaCha20Poly1305::new(&(*key).into()).decrypt(&XNonce::try_from(&raw[..24]).ok()?, &raw[24..]).ok()?;
    let seq = u32::from_le_bytes(plain[..4].try_into().ok()?);
    Some((seq, plain[4..].to_vec()))
}

impl Engine {
    fn call_view(&self) -> Option<CallView> {
        let c = self.call.as_ref()?;
        let direct = c.device.as_ref().and_then(|d| d.parse::<PeerId>().ok()).map(|p| self.files_rt.direct.contains(&p)).unwrap_or(false);
        Some(CallView {
            call: c.id.clone(),
            contact: c.contact.clone(),
            name: self.contact(&c.contact).map(|x| x.petname.clone()).unwrap_or_default(),
            outgoing: c.outgoing,
            state: c.state,
            reason: c.reason.clone(),
            since: c.started,
            direct,
        })
    }

    pub fn emit_call(&self) {
        if let Some(v) = self.call_view() {
            self.emit(Event::Call(v));
        }
    }

    /// Devices of a contact we are connected to right now.
    fn connected_targets(&self, contact: &str) -> Vec<Target> {
        self.targets(contact).into_iter().filter(|t| self.is_connected_peer(&t.entry.peer_id)).collect()
    }

    fn call_to(&mut self, contact: &str, device: Option<&str>, sig: CallSignal) {
        let targets: Vec<Target> = self.connected_targets(contact).into_iter().filter(|t| device.map(|d| d == t.entry.peer_id).unwrap_or(true)).collect();
        for t in targets {
            let _ = self.send_to_target(&t, &Body::Call(sig.clone()), None, None);
        }
    }

    pub fn call_start(&mut self, contact: &str) -> Result<()> {
        if self.call.as_ref().map(|c| c.state != CallState::Ended).unwrap_or(false) {
            bail!("already in a call");
        }
        let c = self.contact(contact).ok_or_else(|| anyhow!("unknown contact"))?;
        if !c.authorized {
            bail!("{} has not authorized you yet", c.petname);
        }
        let name = c.petname.clone();
        if self.connected_targets(contact).is_empty() {
            // Try to get a connection for next time.
            for t in self.targets(contact) {
                if let Ok(p) = t.entry.peer_id.parse::<PeerId>() {
                    let _ = self.swarm.dial(p);
                }
            }
            bail!("{name} cannot be reached directly right now; calls need a direct connection. Try again in a moment.");
        }
        let key: [u8; 32] = rand::random();
        let id = hex::encode(rand::random::<[u8; 8]>());
        self.call = Some(CallRec { id: id.clone(), contact: contact.to_string(), device: None, key, outgoing: true, state: CallState::Calling, reason: String::new(), started: now(), seq: 0, last_heard: now(), to_app: None });
        self.call_to(contact, None, CallSignal::Invite { call: id, key: b64(&key) });
        self.emit_call();
        Ok(())
    }

    pub fn call_accept(&mut self) -> Result<()> {
        let Some(c) = self.call.as_mut() else { bail!("no call") };
        if c.outgoing || c.state != CallState::Ringing {
            bail!("nothing to answer");
        }
        let (id, contact, device) = (c.id.clone(), c.contact.clone(), c.device.clone().unwrap_or_default());
        c.state = CallState::Active;
        c.started = now();
        c.last_heard = now();
        self.call_to(&contact, Some(&device), CallSignal::Accept { call: id.clone() });
        let _ = self.send_body("self", Body::Call(CallSignal::Answered { call: id }), None);
        self.open_media();
        self.emit_call();
        Ok(())
    }

    pub fn call_decline(&mut self) {
        let Some(c) = self.call.as_ref() else { return };
        if c.outgoing || c.state != CallState::Ringing {
            return;
        }
        let (id, contact, device) = (c.id.clone(), c.contact.clone(), c.device.clone());
        self.call_to(&contact, device.as_deref(), CallSignal::Decline { call: id.clone() });
        let _ = self.send_body("self", Body::Call(CallSignal::Answered { call: id }), None);
        self.end_call("declined");
    }

    pub fn call_hangup(&mut self) {
        let Some(c) = self.call.as_ref() else { return };
        if c.state == CallState::Ended {
            return;
        }
        let (id, contact, device) = (c.id.clone(), c.contact.clone(), c.device.clone());
        self.call_to(&contact, device.as_deref(), CallSignal::Hangup { call: id });
        self.end_call("hung up");
    }

    fn end_call(&mut self, reason: &str) {
        if let Some(c) = self.call.as_mut() {
            c.state = CallState::Ended;
            c.reason = reason.to_string();
            c.to_app = None;
        }
        self.call_out = None;
        self.emit_call();
    }

    fn open_media(&mut self) {
        let Some(c) = self.call.as_mut() else { return };
        let (to_net, from_app) = mpsc::unbounded_channel();
        let (to_app, from_net) = std::sync::mpsc::channel();
        c.to_app = Some(to_app);
        let pipe = MediaPipe { call: c.id.clone(), send: to_net, recv: Mutex::new(Some(from_net)) };
        self.call_out = Some(from_app);
        self.emit(Event::CallMedia(Arc::new(pipe)));
    }

    /// A signal from a contact's device.
    pub fn on_call_signal(&mut self, contact: &str, device: &str, sig: CallSignal) {
        let sig_kind = match &sig {
            CallSignal::Decline { .. } => "decline",
            CallSignal::Busy { .. } => "busy",
            _ => "other",
        };
        match sig {
            CallSignal::Invite { call, key } => {
                let busy = self.call.as_ref().map(|c| c.state != CallState::Ended).unwrap_or(false);
                if busy {
                    self.call_to(contact, Some(device), CallSignal::Busy { call });
                    return;
                }
                let Some(key) = unb64(&key).ok().and_then(|k| <[u8; 32]>::try_from(k).ok()) else { return };
                self.call = Some(CallRec { id: call, contact: contact.to_string(), device: Some(device.to_string()), key, outgoing: false, state: CallState::Ringing, reason: String::new(), started: now(), seq: 0, last_heard: now(), to_app: None });
                self.emit_call();
            }
            CallSignal::Accept { call } => {
                let ok = self.call.as_ref().map(|c| c.id == call && c.outgoing && c.state == CallState::Calling && c.contact == contact).unwrap_or(false);
                if !ok {
                    return;
                }
                if let Some(c) = self.call.as_mut() {
                    c.device = Some(device.to_string());
                    c.state = CallState::Active;
                    c.started = now();
                    c.last_heard = now();
                }
                self.open_media();
                self.emit_call();
            }
            CallSignal::Decline { call } | CallSignal::Busy { call } | CallSignal::Hangup { call } => {
                let matches = self.call.as_ref().map(|c| c.id == call && c.contact == contact && c.state != CallState::Ended && c.device.as_deref().map(|d| d == device).unwrap_or(true)).unwrap_or(false);
                if !matches {
                    return;
                }
                let reason = match sig_kind {
                    "decline" => "declined",
                    "busy" => "busy",
                    _ => "hung up",
                };
                self.end_call(reason);
            }
            CallSignal::Answered { .. } => {}
        }
    }

    /// One of our own devices answered or declined: stop ringing here.
    pub fn on_own_call_signal(&mut self, sig: CallSignal) {
        if let CallSignal::Answered { call } = sig {
            let ringing = self.call.as_ref().map(|c| c.id == call && c.state == CallState::Ringing).unwrap_or(false);
            if ringing {
                self.end_call("answered on another device");
            }
        }
    }

    /// An encoded frame from the app: seal and send.
    pub fn call_send_frame(&mut self, frame: Vec<u8>) {
        let Some(c) = self.call.as_mut() else { return };
        if c.state != CallState::Active {
            return;
        }
        let Some(dev) = c.device.clone().and_then(|d| d.parse::<PeerId>().ok()) else { return };
        let seq = c.seq;
        c.seq = c.seq.wrapping_add(1);
        let Some(data) = seal(&c.key, seq, &frame) else { return };
        let req = VoiceReq { call: c.id.clone(), data };
        self.swarm.behaviour_mut().voice.send_request(&dev, req);
    }

    pub fn on_voice_rr(&mut self, ev: request_response::Event<VoiceReq, VoiceResp>) {
        use request_response::{Event as E, Message as M};
        if let E::Message { peer, message: M::Request { request, channel, .. }, .. } = ev {
            let _ = self.swarm.behaviour_mut().voice.send_response(channel, VoiceResp {});
            let Some(c) = self.call.as_mut() else { return };
            if c.state != CallState::Active || c.id != request.call || c.device.as_deref() != Some(peer.to_string().as_str()) {
                return;
            }
            if let Some(frame) = open(&c.key, &request.data) {
                c.last_heard = now();
                if let Some(tx) = &c.to_app {
                    let _ = tx.send(frame);
                }
            }
        }
    }

    /// Timeouts: nobody answers, or the other side vanished.
    pub fn call_heartbeat(&mut self, now: i64) {
        let Some(c) = self.call.as_ref() else { return };
        match c.state {
            CallState::Calling | CallState::Ringing if now - c.started > RING_SECS => {
                if c.outgoing {
                    self.call_hangup();
                    if let Some(c) = self.call.as_mut() {
                        c.reason = "no answer".into();
                    }
                    self.emit_call();
                } else {
                    self.end_call("missed");
                }
            }
            CallState::Active => {
                let gone = c.device.as_deref().map(|d| !self.is_connected_peer(d)).unwrap_or(true);
                if gone && now - c.last_heard > LOST_SECS {
                    self.end_call("connection lost");
                } else {
                    // Keep the "direct / relayed" indicator current.
                    self.emit_call();
                }
            }
            _ => {}
        }
    }
}
