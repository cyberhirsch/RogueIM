//! Sealing/opening envelopes, the outbox, and everything that arrives in one:
//! authorization, presence, text, sync between own devices.

use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use libp2p::request_response::{self};
use libp2p::PeerId;
use vodozemac::olm::{OlmMessage, Session, SessionConfig};
use vodozemac::Curve25519PublicKey;

use super::*;
use crate::identity::{self, account_id, now};

/// How a message reached us.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via {
    Direct,
    Mailbox,
    Relay,
}

/// A device we can send to.
#[derive(Clone)]
pub struct Target {
    pub owner: String,
    pub entry: DeviceEntry,
    pub seeds: Option<DeviceSeeds>,
    pub addrs: Vec<String>,
}

impl Engine {
    // ================================================================ identity helpers

    pub fn my_card(&self) -> Card {
        Card { nick: self.p.nick.clone(), devices: self.p.devices.clone().expect("device list") }
    }

    /// All devices of an owner we can address ("self" = our other devices).
    pub fn targets(&self, owner: &str) -> Vec<Target> {
        let me = self.peer_id.to_string();
        if owner == "self" {
            let Some(l) = &self.p.devices else { return vec![] };
            return l
                .list
                .devices
                .iter()
                .filter(|d| d.peer_id != me)
                .map(|d| Target {
                    owner: "self".into(),
                    entry: d.clone(),
                    seeds: self.p.own_seeds.get(&d.peer_id).cloned(),
                    addrs: self.p.own_addrs.get(&d.peer_id).cloned().unwrap_or_default(),
                })
                .collect();
        }
        if let Some(c) = self.contact(owner) {
            return c
                .devices
                .iter()
                .map(|d| Target { owner: owner.into(), entry: d.entry.clone(), seeds: d.seeds.clone(), addrs: d.addrs.clone() })
                .collect();
        }
        // Group members that are not contacts.
        for g in &self.p.groups {
            if let Some(m) = g.state.members.iter().find(|m| m.account == owner) {
                return m
                    .devices
                    .list
                    .devices
                    .iter()
                    .map(|d| Target {
                        owner: owner.into(),
                        entry: d.clone(),
                        seeds: m.seeds.iter().find(|(p, _)| p == &d.peer_id).map(|(_, s)| s.clone()),
                        addrs: m.addrs.iter().find(|(p, _)| p == &d.peer_id).map(|(_, a)| a.clone()).unwrap_or_default(),
                    })
                    .collect();
            }
        }
        vec![]
    }

    /// Device entry and owner for a sender's Olm identity key.
    pub fn owner_of_curve(&self, curve: &str) -> Option<(String, DeviceEntry)> {
        if let Some(l) = &self.p.devices {
            if let Some(d) = l.list.devices.iter().find(|d| d.curve == curve) {
                return Some(("self".into(), d.clone()));
            }
        }
        for c in &self.p.contacts {
            if c.removed {
                continue;
            }
            if let Some(d) = c.devices.iter().find(|d| d.entry.curve == curve) {
                return Some((c.id.clone(), d.entry.clone()));
            }
        }
        for p in &self.p.pending {
            if let Some(d) = p.card.devices.list.devices.iter().find(|d| d.curve == curve) {
                return Some((format!("pending:{}", p.id), d.clone()));
            }
        }
        for g in &self.p.groups {
            for m in &g.state.members {
                if let Some(d) = m.devices.list.devices.iter().find(|d| d.curve == curve) {
                    return Some((m.account.clone(), d.clone()));
                }
            }
        }
        None
    }

    // ================================================================ sessions

    fn session_for(&mut self, entry: &DeviceEntry, otk: Option<&str>) -> Result<&mut Session> {
        if self.sessions.get(&entry.curve).map(|v| v.is_empty()).unwrap_or(true) {
            let curve = Curve25519PublicKey::from_base64(&entry.curve).map_err(|e| anyhow!("{e}"))?;
            let key = Curve25519PublicKey::from_base64(otk.filter(|k| !k.is_empty()).unwrap_or(&entry.fallback)).map_err(|e| anyhow!("{e}"))?;
            let s = self.olm.create_outbound_session(SessionConfig::version_1(), curve, key).map_err(|e| anyhow!("session: {e}"))?;
            self.sessions.entry(entry.curve.clone()).or_default().insert(0, s);
        }
        Ok(&mut self.sessions.get_mut(&entry.curve).unwrap()[0])
    }

    pub fn seal(&mut self, entry: &DeviceEntry, body: &Body, otk: Option<&str>) -> Result<WireReq> {
        let env = Envelope { account: self.p.account_pk.clone(), device: self.peer_id.to_string(), body: body.clone() };
        let plain = serde_json::to_vec(&env)?;
        let s = self.session_for(entry, otk)?;
        let msg = s.encrypt(plain).map_err(|e| anyhow!("encrypt: {e}"))?;
        Ok(WireReq { sender_curve: self.olm.curve25519_key().to_base64(), msg })
    }

    /// Decrypt with any session for this device, or start a new inbound one.
    fn open(&mut self, curve: &str, msg: &OlmMessage) -> Result<Envelope> {
        if let Some(list) = self.sessions.get_mut(curve) {
            for i in 0..list.len() {
                if let Ok(plain) = list[i].decrypt(msg) {
                    // Most recently working session first.
                    let s = list.remove(i);
                    list.insert(0, s);
                    return Ok(serde_json::from_slice(&plain)?);
                }
            }
        }
        if let OlmMessage::PreKey(pk) = msg {
            let key = Curve25519PublicKey::from_base64(curve).map_err(|e| anyhow!("{e}"))?;
            let r = self.olm.create_inbound_session(SessionConfig::version_1(), key, pk).map_err(|e| anyhow!("inbound session: {e}"))?;
            let list = self.sessions.entry(curve.to_string()).or_default();
            list.insert(0, r.session);
            list.truncate(MAX_SESSIONS);
            return Ok(serde_json::from_slice(&r.plaintext)?);
        }
        bail!("cannot decrypt message")
    }

    // ================================================================ sending

    /// Send a body to every device of `owner`. `msg_id` Some = reliable (outbox,
    /// mailbox fallback); None = only over live connections.
    pub fn send_body(&mut self, owner: &str, body: Body, msg_id: Option<u64>) -> Result<()> {
        let targets = self.targets(owner);
        for t in targets {
            self.send_to_target(&t, &body, msg_id, None)?;
        }
        if msg_id.is_some() {
            self.save();
        }
        Ok(())
    }

    pub fn send_to_target(&mut self, t: &Target, body: &Body, msg_id: Option<u64>, otk: Option<&str>) -> Result<()> {
        if t.entry.peer_id == self.peer_id.to_string() {
            return Ok(());
        }
        let connected = self.is_connected_peer(&t.entry.peer_id);
        if msg_id.is_none() && !connected {
            return Ok(());
        }
        self.register_addrs(&t.entry.peer_id, &t.addrs);
        let req = self.seal(&t.entry, body, otk)?;
        match msg_id {
            Some(id) => {
                self.p.outbox.push(OutboxItem {
                    owner: t.owner.clone(),
                    rec: OutRec { msg_id: id, device: t.entry.peer_id.clone(), req: req.clone(), created: now(), stored: false },
                });
                if connected || !t.addrs.is_empty() || self.swarm_knows(&t.entry.peer_id) {
                    self.send_wire(&t.owner, &t.entry.peer_id, req, id);
                } else {
                    self.store_in_network(&t.owner, &t.entry.peer_id, id);
                }
            }
            None => {
                if let Ok(pid) = t.entry.peer_id.parse::<PeerId>() {
                    self.swarm.behaviour_mut().rr.send_request(&pid, req);
                }
            }
        }
        Ok(())
    }

    fn swarm_knows(&self, _peer: &str) -> bool {
        // mDNS / rendezvous addresses were added to the swarm; let the dial try.
        true
    }

    pub fn send_wire(&mut self, owner: &str, device: &str, req: WireReq, msg_id: u64) {
        let Ok(pid) = device.parse::<PeerId>() else { return };
        let rid = self.swarm.behaviour_mut().rr.send_request(&pid, req);
        self.in_flight.insert(rid, (owner.to_string(), device.to_string(), msg_id));
        self.in_flight_set.insert((device.to_string(), msg_id));
    }

    pub fn flush_outbox_for_device(&mut self, device: &str) {
        let items: Vec<OutboxItem> = self
            .p
            .outbox
            .iter()
            .filter(|o| o.rec.device == device && !self.in_flight_set.contains(&(device.to_string(), o.rec.msg_id)))
            .cloned()
            .collect();
        for o in items {
            self.send_wire(&o.owner, device, o.rec.req, o.rec.msg_id);
        }
    }

    pub fn flush_all_outboxes(&mut self) {
        let devices: std::collections::HashSet<String> = self.p.outbox.iter().map(|o| o.rec.device.clone()).collect();
        for d in devices {
            if self.is_connected_peer(&d) {
                self.flush_outbox_for_device(&d);
            }
        }
        // Items nobody could take directly for a while go to the network stores.
        let stale: Vec<(String, String, u64)> = self
            .p
            .outbox
            .iter()
            .filter(|o| !o.rec.stored && now() - o.rec.created > 20 && !self.in_flight_set.contains(&(o.rec.device.clone(), o.rec.msg_id)))
            .map(|o| (o.owner.clone(), o.rec.device.clone(), o.rec.msg_id))
            .collect();
        for (owner, dev, id) in stale {
            self.store_in_network(&owner, &dev, id);
        }
    }

    pub fn on_rr(&mut self, ev: request_response::Event<WireReq, WireResp>) {
        use request_response::{Event as E, Message as M};
        match ev {
            E::Message { peer, message: M::Request { request, channel, .. }, .. } => {
                let ok = match self.on_wire(Some(peer), request, Via::Direct) {
                    Ok(()) => true,
                    Err(err) => {
                        tracing::debug!("rejected request from {peer}: {err:#}");
                        false
                    }
                };
                let _ = self.swarm.behaviour_mut().rr.send_response(channel, WireResp { ok });
            }
            E::Message { message: M::Response { request_id, response }, .. } => {
                if let Some((owner, device, msg_id)) = self.in_flight.remove(&request_id) {
                    self.in_flight_set.remove(&(device.clone(), msg_id));
                    self.delivered(&owner, &device, msg_id, response.ok);
                }
            }
            E::OutboundFailure { request_id, .. } => {
                if let Some((owner, device, msg_id)) = self.in_flight.remove(&request_id) {
                    self.in_flight_set.remove(&(device.clone(), msg_id));
                    self.store_in_network(&owner, &device, msg_id);
                }
            }
            _ => {}
        }
    }

    /// A device took a message (directly or via receipt).
    pub fn delivered(&mut self, owner: &str, device: &str, msg_id: u64, ok: bool) {
        let before = self.p.outbox.len();
        self.p.outbox.retain(|o| !(o.rec.device == device && o.rec.msg_id == msg_id));
        if before == self.p.outbox.len() && msg_id == 0 {
            return;
        }
        if msg_id != 0 {
            let still_pending = self.p.outbox.iter().any(|o| o.owner == owner && o.rec.msg_id == msg_id);
            if let Some(c) = self.contact_mut(owner) {
                if let Some(l) = c.history.iter_mut().find(|l| l.id == msg_id && l.from_me) {
                    if ok && l.delivery != Delivery::Read {
                        l.delivery = Delivery::Delivered;
                    } else if !ok && !still_pending && l.delivery != Delivery::Delivered {
                        l.delivery = Delivery::Failed;
                    }
                }
                self.emit_history(owner);
            }
            self.mark_group_delivered(owner, msg_id);
        }
        self.save();
    }

    fn set_delivery(&mut self, owner: &str, msg_id: u64, d: Delivery) {
        if let Some(c) = self.contact_mut(owner) {
            if let Some(l) = c.history.iter_mut().find(|l| l.id == msg_id && l.from_me) {
                if matches!(l.delivery, Delivery::Queued) {
                    l.delivery = d;
                }
            }
            let o = owner.to_string();
            self.emit_history(&o);
        }
    }

    pub fn on_mail_posted(&mut self, owner: &str, device: &str, msg_id: u64, ok: bool) {
        if let Some(o) = self.p.outbox.iter_mut().find(|o| o.rec.device == device && o.rec.msg_id == msg_id) {
            o.rec.stored = ok;
        }
        if ok && msg_id != 0 {
            self.set_delivery(owner, msg_id, Delivery::Stored);
        }
        self.save();
    }

    // ================================================================ receiving

    pub fn on_wire(&mut self, from: Option<PeerId>, req: WireReq, via: Via) -> Result<()> {
        let known = self.owner_of_curve(&req.sender_curve);
        if let Some((_, entry)) = &known {
            if let Some(p) = &from {
                if via == Via::Direct && entry.peer_id != p.to_string() {
                    bail!("identity key used from the wrong device");
                }
            }
        }
        let env = self.open(&req.sender_curve, &req.msg)?;
        // The envelope must name the device that owns this Olm key.
        let sender_device = env.device.clone();
        if let Some(p) = &from {
            if via == Via::Direct && sender_device != p.to_string() {
                bail!("envelope device does not match the connection");
            }
        }
        let receipt_id = receipt_id(&env.body);
        match known {
            Some((owner, entry)) => {
                if entry.peer_id != sender_device || !self.account_matches(&owner, &env.account) {
                    bail!("envelope does not match the sender's device list");
                }
                if let Some(id) = owner.strip_prefix("pending:") {
                    // More messages from someone we have not accepted yet: keep the
                    // ratchet in step, ignore the content.
                    let _ = id;
                } else if owner == "self" {
                    self.on_own_body(&sender_device, env.body)?;
                } else {
                    let is_contact = self.contact(&owner).is_some();
                    if is_contact {
                        self.on_contact_body(&owner, &sender_device, env.body, via)?;
                    } else {
                        self.on_group_peer_body(&owner, &sender_device, env.body)?;
                    }
                    if let Some(p) = self.p.contacts.iter_mut().find(|c| c.id == owner) {
                        if let Some(d) = p.devices.iter_mut().find(|d| d.entry.peer_id == sender_device) {
                            d.last_seen = now();
                        }
                    }
                }
                if via != Via::Direct {
                    if let (Some(id), false) = (receipt_id, owner.starts_with("pending:")) {
                        self.send_receipt(&owner, &sender_device, id);
                    }
                }
                self.save();
                Ok(())
            }
            None => self.on_stranger(from, &req.sender_curve, &sender_device, env, via),
        }
    }

    fn account_matches(&self, owner: &str, account_pk: &str) -> bool {
        if owner == "self" {
            return account_pk == self.p.account_pk;
        }
        if let Some(id) = owner.strip_prefix("pending:") {
            return account_id(account_pk).map(|a| a == id).unwrap_or(false);
        }
        account_id(account_pk).map(|a| a == owner).unwrap_or(false)
    }

    fn send_receipt(&mut self, owner: &str, device: &str, id: u64) {
        let targets: Vec<Target> = self.targets(owner).into_iter().filter(|t| t.entry.peer_id == device).collect();
        for t in targets {
            let _ = self.send_to_target(&t, &Body::Receipt { ids: vec![id] }, Some(0), None);
        }
    }

    /// First contact from an unknown device: authorization requests, group
    /// traffic from members, and (in link mode) the link grant.
    fn on_stranger(&mut self, from: Option<PeerId>, curve: &str, device: &str, env: Envelope, via: Via) -> Result<()> {
        match env.body {
            Body::LinkGrant(grant) if self.p.linking.is_some() => self.on_link_grant(*grant),
            // A new device of a known account announces itself with a list that
            // is signed by the account key and contains the sending device.
            Body::DeviceList(list) => {
                identity::verify_device_list(&list)?;
                if list.list.account_pk != env.account || !list.list.devices.iter().any(|d| d.curve == curve && d.peer_id == device) {
                    bail!("device list does not vouch for its sender");
                }
                if env.account == self.p.account_pk {
                    return self.on_own_device_list(list);
                }
                let id = account_id(&env.account)?;
                if self.contact(&id).is_none() {
                    bail!("device list from a stranger");
                }
                self.on_contact_device_list(&id, list)
            }
            Body::AuthRequest { token, card, seeds, text, pow, intro } => {
                identity::verify_card(&card)?;
                if card.devices.list.account_pk != env.account {
                    bail!("card does not match sender");
                }
                let entry = card
                    .devices
                    .list
                    .devices
                    .iter()
                    .find(|d| d.curve == curve && d.peer_id == device)
                    .ok_or_else(|| anyhow!("sender device not in its card"))?
                    .clone();
                if let (Some(p), Via::Direct) = (from, via) {
                    if entry.peer_id != p.to_string() {
                        bail!("card does not match connection");
                    }
                }
                let id = account_id(&env.account)?;
                if id == self.my_account_id() {
                    bail!("own account");
                }
                // A request needs a valid invite token (with proof of work) or an
                // introduction by one of our authorized contacts.
                let mut introduced_by = String::new();
                if let Some(intro) = &intro {
                    let introducer = self
                        .p
                        .contacts
                        .iter()
                        .find(|c| !c.removed && c.authorized && c.account_pk == intro.introducer)
                        .ok_or_else(|| anyhow!("introduction by a stranger"))?;
                    identity::verify_introduction(intro, &introducer.card_signed())?;
                    // The introduction presents *us* to the sender, and was given to the sender.
                    if intro.card.devices.list.account_pk != self.p.account_pk || intro.to != env.account {
                        bail!("introduction is for someone else");
                    }
                    introduced_by = format!("{}{}", introducer.petname, if intro.verified { " (verified)" } else { "" });
                } else {
                    let inv = self
                        .p
                        .invites
                        .iter_mut()
                        .find(|i| i.token == token && !i.revoked)
                        .ok_or_else(|| anyhow!("invalid invite token"))?;
                    if inv.expires.map(|e| e < now()).unwrap_or(false) || inv.uses_left == Some(0) {
                        bail!("invite used up or expired");
                    }
                    if !identity::pow_check(&token, &env.account, pow, 12) {
                        bail!("insufficient proof of work");
                    }
                    if let Some(u) = inv.uses_left.as_mut() {
                        *u -= 1;
                    }
                }
                if self.contact(&id).map(|c| c.authorized).unwrap_or(false) {
                    // Someone we already have re-adding us (e.g. after a reinstall).
                    let c = self.contact_mut(&id).unwrap();
                    c.apply_devices(&card.devices);
                    if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == device) {
                        d.seeds = Some(seeds);
                    }
                    self.save();
                    self.send_body(&id, Body::AuthAccept { card: self.my_card(), seeds: self.my_seeds() }, Some(0))?;
                    self.emit_contacts();
                    return Ok(());
                }
                let seeds = vec![(device.to_string(), seeds)];
                self.p.pending.retain(|p| p.id != id);
                self.p.pending.push(PendingRec { id, card: card.clone(), seeds, text, introduced_by, ts: now() });
                self.save();
                self.emit_pending();
                self.emit_invites();
                self.emit(Event::AuthRequested { name: card.nick });
                Ok(())
            }
            body @ (Body::GroupInvite(_) | Body::GroupKey { .. } | Body::GroupMsg { .. } | Body::GroupUpdate(_) | Body::GroupSyncReq { .. } | Body::GroupSync { .. }) => {
                let owner = account_id(&env.account)?;
                self.on_group_peer_body(&owner, device, body)
            }
            _ => bail!("unknown sender"),
        }
    }

    // ================================================================ contact bodies

    fn on_contact_body(&mut self, id: &str, device: &str, body: Body, via: Via) -> Result<()> {
        let (authorized, ignored) = {
            let c = self.contact(id).unwrap();
            (c.authorized, c.ignored)
        };
        if ignored && !matches!(body, Body::DeviceList(_) | Body::Receipt { .. }) {
            return Ok(());
        }
        match body {
            Body::AuthAccept { card, seeds } => {
                identity::verify_card(&card)?;
                let c = self.contact_mut(id).unwrap();
                if c.account_pk != card.devices.list.account_pk {
                    bail!("accept from another account");
                }
                c.apply_devices(&card.devices);
                if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == device) {
                    d.seeds = Some(seeds);
                }
                let was_awaiting = c.awaiting;
                c.authorized = true;
                c.awaiting = false;
                c.updated = now();
                let name = c.petname.clone();
                self.save();
                if was_awaiting {
                    self.notice(format!("{name} accepted your authorization request."));
                }
                self.sync_contact(id);
                self.send_presence_to(id);
                self.emit_contacts();
            }
            Body::AuthDeny => {
                let c = self.contact_mut(id).unwrap();
                if c.awaiting {
                    let name = c.petname.clone();
                    c.removed = true;
                    self.notice(format!("{name} declined your authorization request."));
                    self.emit_contacts();
                }
            }
            Body::Seeds(s) => {
                if let Some(d) = self.contact_mut(id).and_then(|c| c.devices.iter_mut().find(|d| d.entry.peer_id == device)) {
                    d.seeds = Some(s);
                }
            }
            Body::DeviceList(list) => self.on_contact_device_list(id, list)?,
            Body::Profile(p) => {
                if let Some(c) = self.contact_mut(id) {
                    c.profile = p;
                }
                self.emit_contacts();
            }
            _ if !authorized => {}
            Body::Text(m) => self.on_text(id, m, via),
            Body::Edit { id: mid, text } => {
                if let Some(l) = self.contact_mut(id).and_then(|c| c.history.iter_mut().find(|l| l.id == mid && !l.from_me)) {
                    l.text = text;
                    l.edited = true;
                }
                self.emit_history(id);
            }
            Body::Delete { id: mid } => {
                if let Some(l) = self.contact_mut(id).and_then(|c| c.history.iter_mut().find(|l| l.id == mid && !l.from_me)) {
                    l.text = String::new();
                    l.deleted = true;
                }
                self.emit_history(id);
            }
            Body::Typing(t) => {
                if t {
                    self.typing.insert(id.to_string(), Instant::now());
                } else {
                    self.typing.remove(id);
                }
                self.emit_contacts();
            }
            Body::Receipt { ids } => {
                for mid in ids {
                    self.delivered(id, device, mid, true);
                }
            }
            Body::Read { up_to } => {
                if let Some(c) = self.contact_mut(id) {
                    for l in c.history.iter_mut().filter(|l| l.from_me && l.ts <= up_to) {
                        if matches!(l.delivery, Delivery::Delivered | Delivery::Stored | Delivery::Queued) {
                            l.delivery = Delivery::Read;
                        }
                    }
                }
                self.emit_history(id);
            }
            Body::Presence(p) => self.on_presence(id, device, p),
            Body::Introduce(intro) => {
                let from = self.contact(id).map(|c| c.petname.clone()).unwrap_or_default();
                let introducer = self.contact(id).unwrap().card_signed();
                identity::verify_introduction(&intro, &introducer)?;
                self.p.intros.push(IntroRec { intro, from });
                self.emit_intros();
            }
            Body::FileOffer(o) => self.on_file_offer(id, o),
            Body::FileAccept { id: f } => self.on_file_accept(id, device, &f),
            Body::FileDecline { id: f } => self.on_file_declined(&f),
            Body::FileCancel { id: f } => self.on_file_declined(&f),
            b @ (Body::GroupInvite(_) | Body::GroupUpdate(_) | Body::GroupKey { .. } | Body::GroupMsg { .. } | Body::GroupLeave { .. } | Body::GroupSyncReq { .. } | Body::GroupSync { .. }) => {
                self.on_group_peer_body(id, device, b)?
            }
            Body::Hold { device: target, wire, expires } => {
                if self.p.net.helper || self.node {
                    if self.p.held.len() < 2000 {
                        self.p.held.push(HeldRec { device: target, wire, expires: expires.min(now() + crate::mailbox::TTL_SECS as i64) });
                    }
                }
            }
            Body::Fetch => self.deliver_held(device),
            _ => {}
        }
        Ok(())
    }

    fn on_contact_device_list(&mut self, id: &str, list: SignedDeviceList) -> Result<()> {
        identity::verify_device_list(&list)?;
        let c = self.contact_mut(id).ok_or_else(|| anyhow!("unknown contact"))?;
        if list.list.account_pk != c.account_pk {
            bail!("device list for another account");
        }
        if list.list.version <= c.list_version {
            return Ok(());
        }
        let (added, removed) = c.apply_devices(&list);
        let name = c.petname.clone();
        let removed_curves: Vec<String> = removed
            .iter()
            .filter_map(|p| c.devices.iter().find(|d| &d.entry.peer_id == p).map(|d| d.entry.curve.clone()))
            .collect();
        for curve in removed_curves {
            self.sessions.remove(&curve);
        }
        self.p.outbox.retain(|o| !(o.owner == id && removed.contains(&o.rec.device)));
        for a in &added {
            let dev_name = list.list.devices.iter().find(|d| &d.peer_id == a).map(|d| d.name.clone()).unwrap_or_default();
            self.notice(format!("{name} added a device ({dev_name}). It is signed by their account key."));
        }
        // Give new devices our seeds and presence.
        if !added.is_empty() {
            let seeds = self.my_seeds();
            let targets: Vec<Target> = self.targets(id).into_iter().filter(|t| added.contains(&t.entry.peer_id)).collect();
            for t in targets {
                let _ = self.send_to_target(&t, &Body::Seeds(seeds.clone()), Some(0), None);
            }
        }
        self.save();
        self.sync_contact(id);
        self.emit_contacts();
        Ok(())
    }

    fn on_text(&mut self, id: &str, m: TextMsg, via: Via) {
        let open = self.open_chats.contains(id);
        let my_status = self.p.status;
        let auto_reply = self.p.auto_reply && my_status.is_away() && !self.p.away_msg.is_empty();
        let away_msg = self.p.away_msg.clone();
        let Some(c) = self.contact_mut(id) else { return };
        if c.history.iter().any(|l| l.id == m.id && !l.from_me) {
            return;
        }
        // Urgent: allowed per contact, max 3 per hour.
        let hour_ago = now() - 3600;
        c.urgent_log.retain(|t| *t > hour_ago);
        let urgent = m.urgent && c.urgent_allowed && c.urgent_log.len() < 3;
        if urgent {
            c.urgent_log.push(now());
        }
        let mut line = LineRec::text(m.id, m.ts, false, m.text.clone(), Delivery::Received);
        line.reply_to = m.reply_to;
        line.urgent = urgent;
        line.expires = m.ttl.map(|t| now() + t as i64);
        c.history.push(line);
        if !open {
            c.unread += 1;
        }
        let name = c.petname.clone();
        let do_reply = auto_reply && now() - c.last_auto_reply > 3600;
        if do_reply {
            c.last_auto_reply = now();
        }
        self.emit(Event::Incoming { id: id.to_string(), name, text: m.text, urgent });
        if open && self.p.read_receipts {
            let _ = self.send_body(id, Body::Read { up_to: now() }, None);
        }
        if do_reply {
            let _ = self.send_text(id, format!("[auto-reply] {away_msg}"), None, false);
        }
        let _ = via;
        self.emit_history(id);
        self.emit_contacts();
    }

    pub fn on_presence(&mut self, owner: &str, device: &str, p: Presence) {
        let first = !self.presence.contains_key(device);
        let addrs = p.addrs.clone();
        if owner != "self" {
            let online = p.status != Status::Offline;
            let was_online = self.contact_status(owner) != Status::Offline;
            if let Some(c) = self.contact_mut(owner) {
                if !c.authorized {
                    return;
                }
                c.last_os = p.os.clone();
                c.last_device = p.device_class;
                if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == device) {
                    for a in &addrs {
                        if !d.addrs.contains(a) {
                            d.addrs.insert(0, a.clone());
                        }
                    }
                    d.addrs.truncate(16);
                    d.last_seen = now();
                }
            }
            if online {
                self.presence.insert(device.to_string(), PresenceRec { owner: owner.to_string(), pres: p, at: Instant::now(), ttl: PRESENCE_TTL });
            } else {
                self.presence.remove(device);
            }
            if online && !was_online && self.contact(owner).map(|c| c.notify_online).unwrap_or(false) {
                let name = self.contact(owner).map(|c| c.petname.clone()).unwrap_or_default();
                self.emit(Event::ContactOnline { id: owner.to_string(), name });
            }
        } else {
            self.p.own_last_seen.insert(device.to_string(), now());
            let e = self.p.own_addrs.entry(device.to_string()).or_default();
            for a in &addrs {
                if !e.contains(a) {
                    e.insert(0, a.clone());
                }
            }
            e.truncate(16);
            if p.status != Status::Offline {
                self.presence.insert(device.to_string(), PresenceRec { owner: "self".into(), pres: p, at: Instant::now(), ttl: PRESENCE_TTL });
            } else {
                self.presence.remove(device);
            }
            self.emit_devices();
        }
        self.register_addrs(device, &addrs);
        if first {
            self.send_presence_to_peer(device);
        }
        self.emit_contacts();
    }

    // ================================================================ presence

    pub fn my_presence(&self) -> Presence {
        Presence {
            status: self.p.status,
            away_msg: if self.p.status.is_away() { self.p.away_msg.clone() } else { String::new() },
            os: self.os.clone(),
            device_class: self.device_class,
            on_battery: crate::device::on_battery(),
            background: self.background,
            helper: self.p.net.helper || self.node,
            bot: self.bot,
            addrs: self.my_addrs(),
            now_playing: self.now_playing.clone(),
        }
    }

    /// What a given contact is allowed to see.
    pub fn presence_for(&self, owner: &str) -> Option<Presence> {
        let mut p = self.my_presence();
        if owner == "self" {
            return Some(p);
        }
        let c = self.contact(owner)?;
        if !c.authorized || c.ignored {
            return None;
        }
        match c.visibility {
            Visibility::Invisible => {
                p.status = Status::Offline;
                Some(p)
            }
            Visibility::Visible if self.p.status == Status::Invisible => {
                p.status = Status::Online;
                Some(p)
            }
            _ if self.p.status == Status::Invisible => None,
            _ => Some(p),
        }
    }

    pub fn send_presence_to_peer(&mut self, device: &str) {
        let Some(owner) = self.owner_of_peer(device) else { return };
        if !self.is_connected_peer(device) {
            return;
        }
        let Some(p) = self.presence_for(&owner) else { return };
        let targets: Vec<Target> = self.targets(&owner).into_iter().filter(|t| t.entry.peer_id == device).collect();
        for t in targets {
            let _ = self.send_to_target(&t, &Body::Presence(p.clone()), None, None);
        }
    }

    pub fn send_presence_to(&mut self, owner: &str) {
        let Some(p) = self.presence_for(owner) else { return };
        let _ = self.send_body(owner, Body::Presence(p), None);
    }

    pub fn broadcast_presence(&mut self) {
        let owners: Vec<String> = self.p.contacts.iter().filter(|c| !c.removed).map(|c| c.id.clone()).collect();
        for o in owners {
            self.send_presence_to(&o);
        }
        self.send_presence_to("self");
    }

    pub fn set_status(&mut self, s: Status) {
        self.p.status = s;
        self.settings_changed();
        self.emit(Event::Status(s));
        self.broadcast_presence();
    }

    pub async fn go_offline(&mut self) {
        let prev = self.p.status;
        self.p.status = Status::Offline;
        self.broadcast_presence();
        // Those who only see us via Nostr learn it too, instead of waiting for a timeout.
        self.signal_presence_all();
        self.p.status = prev;
        let deadline = tokio::time::sleep(std::time::Duration::from_millis(600));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                ev = self.swarm.select_next_some() => {
                    if let libp2p::swarm::SwarmEvent::Behaviour(net::BehaviourEvent::Rr(e)) = ev { self.on_rr(e); }
                }
            }
        }
    }

    pub fn contact_status(&self, id: &str) -> Status {
        self.presence
            .values()
            .filter(|r| r.owner == id && r.at.elapsed() < r.ttl)
            .max_by_key(|r| r.at)
            .map(|r| r.pres.status)
            .unwrap_or(Status::Offline)
    }

    pub fn set_profile(&mut self, pr: Profile) {
        let mut pr = pr;
        pr.updated = now();
        self.p.profile = pr.clone();
        self.settings_changed();
        let owners: Vec<String> = self.p.contacts.iter().filter(|c| c.authorized && !c.removed).map(|c| c.id.clone()).collect();
        for o in owners {
            let _ = self.send_body(&o, Body::Profile(pr.clone()), Some(0));
        }
    }

    // ================================================================ messaging commands

    pub fn send_text(&mut self, id: &str, text: String, reply_to: Option<u64>, urgent: bool) -> Result<()> {
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return Ok(());
        }
        let c = self.contact(id).ok_or_else(|| anyhow!("unknown contact"))?;
        if !c.authorized {
            bail!("{} has not authorized you yet", c.petname);
        }
        let ttl = c.disappearing;
        let msg_id = rand::random::<u64>() | 1;
        let ts = now();
        let m = TextMsg { id: msg_id, ts, text: text.clone(), reply_to, urgent, ttl };
        let c = self.contact_mut(id).unwrap();
        let mut line = LineRec::text(msg_id, ts, true, text, Delivery::Queued);
        line.reply_to = reply_to;
        line.urgent = urgent;
        line.expires = ttl.map(|t| ts + t as i64);
        c.history.push(line);
        self.send_body(id, Body::Text(m.clone()), Some(msg_id))?;
        let _ = self.send_body("self", Body::SelfCopy { contact: id.to_string(), msg: m }, Some(0));
        self.emit_history(id);
        Ok(())
    }

    pub fn edit_text(&mut self, id: &str, msg: u64, text: String) -> Result<()> {
        let c = self.contact_mut(id).ok_or_else(|| anyhow!("unknown contact"))?;
        let l = c.history.iter_mut().find(|l| l.id == msg && l.from_me).ok_or_else(|| anyhow!("no such message"))?;
        l.text = text.clone();
        l.edited = true;
        self.send_body(id, Body::Edit { id: msg, text }, Some(0))?;
        self.emit_history(id);
        Ok(())
    }

    pub fn delete_text(&mut self, id: &str, msg: u64) -> Result<()> {
        let c = self.contact_mut(id).ok_or_else(|| anyhow!("unknown contact"))?;
        let l = c.history.iter_mut().find(|l| l.id == msg).ok_or_else(|| anyhow!("no such message"))?;
        l.text.clear();
        l.deleted = true;
        let from_me = l.from_me;
        if from_me {
            self.send_body(id, Body::Delete { id: msg }, Some(0))?;
        }
        self.save();
        self.emit_history(id);
        Ok(())
    }

    pub fn send_typing(&mut self, id: &str, typing: bool) {
        if !self.p.send_typing {
            return;
        }
        // Not to backgrounded mobile devices (PRD PR-9).
        let skip: Vec<String> = self
            .presence
            .iter()
            .filter(|(_, r)| r.owner == id && r.pres.background && r.pres.device_class == DeviceClass::Mobile)
            .map(|(k, _)| k.clone())
            .collect();
        let targets: Vec<Target> = self.targets(id).into_iter().filter(|t| !skip.contains(&t.entry.peer_id)).collect();
        for t in targets {
            let _ = self.send_to_target(&t, &Body::Typing(typing), None, None);
        }
    }

    pub fn expire_typing(&mut self) {
        let before = self.typing.len();
        self.typing.retain(|_, t| t.elapsed().as_secs() < 8);
        if before != self.typing.len() {
            self.emit_contacts();
        }
    }

    pub fn expire_disappearing(&mut self, now: i64) {
        let mut changed = vec![];
        for c in &mut self.p.contacts {
            let before = c.history.len();
            c.history.retain(|l| l.expires.map(|e| e > now).unwrap_or(true));
            if before != c.history.len() {
                changed.push(c.id.clone());
            }
        }
        if !changed.is_empty() {
            self.save();
            for id in changed {
                self.emit_history(&id);
            }
        }
    }

    pub fn open_chat(&mut self, id: &str) {
        self.open_chats.insert(id.to_string());
        let had_unread = self.contact(id).map(|c| c.unread > 0).unwrap_or(false);
        if let Some(c) = self.contact_mut(id) {
            c.unread = 0;
        }
        if had_unread {
            if self.p.read_receipts {
                let _ = self.send_body(id, Body::Read { up_to: now() }, None);
            }
            let _ = self.send_body("self", Body::SyncRead { contact: id.to_string(), up_to: now() }, None);
            self.save();
        }
        self.emit_history(id);
        self.emit_contacts();
    }

    pub fn send_note(&mut self, text: String) {
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        let id = rand::random::<u64>() | 1;
        let m = TextMsg { id, ts: now(), text: text.clone(), reply_to: None, urgent: false, ttl: None };
        self.p.notes.push(LineRec::text(id, m.ts, true, text, Delivery::Delivered));
        let _ = self.send_body("self", Body::SelfNote(m), Some(0));
        self.save();
        self.emit_notes();
    }

    pub fn search(&self, q: &str) {
        let q = q.to_lowercase();
        let mut out = vec![];
        if q.trim().is_empty() {
            self.emit(Event::SearchResults(out));
            return;
        }
        for c in &self.p.contacts {
            if c.removed {
                continue;
            }
            for l in &c.history {
                if !l.deleted && l.text.to_lowercase().contains(&q) {
                    out.push((c.id.clone(), c.petname.clone(), self.line_view(c, l)));
                }
            }
        }
        for l in &self.p.notes {
            if l.text.to_lowercase().contains(&q) {
                out.push(("self".into(), "note to self".into(), self.note_view(l)));
            }
        }
        out.sort_by_key(|(_, _, l)| -l.ts);
        out.truncate(200);
        self.emit(Event::SearchResults(out));
    }

    // ================================================================ invites & authorization

    pub fn new_invite(&mut self, uses: Option<u32>, ttl_secs: Option<u64>, label: String) -> Result<()> {
        let single = uses == Some(1);
        let otk = if single {
            let k = self.olm.generate_one_time_keys(1).created.into_iter().next().ok_or_else(|| anyhow!("no one-time key"))?;
            self.olm.mark_keys_as_published();
            k.to_base64()
        } else {
            String::new()
        };
        let token = hex::encode(rand::random::<[u8; 16]>());
        let expires = ttl_secs.map(|t| now() + t as i64);
        let inv = Invite {
            v: 2,
            card: self.my_card(),
            device: self.peer_id.to_string(),
            otk,
            token: token.clone(),
            seeds: self.my_seeds(),
            addrs: self.my_addrs(),
            pow_bits: 12,
            expires,
        };
        let code = identity::encode_invite(&inv)?;
        self.p.invites.push(InviteRec { token, label, created: now(), uses_left: uses, expires, revoked: false, code: code.clone() });
        self.save();
        self.emit(Event::Invite(code));
        self.emit_invites();
        Ok(())
    }

    pub fn add_contact(&mut self, invite: &str, text: &str) -> Result<()> {
        let inv = identity::decode_invite(invite)?;
        let id = account_id(&inv.card.devices.list.account_pk)?;
        if id == self.my_account_id() {
            bail!("that is your own invite");
        }
        if self.contact(&id).map(|c| c.authorized).unwrap_or(false) {
            bail!("{} is already in your list", inv.card.nick);
        }
        let entry = inv.card.devices.list.devices.iter().find(|d| d.peer_id == inv.device).cloned().context("device")?;
        self.p.contacts.retain(|c| c.id != id);
        let mut c = ContactRec::new(id.clone(), &inv.card, vec![(inv.device.clone(), inv.seeds.clone())]);
        c.awaiting = true;
        if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == inv.device) {
            d.addrs = inv.addrs.clone();
        }
        self.p.contacts.push(c);
        let pow = identity::pow_solve(&inv.token, &self.p.account_pk, inv.pow_bits);
        let body = Body::AuthRequest { token: inv.token.clone(), card: self.my_card(), seeds: self.my_seeds(), text: text.to_string(), pow, intro: None };
        let t = Target { owner: id.clone(), entry, seeds: Some(inv.seeds.clone()), addrs: inv.addrs.clone() };
        // The first session uses the invite's one-time key (single-use invites).
        self.sessions.remove(&t.entry.curve);
        self.send_to_target(&t, &body, Some(0), Some(&inv.otk))?;
        self.save();
        self.sync_contact(&id);
        self.emit_contacts();
        self.notice(format!("Authorization request sent to {}.", inv.card.nick));
        Ok(())
    }

    pub fn accept(&mut self, id: &str) -> Result<()> {
        let idx = self.p.pending.iter().position(|p| p.id == id).ok_or_else(|| anyhow!("no such request"))?;
        let pr = self.p.pending.remove(idx);
        self.p.contacts.retain(|c| c.id != id);
        let mut c = ContactRec::new(pr.id.clone(), &pr.card, pr.seeds.clone());
        c.authorized = true;
        c.introduced_by = pr.introduced_by.clone();
        self.p.contacts.push(c);
        self.send_body(id, Body::AuthAccept { card: self.my_card(), seeds: self.my_seeds() }, Some(0))?;
        self.save();
        self.sync_contact(id);
        self.send_presence_to(id);
        self.emit_pending();
        self.emit_contacts();
        Ok(())
    }

    pub fn deny(&mut self, id: &str) {
        let Some(pr) = self.p.pending.iter().find(|p| p.id == id).cloned() else { return };
        let targets: Vec<Target> = pr
            .card
            .devices
            .list
            .devices
            .iter()
            .map(|d| Target { owner: format!("pending:{id}"), entry: d.clone(), seeds: None, addrs: vec![] })
            .collect();
        for t in targets {
            let _ = self.send_to_target(&t, &Body::AuthDeny, None, None);
        }
        self.p.pending.retain(|p| p.id != id);
        for d in &pr.card.devices.list.devices {
            self.sessions.remove(&d.curve);
        }
        self.save();
        self.emit_pending();
    }

    pub fn remove_contact(&mut self, id: &str) {
        if let Some(c) = self.contact_mut(id) {
            c.removed = true;
            c.updated = now();
            let curves: Vec<String> = c.devices.iter().map(|d| d.entry.curve.clone()).collect();
            for cv in curves {
                self.sessions.remove(&cv);
            }
        }
        self.p.outbox.retain(|o| o.owner != id);
        self.save();
        self.sync_contact(id);
        self.emit_contacts();
    }

    pub fn introduce(&mut self, to: &str, whom: &str) -> Result<()> {
        let a = self.contact(to).ok_or_else(|| anyhow!("unknown contact"))?;
        let b = self.contact(whom).ok_or_else(|| anyhow!("unknown contact"))?;
        if !a.authorized || !b.authorized {
            bail!("both must be authorized contacts");
        }
        let intro = Introduction {
            card: b.card_signed_card(),
            seeds: b.seeds(),
            introducer: String::new(),
            introducer_device: String::new(),
            introducer_nick: self.p.nick.clone(),
            to: a.account_pk.clone(),
            verified: b.verified,
            ts: now(),
            sig: String::new(),
            addrs: self.known_addrs(b),
        };
        let bname = b.petname.clone();
        let aname = a.petname.clone();
        let intro = identity::sign_introduction(&self.device_key, &self.p.account_pk, intro)?;
        self.send_body(to, Body::Introduce(intro), Some(0))?;
        self.notice(format!("Introduced {bname} to {aname}."));
        Ok(())
    }

    pub fn accept_introduction(&mut self, index: usize, text: &str, trust: bool) -> Result<()> {
        if index >= self.p.intros.len() {
            bail!("no such introduction");
        }
        let rec = self.p.intros.remove(index);
        let intro = rec.intro;
        let id = account_id(&intro.card.devices.list.account_pk)?;
        if self.contact(&id).map(|c| c.authorized).unwrap_or(false) {
            self.save();
            self.emit_intros();
            bail!("{} is already in your list", intro.card.nick);
        }
        self.p.contacts.retain(|c| c.id != id);
        let mut c = ContactRec::new(id.clone(), &intro.card, intro.seeds.clone());
        c.awaiting = true;
        c.introduced_by = format!("{}{}", rec.from, if intro.verified { " (verified)" } else { "" });
        // A vouch counts as verification only if the user says so (CT-9).
        c.verified = trust && intro.verified;
        c.set_addrs(&intro.addrs);
        for (peer, a) in &intro.addrs {
            self.register_addrs(peer, a);
        }
        self.p.contacts.push(c);
        let body = Body::AuthRequest { token: String::new(), card: self.my_card(), seeds: self.my_seeds(), text: text.to_string(), pow: 0, intro: Some(intro) };
        self.send_body(&id, body, Some(0))?;
        self.save();
        self.sync_contact(&id);
        self.emit_intros();
        self.emit_contacts();
        Ok(())
    }

    // ================================================================ own devices

    fn on_own_body(&mut self, device: &str, body: Body) -> Result<()> {
        match body {
            Body::Presence(p) => self.on_presence("self", device, p),
            Body::Seeds(s) => {
                self.p.own_seeds.insert(device.to_string(), s);
            }
            Body::DeviceList(list) => self.on_own_device_list(list)?,
            Body::SelfCopy { contact, msg } => {
                if let Some(c) = self.contact_mut(&contact) {
                    if !c.history.iter().any(|l| l.id == msg.id) {
                        let mut l = LineRec::text(msg.id, msg.ts, true, msg.text, Delivery::Delivered);
                        l.reply_to = msg.reply_to;
                        l.expires = msg.ttl.map(|t| msg.ts + t as i64);
                        c.history.push(l);
                    }
                    self.emit_history(&contact);
                }
            }
            Body::SelfNote(m) => {
                if !self.p.notes.iter().any(|l| l.id == m.id) {
                    self.p.notes.push(LineRec::text(m.id, m.ts, true, m.text, Delivery::Delivered));
                }
                self.emit_notes();
            }
            Body::SyncContacts(list) => {
                for cs in list {
                    self.merge_contact_sync(cs);
                }
                self.emit_contacts();
            }
            Body::SyncSettings(s) => self.merge_settings(s),
            Body::SyncRead { contact, up_to } => {
                if let Some(c) = self.contact_mut(&contact) {
                    if c.history.iter().all(|l| l.ts <= up_to) {
                        c.unread = 0;
                    }
                }
                self.emit_contacts();
            }
            Body::History { contact, lines } => {
                if let Some(c) = self.contact_mut(&contact) {
                    for l in lines {
                        if !c.history.iter().any(|h| h.id == l.id) {
                            c.history.push(LineRec::text(l.id, l.ts, l.from_me, l.text, if l.from_me { Delivery::Delivered } else { Delivery::Received }));
                        }
                    }
                    c.history.sort_by_key(|l| l.ts);
                }
            }
            Body::Remote(cmd) => self.on_remote(cmd)?,
            Body::RemoteAck { action, .. } => {
                let (status, what) = match action {
                    RemoteAction::Lock => ("locked", "locked"),
                    RemoteAction::Wipe => ("wiped", "wiped its data"),
                };
                self.p.remote_status.insert(device.to_string(), status.into());
                let name = self.p.devices.as_ref().and_then(|l| l.list.devices.iter().find(|d| d.peer_id == device).map(|d| d.name.clone())).unwrap_or_else(|| "A removed device".into());
                self.save();
                self.emit_devices();
                self.notice(format!("{name} {what}."));
            }
            Body::PluginState { plugin, state, .. } => self.emit(Event::PluginState { plugin, state }),
            Body::Receipt { ids } => {
                for id in ids {
                    self.delivered("self", device, id, true);
                }
            }
            Body::FileOffer(o) => self.on_file_offer("self", o),
            Body::FileAccept { id } => self.on_file_accept("self", device, &id),
            Body::FileDecline { id } | Body::FileCancel { id } => self.on_file_declined(&id),
            b @ (Body::GroupKey { .. } | Body::GroupMsg { .. } | Body::GroupInvite(_) | Body::GroupUpdate(_) | Body::GroupSync { .. }) => {
                let me = self.my_account_id();
                self.on_group_peer_body(&me, device, b)?
            }
            Body::Hold { device: target, wire, expires } => {
                if self.p.held.len() < 2000 {
                    self.p.held.push(HeldRec { device: target, wire, expires });
                }
            }
            Body::Fetch => self.deliver_held(device),
            _ => {}
        }
        Ok(())
    }

    /// Contact state changed here: tell our other devices.
    pub fn sync_contact(&mut self, id: &str) {
        let Some(c) = self.p.contacts.iter().find(|c| c.id == id) else { return };
        let cs = ContactSync {
            id: c.id.clone(),
            petname: c.petname.clone(),
            folder: c.folder.clone(),
            verified: c.verified,
            visibility: c.visibility,
            ignored: c.ignored,
            removed: c.removed,
            card: c.card_signed_card(),
            seeds: c.seeds(),
            authorized: c.authorized,
            updated: c.updated,
            addrs: self.known_addrs(c),
            notify_online: Some(c.notify_online),
            auto_accept: Some(c.auto_accept),
            urgent_allowed: Some(c.urgent_allowed),
        };
        let _ = self.send_body("self", Body::SyncContacts(vec![cs]), Some(0));
    }

    pub fn merge_contact_sync(&mut self, cs: ContactSync) {
        let newly_authorized;
        match self.p.contacts.iter_mut().find(|c| c.id == cs.id) {
            Some(c) => {
                if cs.updated < c.updated {
                    return;
                }
                newly_authorized = cs.authorized && !c.authorized;
                c.petname = cs.petname;
                c.folder = cs.folder;
                c.verified = cs.verified;
                c.visibility = cs.visibility;
                c.ignored = cs.ignored;
                c.removed = cs.removed;
                c.authorized = cs.authorized;
                c.awaiting = !cs.authorized && !cs.removed;
                c.updated = cs.updated;
                if let Some(v) = cs.notify_online {
                    c.notify_online = v;
                }
                if let Some(v) = cs.auto_accept {
                    c.auto_accept = v;
                }
                if let Some(v) = cs.urgent_allowed {
                    c.urgent_allowed = v;
                }
                if identity::verify_card(&cs.card).is_ok() && cs.card.devices.list.version > c.list_version {
                    c.apply_devices(&cs.card.devices);
                }
                for (peer, s) in cs.seeds {
                    if let Some(d) = c.devices.iter_mut().find(|d| d.entry.peer_id == peer) {
                        d.seeds = Some(s);
                    }
                }
                c.set_addrs(&cs.addrs);
            }
            None => {
                if identity::verify_card(&cs.card).is_err() {
                    return;
                }
                let mut c = ContactRec::new(cs.id.clone(), &cs.card, cs.seeds);
                c.petname = cs.petname;
                c.folder = cs.folder;
                c.verified = cs.verified;
                c.visibility = cs.visibility;
                c.ignored = cs.ignored;
                c.removed = cs.removed;
                c.authorized = cs.authorized;
                c.awaiting = !cs.authorized && !cs.removed;
                c.updated = cs.updated;
                if let Some(v) = cs.notify_online {
                    c.notify_online = v;
                }
                if let Some(v) = cs.auto_accept {
                    c.auto_accept = v;
                }
                if let Some(v) = cs.urgent_allowed {
                    c.urgent_allowed = v;
                }
                c.set_addrs(&cs.addrs);
                newly_authorized = cs.authorized;
                self.p.contacts.push(c);
            }
        }
        for (peer, a) in &cs.addrs {
            self.register_addrs(peer, a);
        }
        if newly_authorized {
            // This device is new to the contact: introduce our seeds.
            let seeds = self.my_seeds();
            let _ = self.send_body(&cs.id, Body::Seeds(seeds), Some(0));
            self.send_presence_to(&cs.id);
        }
        self.save();
    }

    pub fn sync_settings(&mut self) {
        let s = SettingsSync {
            status: Some(self.p.status),
            away_msg: Some(self.p.away_msg.clone()),
            auto_reply: Some(self.p.auto_reply),
            profile: Some(self.p.profile.clone()),
            folders: Some(self.p.folders.clone()),
            updated: self.p.settings_updated,
            nick: Some(self.p.nick.clone()),
            send_typing: Some(self.p.send_typing),
        };
        let _ = self.send_body("self", Body::SyncSettings(s), Some(0));
    }

    fn merge_settings(&mut self, s: SettingsSync) {
        if s.updated < self.p.settings_updated {
            return;
        }
        if let Some(v) = s.away_msg {
            self.p.away_msg = v;
        }
        if let Some(v) = s.auto_reply {
            self.p.auto_reply = v;
        }
        if let Some(v) = s.profile {
            self.p.profile = v;
        }
        if let Some(v) = s.folders {
            self.p.folders = v;
            self.emit(Event::Folders(self.p.folders.clone()));
        }
        if let Some(v) = s.nick.filter(|n| !n.trim().is_empty()) {
            self.p.nick = v;
        }
        if let Some(v) = s.send_typing {
            self.p.send_typing = v;
        }
        self.p.settings_updated = s.updated;
        self.save();
        self.emit_settings();
    }
}

fn receipt_id(b: &Body) -> Option<u64> {
    match b {
        Body::Text(m) => Some(m.id),
        Body::SelfCopy { .. } | Body::SelfNote(_) => None,
        _ => None,
    }
}

impl ContactRec {
    /// Card with a real signature is not kept; for re-sharing we keep the last
    /// verified list signature alongside (see `signed`).
    pub fn card_signed(&self) -> SignedDeviceList {
        self.signed.clone().unwrap_or_else(|| self.card().devices)
    }

    pub fn card_signed_card(&self) -> Card {
        Card { nick: self.nick.clone(), devices: self.card_signed() }
    }
}
