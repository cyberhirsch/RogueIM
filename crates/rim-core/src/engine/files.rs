//! Direct-only file transfer (PRD FT-1..6). The offer (with the per-file key and
//! BLAKE3 hash) travels Olm-encrypted; chunks are pulled over `/rim/file/1` and
//! only ever over a direct (non-relayed) connection.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use libp2p::request_response::{self, OutboundRequestId};
use libp2p::PeerId;

use super::*;
use crate::identity::{b64, now, unb64};

pub const CHUNK: u64 = 64 * 1024;
const PIPELINE: usize = 8;

#[derive(Default)]
pub struct FilesRuntime {
    /// Peers with at least one direct connection.
    pub direct: HashSet<PeerId>,
    pub inflight: HashMap<OutboundRequestId, (String, u64)>,
    pub per_file: HashMap<String, HashSet<u64>>,
    pub dirty: bool,
}

impl FilesRuntime {
    pub fn connection_up(&mut self, peer: PeerId, direct: bool) {
        if direct {
            self.direct.insert(peer);
        }
    }
    pub fn connection_down(&mut self, peer: PeerId) {
        self.direct.remove(&peer);
    }
}

fn nonce(file: &str, index: u64) -> [u8; 24] {
    let h = blake3::hash(format!("{file}|{index}").as_bytes());
    let mut n = [0u8; 24];
    n.copy_from_slice(&h.as_bytes()[..24]);
    n
}

fn key_of(offer: &FileOffer) -> Result<[u8; 32]> {
    unb64(&offer.key)?.try_into().map_err(|_| anyhow!("bad file key"))
}

fn chunks_of(offer: &FileOffer) -> u64 {
    offer.size.div_ceil(offer.chunk).max(1)
}

fn part_path(p: &str) -> PathBuf {
    PathBuf::from(format!("{p}.part"))
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let name = super::account::sanitize(name);
    let mut p = dir.join(&name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut i = 1;
    while p.exists() || part_path(&p.to_string_lossy()).exists() {
        p = dir.join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    p
}

impl Engine {
    pub fn file_view(&self, id: &str) -> Option<FileView> {
        let f = self.p.files.iter().find(|f| f.offer.id == id)?;
        Some(FileView {
            id: id.to_string(),
            name: f.offer.name.clone(),
            size: f.offer.size,
            done: (f.done_chunks.len() as u64 * f.offer.chunk).min(f.offer.size),
            state: f.state,
            path: f.path.clone(),
        })
    }

    pub fn emit_files(&self) {
        let v = self.p.files.iter().filter_map(|f| self.file_view(&f.offer.id).map(|v| (f.peer.clone(), v))).collect();
        self.emit(Event::Files(v));
    }

    fn file_line(&mut self, peer: &str, id: &str, from_me: bool, name: &str, ts: i64) {
        let mut l = LineRec::text(rand::random::<u64>() | 1, ts, from_me, format!("[file] {name}"), if from_me { Delivery::Delivered } else { Delivery::Received });
        l.file = Some(id.to_string());
        if peer == "self" {
            self.p.notes.push(l);
        } else if let Some(c) = self.contact_mut(peer) {
            c.history.push(l);
        }
    }

    fn refresh_file_views(&mut self, peer: &str) {
        self.emit_files();
        if peer == "self" {
            self.emit_notes();
        } else {
            self.emit_history(peer);
        }
    }

    pub fn send_file(&mut self, id: &str, path: &str) -> Result<()> {
        let meta = std::fs::metadata(path).with_context(|| format!("cannot read {path}"))?;
        if !meta.is_file() {
            bail!("{path} is not a file");
        }
        if id != "self" {
            let c = self.contact(id).ok_or_else(|| anyhow!("unknown contact"))?;
            if !c.authorized {
                bail!("{} has not authorized you yet", c.petname);
            }
        }
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let hash = tokio::task::block_in_place(|| -> Result<String> {
            let mut h = blake3::Hasher::new();
            let mut f = std::fs::File::open(path)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(h.finalize().to_hex().to_string())
        })?;
        let key: [u8; 32] = rand::random();
        let offer = FileOffer { id: hex::encode(rand::random::<[u8; 12]>()), name: name.clone(), size: meta.len(), chunk: CHUNK, hash, key: b64(&key), ts: now() };
        self.p.files.push(FileRec { offer: offer.clone(), peer: id.to_string(), outgoing: true, path: path.to_string(), done_chunks: vec![], state: FileState::Offered });
        self.file_line(id, &offer.id, true, &name, offer.ts);
        self.send_body(id, Body::FileOffer(offer), Some(0))?;
        self.save();
        self.refresh_file_views(id);
        Ok(())
    }

    pub fn on_file_offer(&mut self, peer: &str, o: FileOffer) {
        if self.p.files.iter().any(|f| f.offer.id == o.id) {
            return;
        }
        let from = if peer == "self" { "your other device".to_string() } else { self.contact(peer).map(|c| c.petname.clone()).unwrap_or_default() };
        self.file_line(peer, &o.id, false, &o.name, o.ts);
        self.p.files.push(FileRec { offer: o.clone(), peer: peer.to_string(), outgoing: false, path: String::new(), done_chunks: vec![], state: FileState::Offered });
        self.save();
        self.emit(Event::FileOffered { id: o.id, from, name: o.name, size: o.size });
        if peer != "self" {
            if let Some(c) = self.contact_mut(peer) {
                c.unread += 1;
            }
            self.emit_contacts();
        }
        self.refresh_file_views(peer);
    }

    pub fn accept_file(&mut self, file: &str, dir: &str) -> Result<()> {
        let dir = if dir.is_empty() {
            directories::UserDirs::new().and_then(|u| u.download_dir().map(|d| d.join("RogueIM"))).unwrap_or_else(|| self.store_dir().join("files"))
        } else {
            PathBuf::from(dir)
        };
        std::fs::create_dir_all(&dir)?;
        let f = self.p.files.iter_mut().find(|f| f.offer.id == file && !f.outgoing).ok_or_else(|| anyhow!("no such file offer"))?;
        let path = unique_path(&dir, &f.offer.name);
        f.path = path.to_string_lossy().to_string();
        f.state = FileState::Waiting;
        let peer = f.peer.clone();
        std::fs::File::create(part_path(&f.path))?;
        self.send_body(&peer, Body::FileAccept { id: file.to_string() }, Some(0))?;
        self.save();
        self.refresh_file_views(&peer);
        Ok(())
    }

    pub fn decline_file(&mut self, file: &str) {
        if let Some(f) = self.p.files.iter_mut().find(|f| f.offer.id == file) {
            f.state = FileState::Declined;
            let peer = f.peer.clone();
            let _ = self.send_body(&peer, Body::FileDecline { id: file.to_string() }, Some(0));
            self.save();
            self.refresh_file_views(&peer);
        }
    }

    pub fn cancel_file(&mut self, file: &str) {
        if let Some(f) = self.p.files.iter_mut().find(|f| f.offer.id == file) {
            f.state = FileState::Failed;
            let peer = f.peer.clone();
            let path = f.path.clone();
            let outgoing = f.outgoing;
            let _ = self.send_body(&peer, Body::FileCancel { id: file.to_string() }, Some(0));
            if !outgoing {
                let _ = std::fs::remove_file(part_path(&path));
            }
            self.save();
            self.refresh_file_views(&peer);
        }
    }

    pub fn set_file_state(&mut self, file: &str, s: FileState) {
        if let Some(f) = self.p.files.iter_mut().find(|f| f.offer.id == file && !matches!(f.state, FileState::Done | FileState::Declined | FileState::Failed)) {
            f.state = s;
            let peer = f.peer.clone();
            self.save();
            self.refresh_file_views(&peer);
        }
    }

    pub fn on_file_accept(&mut self, _peer: &str, _device: &str, file: &str) {
        if let Some(f) = self.p.files.iter_mut().find(|f| f.offer.id == file && f.outgoing) {
            f.state = FileState::Transferring;
            let peer = f.peer.clone();
            self.save();
            self.refresh_file_views(&peer);
        }
    }

    pub fn on_file_declined(&mut self, file: &str) {
        if let Some(f) = self.p.files.iter_mut().find(|f| f.offer.id == file) {
            if f.state != FileState::Done {
                f.state = FileState::Declined;
            }
            let peer = f.peer.clone();
            self.save();
            self.refresh_file_views(&peer);
        }
    }

    /// Pull missing chunks for accepted incoming files over direct connections.
    pub fn files_tick(&mut self) {
        let active: Vec<(String, String)> = self
            .p
            .files
            .iter()
            .filter(|f| !f.outgoing && matches!(f.state, FileState::Waiting | FileState::Transferring | FileState::NoDirect))
            .map(|f| (f.offer.id.clone(), f.peer.clone()))
            .collect();
        for (id, owner) in active {
            let device = self
                .targets(&owner)
                .into_iter()
                .filter_map(|t| t.entry.peer_id.parse::<PeerId>().ok())
                .find(|p| self.files_rt.direct.contains(p));
            let Some(dev) = device else {
                if let Some(f) = self.p.files.iter_mut().find(|f| f.offer.id == id) {
                    if f.state != FileState::NoDirect {
                        f.state = FileState::NoDirect;
                        self.files_rt.dirty = true;
                    }
                }
                // Keep trying to open a connection (hole punching, LAN, IPv6).
                let targets = self.targets(&owner);
                for t in targets {
                    if let Ok(pid) = t.entry.peer_id.parse::<PeerId>() {
                        if !self.swarm.is_connected(&pid) {
                            let _ = self.swarm.dial(pid);
                        }
                    }
                }
                continue;
            };
            let f = self.p.files.iter_mut().find(|f| f.offer.id == id).unwrap();
            if f.state == FileState::NoDirect || f.state == FileState::Waiting {
                f.state = FileState::Transferring;
                self.files_rt.dirty = true;
            }
            let total = chunks_of(&f.offer);
            let done: HashSet<u64> = f.done_chunks.iter().copied().collect();
            let inflight = self.files_rt.per_file.entry(id.clone()).or_default();
            let mut next = 0u64;
            while inflight.len() < PIPELINE && next < total {
                if !done.contains(&next) && !inflight.contains(&next) {
                    let rid = self.swarm.behaviour_mut().file.send_request(&dev, ChunkReq { file_id: id.clone(), index: next });
                    inflight.insert(next);
                    self.files_rt.inflight.insert(rid, (id.clone(), next));
                }
                next += 1;
            }
        }
        if self.files_rt.dirty {
            self.files_rt.dirty = false;
            self.save();
            self.emit_files();
        }
    }

    pub fn on_file_rr(&mut self, ev: request_response::Event<ChunkReq, ChunkResp>) {
        use request_response::{Event as E, Message as M};
        match ev {
            E::Message { peer, message: M::Request { request, channel, .. }, .. } => {
                let resp = self.serve_chunk(peer, &request);
                let _ = self.swarm.behaviour_mut().file.send_response(channel, resp);
            }
            E::Message { message: M::Response { request_id, response }, .. } => {
                if let Some((file, index)) = self.files_rt.inflight.remove(&request_id) {
                    if let Some(s) = self.files_rt.per_file.get_mut(&file) {
                        s.remove(&index);
                    }
                    self.store_chunk(&file, index, response);
                }
            }
            E::OutboundFailure { request_id, .. } => {
                if let Some((file, index)) = self.files_rt.inflight.remove(&request_id) {
                    if let Some(s) = self.files_rt.per_file.get_mut(&file) {
                        s.remove(&index);
                    }
                }
            }
            _ => {}
        }
    }

    fn serve_chunk(&mut self, peer: PeerId, req: &ChunkReq) -> ChunkResp {
        let refuse = |e: &str| ChunkResp { file_id: req.file_id.clone(), index: req.index, data: String::new(), error: Some(e.into()) };
        if !self.files_rt.direct.contains(&peer) {
            return refuse("direct connections only");
        }
        let Some(f) = self.p.files.iter().find(|f| f.offer.id == req.file_id && f.outgoing).cloned() else { return refuse("unknown file") };
        if matches!(f.state, FileState::Declined | FileState::Failed | FileState::Paused) {
            return refuse("transfer stopped");
        }
        let allowed = self.targets(&f.peer).iter().any(|t| t.entry.peer_id == peer.to_string());
        if !allowed {
            return refuse("not the recipient");
        }
        let read = || -> Result<Vec<u8>> {
            let mut file = std::fs::File::open(&f.path)?;
            file.seek(SeekFrom::Start(req.index * f.offer.chunk))?;
            let mut buf = vec![0u8; f.offer.chunk as usize];
            let mut n = 0;
            while n < buf.len() {
                let r = file.read(&mut buf[n..])?;
                if r == 0 {
                    break;
                }
                n += r;
            }
            buf.truncate(n);
            let key = key_of(&f.offer)?;
            let c = XChaCha20Poly1305::new(&key.into());
            c.encrypt(&XNonce::from(nonce(&f.offer.id, req.index)), buf.as_slice()).map_err(|_| anyhow!("encrypt"))
        };
        match read() {
            Ok(ct) => {
                if let Some(fr) = self.p.files.iter_mut().find(|x| x.offer.id == req.file_id) {
                    if fr.state == FileState::Offered {
                        fr.state = FileState::Transferring;
                    }
                    if req.index + 1 == chunks_of(&fr.offer) {
                        fr.state = FileState::Done;
                        self.files_rt.dirty = true;
                    }
                }
                ChunkResp { file_id: req.file_id.clone(), index: req.index, data: b64(&ct), error: None }
            }
            Err(e) => refuse(&format!("{e}")),
        }
    }

    fn store_chunk(&mut self, file: &str, index: u64, resp: ChunkResp) {
        let Some(pos) = self.p.files.iter().position(|f| f.offer.id == file && !f.outgoing) else { return };
        if let Some(e) = resp.error {
            let f = &mut self.p.files[pos];
            if e.contains("direct") {
                f.state = FileState::NoDirect;
            } else if e.contains("stopped") {
                f.state = FileState::Paused;
            }
            self.files_rt.dirty = true;
            return;
        }
        let f = self.p.files[pos].clone();
        if f.done_chunks.contains(&index) || f.state == FileState::Paused {
            return;
        }
        let write = || -> Result<()> {
            let key = key_of(&f.offer)?;
            let c = XChaCha20Poly1305::new(&key.into());
            let plain = c.decrypt(&XNonce::from(nonce(&f.offer.id, index)), unb64(&resp.data)?.as_slice()).map_err(|_| anyhow!("chunk does not decrypt"))?;
            let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(part_path(&f.path))?;
            out.seek(SeekFrom::Start(index * f.offer.chunk))?;
            out.write_all(&plain)?;
            Ok(())
        };
        if write().is_err() {
            self.p.files[pos].state = FileState::Failed;
            self.files_rt.dirty = true;
            return;
        }
        self.p.files[pos].done_chunks.push(index);
        self.files_rt.dirty = self.p.files[pos].done_chunks.len() % 16 == 0 || self.files_rt.dirty;
        if self.p.files[pos].done_chunks.len() as u64 == chunks_of(&f.offer) {
            // Verify before releasing the file to the user.
            let ok = (|| -> Result<bool> {
                let mut h = blake3::Hasher::new();
                let mut file = std::fs::File::open(part_path(&f.path))?;
                file.set_len(f.offer.size).ok();
                let mut buf = vec![0u8; 1 << 20];
                let mut total = 0u64;
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    let take = n.min((f.offer.size - total) as usize);
                    h.update(&buf[..take]);
                    total += take as u64;
                    if total >= f.offer.size {
                        break;
                    }
                }
                Ok(h.finalize().to_hex().to_string() == f.offer.hash)
            })()
            .unwrap_or(false);
            let fr = &mut self.p.files[pos];
            let msg = if ok {
                let _ = std::fs::OpenOptions::new().write(true).open(part_path(&fr.path)).and_then(|x| x.set_len(fr.offer.size));
                let _ = std::fs::rename(part_path(&fr.path), &fr.path);
                fr.state = FileState::Done;
                format!("Received {} ({}).", fr.offer.name, human_size(fr.offer.size))
            } else {
                fr.state = FileState::Failed;
                let _ = std::fs::remove_file(part_path(&fr.path));
                format!("{} failed the integrity check and was discarded.", fr.offer.name)
            };
            let peer = fr.peer.clone();
            self.notice(msg);
            self.save();
            self.refresh_file_views(&peer);
        }
    }
}

pub fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}
