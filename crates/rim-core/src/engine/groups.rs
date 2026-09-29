//! Private groups (PRD GR-1..3): Megolm-encrypted, membership signed by an
//! admin device, fanned out over the 1:1 Olm channels (and their mailboxes),
//! key rotation on removal, catch-up from any member.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use vodozemac::megolm::{ExportedSessionKey, GroupSession, InboundGroupSession, MegolmMessage, SessionConfig, SessionKey};

use super::msg::Target;
use super::*;
use crate::identity::{self, now};

const MAX_MEMBERS: usize = 100;

#[derive(Serialize, Deserialize)]
struct GroupPlain {
    id: u64,
    ts: i64,
    text: String,
}

impl Engine {
    fn group(&self, gid: &str) -> Option<&GroupRec> {
        self.p.groups.iter().find(|g| g.state.id == gid)
    }

    fn group_mut(&mut self, gid: &str) -> Option<&mut GroupRec> {
        self.p.groups.iter_mut().find(|g| g.state.id == gid)
    }

    fn me_member(&self) -> GroupMember {
        GroupMember {
            account: self.my_account_id(),
            nick: self.p.nick.clone(),
            devices: self.p.devices.clone().expect("device list"),
            seeds: self.p.own_seeds.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            addrs: vec![(self.peer_id.to_string(), self.my_addrs())],
            admin: true,
        }
    }

    fn member_from_contact(&self, id: &str) -> Result<GroupMember> {
        let c = self.contact(id).ok_or_else(|| anyhow!("unknown contact"))?;
        if !c.authorized {
            bail!("{} has not authorized you", c.petname);
        }
        let devices = c.signed.clone().ok_or_else(|| anyhow!("no signed device list for {} yet", c.petname))?;
        let addrs = c.devices.iter().map(|d| (d.entry.peer_id.clone(), d.addrs.clone())).collect();
        Ok(GroupMember { account: c.id.clone(), nick: c.petname.clone(), devices, seeds: c.seeds(), addrs, admin: false })
    }

    fn is_admin(&self, g: &GroupState) -> bool {
        let me = self.my_account_id();
        g.members.iter().any(|m| m.account == me && m.admin)
    }

    /// Devices of a member: the contact record (fresher) if they are a contact.
    fn member_targets(&self, gid: &str, account: &str) -> Vec<Target> {
        if account == self.my_account_id() {
            return self.targets("self");
        }
        if self.contact(account).map(|c| c.authorized).unwrap_or(false) {
            return self.targets(account);
        }
        let Some(g) = self.group(gid) else { return vec![] };
        let Some(m) = g.state.members.iter().find(|m| m.account == account) else { return vec![] };
        m.devices
            .list
            .devices
            .iter()
            .map(|d| Target {
                owner: account.to_string(),
                entry: d.clone(),
                seeds: m.seeds.iter().find(|(p, _)| p == &d.peer_id).map(|(_, s)| s.clone()),
                addrs: m.addrs.iter().find(|(p, _)| p == &d.peer_id).map(|(_, a)| a.clone()).unwrap_or_default(),
            })
            .collect()
    }

    fn send_to_members(&mut self, gid: &str, body: Body, reliable: bool, include_self_devices: bool) {
        let Some(g) = self.group(gid) else { return };
        let me = self.my_account_id();
        let accounts: Vec<String> = g.state.members.iter().map(|m| m.account.clone()).filter(|a| *a != me || include_self_devices).collect();
        for a in accounts {
            let targets = self.member_targets(gid, &a);
            for t in targets {
                let id = if reliable { Some(0) } else { None };
                let _ = self.send_to_target(&t, &body, id, None);
            }
        }
        self.save();
    }

    fn publish_state(&mut self, mut st: GroupState) -> Result<()> {
        st.version += 1;
        let st = identity::sign_group(&self.device_key, &self.p.account_pk, st)?;
        let gid = st.id.clone();
        let old_members: Vec<String> = self.group(&gid).map(|g| g.state.members.iter().map(|m| m.account.clone()).collect()).unwrap_or_default();
        if let Some(g) = self.group_mut(&gid) {
            g.state = st.clone();
        }
        // Everyone in the old or new list learns the new state.
        let mut all: Vec<String> = st.members.iter().map(|m| m.account.clone()).collect();
        for o in old_members {
            if !all.contains(&o) {
                all.push(o);
            }
        }
        let me = self.my_account_id();
        for a in all {
            let targets = if a == me {
                self.targets("self")
            } else if self.contact(&a).is_some() {
                self.targets(&a)
            } else {
                self.member_targets(&gid, &a)
            };
            for t in targets {
                let _ = self.send_to_target(&t, &Body::GroupUpdate(st.clone()), Some(0), None);
            }
        }
        self.save();
        self.emit_groups();
        Ok(())
    }

    pub fn create_group(&mut self, name: &str, members: &[String]) -> Result<()> {
        if name.trim().is_empty() {
            bail!("the group needs a name");
        }
        if members.len() + 1 > MAX_MEMBERS {
            bail!("groups are limited to {MAX_MEMBERS} members");
        }
        let mut list = vec![self.me_member()];
        for id in members {
            list.push(self.member_from_contact(id)?);
        }
        let st = GroupState {
            id: hex::encode(rand::random::<[u8; 12]>()),
            name: name.trim().to_string(),
            version: 1,
            members: list,
            by: String::new(),
            by_device: String::new(),
            sig: String::new(),
        };
        let st = identity::sign_group(&self.device_key, &self.p.account_pk, st)?;
        let gid = st.id.clone();
        self.p.groups.push(GroupRec { state: st.clone(), outbound: None, inbound: HashMap::new(), history: vec![], unread: 0, left: false });
        self.group_out.insert(gid.clone(), GroupSession::new(SessionConfig::version_1()));
        self.group_in.insert(gid.clone(), HashMap::new());
        self.send_to_members(&gid, Body::GroupInvite(st), true, true);
        self.distribute_key(&gid, None);
        self.save();
        self.emit_groups();
        Ok(())
    }

    /// Send our current outbound session key to members (or only `only`).
    fn distribute_key(&mut self, gid: &str, only: Option<&[String]>) {
        let Some(out) = self.group_out.get(gid) else { return };
        let key = format!("s:{}", out.session_key().to_base64());
        let Some(g) = self.group(gid) else { return };
        let me = self.my_account_id();
        let accounts: Vec<String> = g
            .state
            .members
            .iter()
            .map(|m| m.account.clone())
            .filter(|a| only.map(|o| o.contains(a)).unwrap_or(true))
            .collect();
        for a in accounts {
            let targets = if a == me { self.targets("self") } else { self.member_targets(gid, &a) };
            for t in targets {
                let _ = self.send_to_target(&t, &Body::GroupKey { group: gid.to_string(), key: key.clone() }, Some(0), None);
            }
        }
    }

    pub fn rotate_group_key(&mut self, gid: &str) {
        if self.group(gid).map(|g| g.left).unwrap_or(true) {
            return;
        }
        self.group_out.insert(gid.to_string(), GroupSession::new(SessionConfig::version_1()));
        self.distribute_key(gid, None);
        self.save();
    }

    pub fn share_group_keys_with(&mut self, t: &Target) {
        let gids: Vec<String> = self.p.groups.iter().filter(|g| !g.left).map(|g| g.state.id.clone()).collect();
        for gid in gids {
            let mut keys = vec![];
            if let Some(ins) = self.group_in.get(&gid) {
                for s in ins.values() {
                    keys.push(format!("x:{}", s.export_at_first_known_index().to_base64()));
                }
            }
            if let Some(o) = self.group_out.get(&gid) {
                keys.push(format!("s:{}", o.session_key().to_base64()));
            }
            for key in keys {
                let _ = self.send_to_target(t, &Body::GroupKey { group: gid.clone(), key }, Some(0), None);
            }
        }
    }

    pub fn announce_to_groups(&mut self) {
        let me = self.my_account_id();
        let gids: Vec<String> = self.p.groups.iter().filter(|g| !g.left && self.is_admin(&g.state)).map(|g| g.state.id.clone()).collect();
        for gid in gids {
            let Some(mut st) = self.group(&gid).map(|g| g.state.clone()) else { continue };
            if let Some(m) = st.members.iter_mut().find(|m| m.account == me) {
                m.devices = self.p.devices.clone().unwrap();
                m.seeds = self.p.own_seeds.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                m.addrs = vec![(self.peer_id.to_string(), self.my_addrs())];
            }
            let _ = self.publish_state(st);
        }
    }

    pub fn group_add(&mut self, gid: &str, id: &str) -> Result<()> {
        let st = self.group(gid).map(|g| g.state.clone()).ok_or_else(|| anyhow!("no such group"))?;
        if !self.is_admin(&st) {
            bail!("only admins can add members");
        }
        if st.members.iter().any(|m| m.account == id) {
            bail!("already a member");
        }
        if st.members.len() >= MAX_MEMBERS {
            bail!("groups are limited to {MAX_MEMBERS} members");
        }
        let m = self.member_from_contact(id)?;
        let mut st2 = st.clone();
        st2.members.push(m);
        self.publish_state(st2)?;
        // The newcomer gets our current key (it cannot read earlier messages).
        self.distribute_key(gid, Some(&[id.to_string()]));
        Ok(())
    }

    pub fn group_remove(&mut self, gid: &str, account: &str) -> Result<()> {
        let st = self.group(gid).map(|g| g.state.clone()).ok_or_else(|| anyhow!("no such group"))?;
        if !self.is_admin(&st) {
            bail!("only admins can remove members");
        }
        let mut st2 = st.clone();
        st2.members.retain(|m| m.account != account);
        if !st2.members.iter().any(|m| m.admin) {
            bail!("a group needs an admin");
        }
        self.publish_state(st2)?;
        self.rotate_group_key(gid);
        Ok(())
    }

    pub fn group_rename(&mut self, gid: &str, name: &str) -> Result<()> {
        let st = self.group(gid).map(|g| g.state.clone()).ok_or_else(|| anyhow!("no such group"))?;
        if !self.is_admin(&st) {
            bail!("only admins can rename the group");
        }
        let mut st2 = st;
        st2.name = name.to_string();
        self.publish_state(st2)
    }

    pub fn group_leave(&mut self, gid: &str) -> Result<()> {
        self.send_to_members(gid, Body::GroupLeave { group: gid.to_string() }, true, false);
        let g = self.group_mut(gid).ok_or_else(|| anyhow!("no such group"))?;
        g.left = true;
        self.group_out.remove(gid);
        self.save();
        self.emit_groups();
        Ok(())
    }

    pub fn group_send(&mut self, gid: &str, text: &str) -> Result<()> {
        let text = text.trim_end();
        if text.is_empty() {
            return Ok(());
        }
        if self.group(gid).map(|g| g.left).unwrap_or(true) {
            bail!("you are not in this group");
        }
        if !self.group_out.contains_key(gid) {
            self.rotate_group_key(gid);
        }
        let id = rand::random::<u64>() | 1;
        let ts = now();
        let plain = serde_json::to_vec(&GroupPlain { id, ts, text: text.to_string() })?;
        let out = self.group_out.get_mut(gid).unwrap();
        let session = out.session_id();
        let ciphertext = out.encrypt(plain).to_base64();
        let me = self.my_account_id();
        let nick = self.p.nick.clone();
        let g = self.group_mut(gid).unwrap();
        g.history.push(GroupLine { id, ts, from: me, nick, text: text.to_string() });
        self.send_to_members(gid, Body::GroupMsg { group: gid.to_string(), session, ciphertext }, true, true);
        self.emit_group_history(gid);
        Ok(())
    }

    pub fn open_group(&mut self, gid: &str) {
        self.open_groups.insert(gid.to_string());
        if let Some(g) = self.group_mut(gid) {
            g.unread = 0;
        }
        self.save();
        self.emit_group_history(gid);
        self.emit_groups();
    }

    /// Keep group traffic that is early (group or key not known yet).
    fn defer(&mut self, owner: &str, device: &str, body: Body) -> Result<()> {
        self.deferred.retain(|(_, _, _, t)| t.elapsed().as_secs() < 600);
        if self.deferred.len() < 500 {
            self.deferred.push((owner.to_string(), device.to_string(), body, std::time::Instant::now()));
        }
        Ok(())
    }

    fn replay_deferred(&mut self, gid: &str) {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred).into_iter().partition(|(_, _, b, _)| group_of(b) == Some(gid));
        self.deferred = rest;
        for (owner, device, body, _) in mine {
            let _ = self.on_group_peer_body(&owner, &device, body);
        }
    }

    /// Group traffic from a member (who may or may not be a contact).
    pub fn on_group_peer_body(&mut self, owner: &str, device: &str, body: Body) -> Result<()> {
        if let Some(gid) = group_of(&body) {
            let known = self.group(gid).is_some();
            if !known && !matches!(body, Body::GroupInvite(_) | Body::GroupUpdate(_)) {
                return self.defer(owner, device, body);
            }
        }
        match body {
            Body::GroupInvite(st) | Body::GroupUpdate(st) => self.on_group_state(owner, st),
            Body::GroupKey { group, key } => {
                let member = self.group(&group).map(|g| g.state.members.iter().any(|m| m.account == owner)).unwrap_or(false);
                if !member && owner != self.my_account_id() {
                    bail!("group key from a non-member");
                }
                let session = if let Some(k) = key.strip_prefix("s:") {
                    InboundGroupSession::new(&SessionKey::from_base64(k).map_err(|e| anyhow!("{e}"))?, SessionConfig::version_1())
                } else if let Some(k) = key.strip_prefix("x:") {
                    InboundGroupSession::import(&ExportedSessionKey::from_base64(k).map_err(|e| anyhow!("{e}"))?, SessionConfig::version_1())
                } else {
                    bail!("bad group key");
                };
                self.group_in.entry(group.clone()).or_default().insert(session.session_id(), session);
                self.save();
                self.replay_deferred(&group);
                Ok(())
            }
            Body::GroupMsg { group, session, ciphertext } => {
                let Some(g) = self.group(&group) else { bail!("unknown group") };
                let nick = g.state.members.iter().find(|m| m.account == owner).map(|m| m.nick.clone()).ok_or_else(|| anyhow!("message from a non-member"))?;
                let nick = self.contact(owner).map(|c| c.petname.clone()).unwrap_or(nick);
                if !self.group_in.get(&group).map(|m| m.contains_key(&session)).unwrap_or(false) {
                    return self.defer(owner, device, Body::GroupMsg { group, session, ciphertext });
                }
                let ins = self.group_in.entry(group.clone()).or_default();
                let s = ins.get_mut(&session).unwrap();
                let msg = MegolmMessage::from_base64(&ciphertext).map_err(|e| anyhow!("{e}"))?;
                let dec = s.decrypt(&msg).map_err(|e| anyhow!("group decrypt: {e}"))?;
                let p: GroupPlain = serde_json::from_slice(&dec.plaintext)?;
                let open = self.open_groups.contains(&group);
                let me = self.my_account_id();
                let g = self.group_mut(&group).unwrap();
                if g.history.iter().any(|l| l.id == p.id) {
                    return Ok(());
                }
                g.history.push(GroupLine { id: p.id, ts: p.ts, from: owner.to_string(), nick: nick.clone(), text: p.text.clone() });
                g.history.sort_by_key(|l| l.ts);
                if !open && owner != me {
                    g.unread += 1;
                }
                let name = g.state.name.clone();
                self.save();
                if owner != self.my_account_id() {
                    self.emit(Event::GroupIncoming { group: group.clone(), name, from: nick, text: p.text });
                }
                self.emit_group_history(&group);
                self.emit_groups();
                Ok(())
            }
            Body::GroupLeave { group } => {
                let st = self.group(&group).map(|g| g.state.clone());
                if let Some(st) = st {
                    if self.is_admin(&st) {
                        let mut st2 = st;
                        st2.members.retain(|m| m.account != owner);
                        if st2.members.iter().any(|m| m.admin) {
                            self.publish_state(st2)?;
                            self.rotate_group_key(&group);
                        }
                    }
                }
                Ok(())
            }
            Body::GroupSyncReq { group, since } => {
                let lines: Vec<GroupLine> = self
                    .group(&group)
                    .filter(|g| g.state.members.iter().any(|m| m.account == owner))
                    .map(|g| g.history.iter().filter(|l| l.ts > since).rev().take(200).cloned().collect())
                    .unwrap_or_default();
                if !lines.is_empty() {
                    let targets = self.member_targets(&group, owner);
                    for t in targets {
                        let _ = self.send_to_target(&t, &Body::GroupSync { group: group.clone(), lines: lines.clone() }, None, None);
                    }
                }
                Ok(())
            }
            Body::GroupSync { group, lines } => {
                let members: Vec<String> = self.group(&group).map(|g| g.state.members.iter().map(|m| m.account.clone()).collect()).unwrap_or_default();
                if !members.contains(&owner.to_string()) && owner != self.my_account_id() {
                    bail!("sync from a non-member");
                }
                let g = self.group_mut(&group).ok_or_else(|| anyhow!("unknown group"))?;
                let mut added = 0;
                for l in lines {
                    if members.contains(&l.from) && !g.history.iter().any(|h| h.id == l.id) {
                        g.history.push(l);
                        added += 1;
                    }
                }
                g.history.sort_by_key(|l| l.ts);
                if added > 0 {
                    self.save();
                    self.emit_group_history(&group);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_group_state(&mut self, owner: &str, st: GroupState) -> Result<()> {
        identity::verify_group(&st)?;
        let me = self.my_account_id();
        let signer = identity::account_id(&st.by)?;
        if signer != owner && owner != me {
            bail!("group state relayed by someone else");
        }
        let existing = self.group(&st.id).map(|g| g.state.clone());
        if let Some(cur) = &existing {
            if st.version <= cur.version {
                return Ok(());
            }
        }
        let in_group = st.members.iter().any(|m| m.account == me);
        let gid = st.id.clone();
        match existing {
            None => {
                if !in_group {
                    return Ok(());
                }
                self.p.groups.push(GroupRec { state: st.clone(), outbound: None, inbound: HashMap::new(), history: vec![], unread: 0, left: false });
                self.group_out.insert(gid.clone(), GroupSession::new(SessionConfig::version_1()));
                self.distribute_key(&gid, None);
                let from = self.contact(owner).map(|c| c.petname.clone()).unwrap_or_else(|| "someone".into());
                self.notice(format!("{from} added you to the group \"{}\".", st.name));
            }
            Some(cur) => {
                let added: Vec<String> = st.members.iter().filter(|m| !cur.members.iter().any(|c| c.account == m.account)).map(|m| m.account.clone()).collect();
                let removed = cur.members.iter().any(|c| !st.members.iter().any(|m| m.account == c.account));
                let g = self.group_mut(&gid).unwrap();
                g.state = st.clone();
                if !in_group {
                    g.left = true;
                    self.group_out.remove(&gid);
                    self.notice(format!("You were removed from \"{}\".", st.name));
                } else if removed {
                    self.rotate_group_key(&gid);
                } else if !added.is_empty() {
                    self.distribute_key(&gid, Some(&added));
                }
            }
        }
        self.save();
        self.emit_groups();
        self.replay_deferred(&gid);
        Ok(())
    }

    /// Connected to a device: ask it for group messages we may have missed.
    pub fn group_catch_up(&mut self, peer: &str) {
        let Some(owner) = self.owner_of_peer(peer) else { return };
        let me = self.my_account_id();
        let account = if owner == "self" { me.clone() } else { owner.clone() };
        let reqs: Vec<(String, i64)> = self
            .p
            .groups
            .iter()
            .filter(|g| !g.left && g.state.members.iter().any(|m| m.account == account))
            .map(|g| (g.state.id.clone(), g.history.last().map(|l| l.ts).unwrap_or(0)))
            .collect();
        for (gid, since) in reqs {
            let targets: Vec<Target> = self.member_targets(&gid, &account).into_iter().filter(|t| t.entry.peer_id == peer).collect();
            for t in targets {
                let _ = self.send_to_target(&t, &Body::GroupSyncReq { group: gid.clone(), since }, None, None);
            }
        }
    }

    pub fn mark_group_delivered(&mut self, _owner: &str, _msg_id: u64) {}

    pub fn emit_groups(&self) {
        let me = self.my_account_id();
        let v = self
            .p
            .groups
            .iter()
            .filter(|g| !g.left)
            .map(|g| GroupView {
                id: g.state.id.clone(),
                name: g.state.name.clone(),
                members: g
                    .state
                    .members
                    .iter()
                    .map(|m| (m.account.clone(), self.contact(&m.account).map(|c| c.petname.clone()).unwrap_or_else(|| if m.account == me { self.p.nick.clone() } else { m.nick.clone() }), m.admin))
                    .collect(),
                admin: g.state.members.iter().any(|m| m.account == me && m.admin),
                unread: g.unread,
            })
            .collect();
        self.emit(Event::Groups(v));
    }

    pub fn emit_group_history(&self, gid: &str) {
        let me = self.my_account_id();
        let Some(g) = self.group(gid) else { return };
        let lines = g
            .history
            .iter()
            .map(|l| LineView {
                id: l.id,
                ts: l.ts,
                from_me: l.from == me,
                who: l.nick.clone(),
                text: l.text.clone(),
                delivery: Delivery::Received,
                edited: false,
                deleted: false,
                reply_to: None,
                urgent: false,
                expires: None,
                file: None,
            })
            .collect();
        self.emit(Event::GroupHistory { group: gid.to_string(), name: g.state.name.clone(), lines });
    }
}

fn group_of(b: &Body) -> Option<&str> {
    match b {
        Body::GroupInvite(s) | Body::GroupUpdate(s) => Some(&s.id),
        Body::GroupKey { group, .. } | Body::GroupMsg { group, .. } | Body::GroupLeave { group } | Body::GroupSyncReq { group, .. } | Body::GroupSync { group, .. } => Some(group),
        _ => None,
    }
}
