//! Screen sharing inside an active 1:1 call (view only, no remote control).
//!
//! The app captures and encodes (H.264); the engine only moves frames: each
//! frame is split into parts of at most 192 KiB, every part sealed with the
//! call key (the frame header is authenticated as associated data), and sent
//! on `/rim/screen/1` over the direct connection. The viewer reassembles and
//! hands whole frames to the app. A viewer that lost track asks for a
//! keyframe; a sender with too many parts in flight skips frames.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use libp2p::{request_response, PeerId};

use super::*;
use crate::identity::{b64, unb64};

const PART: usize = 192 * 1024;

#[derive(Debug, Clone)]
pub struct ScreenFrame {
    pub seq: u32,
    /// A keyframe: decodable on its own.
    pub key: bool,
    pub data: Vec<u8>,
}

/// Frames between app and engine. The sender's app gets `send`, the
/// viewer's app gets `recv`.
pub struct ScreenPipe {
    pub call: String,
    pub send: Option<mpsc::UnboundedSender<ScreenFrame>>,
    pub recv: Mutex<Option<std::sync::mpsc::Receiver<ScreenFrame>>>,
    /// Sender: the viewer wants a keyframe next.
    pub want_key: Arc<AtomicBool>,
    /// Sender: parts still on their way (skip capturing while this is high).
    pub in_flight: Arc<AtomicUsize>,
}

impl std::fmt::Debug for ScreenPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScreenPipe({})", self.call)
    }
}

#[derive(Default)]
pub(crate) struct ScreenState {
    /// We share.
    pub out: bool,
    /// They share.
    pub incoming: bool,
    pub seq: u32,
    pub want_key: Arc<AtomicBool>,
    pub in_flight: Arc<AtomicUsize>,
    pub to_app: Option<std::sync::mpsc::Sender<ScreenFrame>>,
    /// seq -> (keyframe, parts received)
    pub partial: HashMap<u32, (bool, Vec<Option<Vec<u8>>>)>,
    /// Viewer: next frame to hand to the decoder, and complete frames waiting for it.
    pub next: Option<u32>,
    pub ready: std::collections::BTreeMap<u32, ScreenFrame>,
    pub requests: HashMap<request_response::OutboundRequestId, ()>,
}

fn aad(call: &str, seq: u32, part: u16, parts: u16, key: bool) -> Vec<u8> {
    format!("rim-screen|{call}|{seq}|{part}|{parts}|{key}").into_bytes()
}

impl Engine {
    fn screen_peer(&self) -> Option<(PeerId, [u8; 32], String)> {
        let c = self.call.as_ref()?;
        if c.state != CallState::Active {
            return None;
        }
        let dev = c.device.as_ref()?.parse::<PeerId>().ok()?;
        Some((dev, c.key, c.id.clone()))
    }

    pub fn screen_share(&mut self, on: bool) -> Result<()> {
        let Some((dev, _, id)) = self.screen_peer() else { bail!("screen sharing works during a call") };
        if on {
            if !self.files_rt.direct.contains(&dev) {
                bail!("screen sharing needs a direct connection; this call runs over a relay");
            }
            let (tx, rx) = mpsc::unbounded_channel();
            let st = &mut self.call.as_mut().unwrap().screen;
            st.out = true;
            st.want_key.store(true, Ordering::Relaxed);
            st.in_flight.store(0, Ordering::Relaxed);
            let pipe = ScreenPipe { call: id.clone(), send: Some(tx), recv: Mutex::new(None), want_key: st.want_key.clone(), in_flight: st.in_flight.clone() };
            self.screen_out = Some(rx);
            let (contact, device) = self.call.as_ref().map(|c| (c.contact.clone(), c.device.clone())).unwrap();
            self.call_to_device(&contact, device.as_deref(), CallSignal::ScreenOn { call: id });
            self.emit(Event::ScreenOut(Arc::new(pipe)));
        } else {
            self.screen_stop_local();
        }
        self.emit_call();
        Ok(())
    }

    pub fn screen_stop_local(&mut self) {
        let Some(c) = self.call.as_mut() else { return };
        if !c.screen.out {
            return;
        }
        c.screen.out = false;
        let (contact, device, id) = (c.contact.clone(), c.device.clone(), c.id.clone());
        self.screen_out = None;
        self.call_to_device(&contact, device.as_deref(), CallSignal::ScreenOff { call: id });
    }

    /// The viewer asks for a keyframe.
    pub fn screen_want_key(&mut self) {
        let Some(c) = self.call.as_ref() else { return };
        if c.screen.incoming {
            let (contact, device, id) = (c.contact.clone(), c.device.clone(), c.id.clone());
            self.call_to_device(&contact, device.as_deref(), CallSignal::ScreenKey { call: id });
        }
    }

    /// Screen signals from the other side of our call.
    pub fn on_screen_signal(&mut self, device: &str, sig: CallSignal) {
        let Some(c) = self.call.as_mut() else { return };
        if c.state != CallState::Active || c.device.as_deref() != Some(device) {
            return;
        }
        match sig {
            CallSignal::ScreenOn { call } if call == c.id => {
                let (tx, rx) = std::sync::mpsc::channel();
                c.screen.incoming = true;
                c.screen.to_app = Some(tx);
                c.screen.partial.clear();
                c.screen.ready.clear();
                c.screen.next = None;
                let pipe = ScreenPipe { call, send: None, recv: Mutex::new(Some(rx)), want_key: Default::default(), in_flight: Default::default() };
                self.emit(Event::ScreenIn(Arc::new(pipe)));
                self.emit_call();
            }
            CallSignal::ScreenOff { call } if call == c.id => {
                c.screen.incoming = false;
                c.screen.to_app = None;
                c.screen.partial.clear();
                self.emit_call();
            }
            CallSignal::ScreenKey { call } if call == c.id => c.screen.want_key.store(true, Ordering::Relaxed),
            _ => {}
        }
    }

    /// An encoded frame from our app: split, seal, send.
    pub fn screen_send_frame(&mut self, f: ScreenFrame) {
        let Some((dev, key, id)) = self.screen_peer() else { return };
        let seq = {
            let st = &mut self.call.as_mut().unwrap().screen;
            if !st.out {
                return;
            }
            let s = st.seq;
            st.seq = st.seq.wrapping_add(1);
            s
        };
        let chunks: Vec<&[u8]> = if f.data.is_empty() { vec![&[][..]] } else { f.data.chunks(PART).collect() };
        let parts = chunks.len() as u16;
        let cipher = XChaCha20Poly1305::new(&key.into());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let nonce: [u8; 24] = rand::random();
            let Ok(n) = XNonce::try_from(&nonce[..]) else { return };
            let Ok(ct) = cipher.encrypt(&n, Payload { msg: chunk, aad: &aad(&id, seq, i as u16, parts, f.key) }) else { return };
            let mut data = nonce.to_vec();
            data.extend_from_slice(&ct);
            let req = ScreenReq { call: id.clone(), seq, part: i as u16, parts, key: f.key, data: b64(&data) };
            let rid = self.swarm.behaviour_mut().screen.send_request(&dev, req);
            let st = &mut self.call.as_mut().unwrap().screen;
            st.requests.insert(rid, ());
            st.in_flight.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn on_screen_rr(&mut self, ev: request_response::Event<ScreenReq, ScreenResp>) {
        use request_response::{Event as E, Message as M};
        match ev {
            E::Message { peer, message: M::Request { request: r, channel, .. }, .. } => {
                let _ = self.swarm.behaviour_mut().screen.send_response(channel, ScreenResp {});
                let Some(c) = self.call.as_mut() else { return };
                if !c.screen.incoming || c.id != r.call || c.device.as_deref() != Some(peer.to_string().as_str()) || r.parts == 0 || r.part >= r.parts {
                    return;
                }
                let Ok(raw) = unb64(&r.data) else { return };
                if raw.len() < 24 {
                    return;
                }
                let cipher = XChaCha20Poly1305::new(&c.key.into());
                let Ok(n) = XNonce::try_from(&raw[..24]) else { return };
                let Ok(plain) = cipher.decrypt(&n, Payload { msg: &raw[24..], aad: &aad(&r.call, r.seq, r.part, r.parts, r.key) }) else { return };
                let st = &mut c.screen;
                let entry = st.partial.entry(r.seq).or_insert_with(|| (r.key, vec![None; r.parts as usize]));
                if entry.1.len() != r.parts as usize {
                    return;
                }
                entry.1[r.part as usize] = Some(plain);
                if entry.1.iter().all(Option::is_some) {
                    let (key, parts) = st.partial.remove(&r.seq).unwrap();
                    let data: Vec<u8> = parts.into_iter().flatten().flatten().collect();
                    let frame = ScreenFrame { seq: r.seq, key, data };
                    // Frames reach the decoder in order. A keyframe may jump ahead
                    // (everything before it is useless); a gap that lasts too long
                    // is healed by asking for a keyframe.
                    // The decoder can only start at a keyframe.
                    if st.next.is_none() && key {
                        st.next = Some(r.seq);
                    }
                    let next = st.next.unwrap_or(0);
                    if st.next.is_some() && r.seq < next {
                        return;
                    }
                    if key && r.seq > next {
                        st.ready.retain(|s, _| *s > r.seq);
                        st.partial.retain(|s, _| *s > r.seq);
                        st.next = Some(r.seq);
                    }
                    st.ready.insert(r.seq, frame);
                    while let Some(f) = st.next.and_then(|n| st.ready.remove(&n)) {
                        st.next = Some(f.seq.wrapping_add(1));
                        if let Some(tx) = &st.to_app {
                            let _ = tx.send(f);
                        }
                    }
                    let stuck = st.ready.len() > 12;
                    if stuck {
                        st.ready.clear();
                        let (contact, device, id) = (c.contact.clone(), c.device.clone(), c.id.clone());
                        self.call_to_device(&contact, device.as_deref(), CallSignal::ScreenKey { call: id });
                    }
                }
            }
            E::Message { message: M::Response { request_id, .. }, .. } | E::OutboundFailure { request_id, .. } => {
                if let Some(c) = self.call.as_mut() {
                    if c.screen.requests.remove(&request_id).is_some() {
                        let _ = c.screen.in_flight.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(1)));
                    }
                }
            }
            _ => {}
        }
    }
}
