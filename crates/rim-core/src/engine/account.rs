//! Account lifecycle: create, unlock, link, restore; device list management;
//! remote lock/wipe; backups.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use libp2p::identity::Keypair;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use vodozemac::megolm::{GroupSession, InboundGroupSession};
use vodozemac::olm::{Account, Session};

use super::msg::Target;
use super::*;
use crate::identity::{self, account_pk_of, b64, now, unb64};
use crate::mailbox;

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "this computer".into())
}

fn fallback_of(olm: &Account) -> String {
    olm.fallback_key().values().next().map(|k| k.to_base64()).unwrap_or_default()
}

fn fresh_device(cfg: &EngineConfig) -> (Keypair, Account, DeviceSeeds, String) {
    let kp = Keypair::generate_ed25519();
    let mut olm = Account::new();
    olm.generate_fallback_key();
    let seeds = DeviceSeeds { inbox: mailbox::new_seed(), rdv: mailbox::new_seed() };
    let name = cfg.device_name.clone().unwrap_or_else(host_name);
    (kp, olm, seeds, name)
}

fn entry_for(kp: &Keypair, olm: &Account, name: &str, os: &str, class: DeviceClass, manager: bool) -> DeviceEntry {
    DeviceEntry {
        peer_id: kp.public().to_peer_id().to_string(),
        curve: olm.curve25519_key().to_base64(),
        fallback: fallback_of(olm),
        name: name.to_string(),
        os: os.to_string(),
        class,
        added: now(),
        manager,
    }
}

fn base_persist(kp: &Keypair, olm: &Account, seeds: DeviceSeeds, name: String) -> Persist {
    let mut own_seeds = HashMap::new();
    own_seeds.insert(kp.public().to_peer_id().to_string(), seeds);
    Persist {
        version: STATE_VERSION,
        nick: String::new(),
        account_pk: String::new(),
        account_key: None,
        device_key: b64(&kp.to_protobuf_encoding().expect("ed25519")),
        device_name: name,
        olm: olm.pickle(),
        devices: None,
        own_seeds,
        own_addrs: HashMap::new(),
        status: Status::Online,
        away_msg: String::new(),
        auto_reply: true,
        read_receipts: false,
        profile: Profile::default(),
        folders: vec![],
        contacts: vec![],
        pending: vec![],
        intros: vec![],
        invites: vec![],
        groups: vec![],
        notes: vec![],
        sessions: HashMap::new(),
        files: vec![],
        held: vec![],
        outbox: vec![],
        net: NetSettings { relays: mailbox::DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(), upnp: true, public_helpers: true, ..Default::default() },
        backup: BackupSettings { keep: 5, ..Default::default() },
        recovery: None,
        settings_updated: now(),
        linking: None,
        seen_mail: vec![],
        lock_requested: false,
        send_typing: true,
        remote_status: HashMap::new(),
        own_last_seen: HashMap::new(),
    }
}

pub(super) async fn open(cfg: EngineConfig, ev: std::sync::mpsc::Sender<Event>, itx: mpsc::UnboundedSender<Internal>) -> Result<Engine> {
    let os = cfg.os_override.clone().unwrap_or_else(crate::device::os_code);
    let class = cfg.device_override.unwrap_or_else(crate::device::device_class);
    let exists = Store::exists(&cfg.dir);
    let mut recovery_words = None;
    let (store, mut p) = match (&cfg.mode, exists) {
        (StartMode::Restore { .. }, true) => bail!("this profile already exists; restore into a new profile"),
        (_, true) => {
            let (store, plain) = Store::open(&cfg.dir, &cfg.passphrase)?;
            let v: serde_json::Value = serde_json::from_slice(&plain).context("state format")?;
            if v.get("version").and_then(|x| x.as_u64()) != Some(STATE_VERSION as u64) {
                // The passphrase was right, so the user owns it: archive and start over.
                let old = Store::path_in(&cfg.dir);
                let archived = old.with_file_name("state.prototype.rim");
                std::fs::rename(&old, &archived)?;
                bail!("This profile came from the RogueIM prototype, which v0.1 cannot read. It was archived as {}. Create your v0.1 account now (your contacts need to add you again).", archived.display());
            }
            (store, serde_json::from_value::<Persist>(v)?)
        }
        (StartMode::Auto, false) => {
            let nick = cfg.nick.clone().filter(|n| !n.trim().is_empty()).ok_or_else(|| anyhow!("nickname required for a new account"))?;
            check_pass(&cfg.passphrase)?;
            let store = Store::create(&cfg.dir, &cfg.passphrase)?;
            let account = Keypair::generate_ed25519();
            let (kp, olm, seeds, name) = fresh_device(&cfg);
            let mut p = base_persist(&kp, &olm, seeds, name.clone());
            p.nick = nick.trim().to_string();
            p.account_pk = account_pk_of(&account);
            p.account_key = Some(b64(&account.to_protobuf_encoding()?));
            let list = DeviceList { account_pk: p.account_pk.clone(), version: 1, devices: vec![entry_for(&kp, &olm, &name, &os, class, true)] };
            p.devices = Some(identity::sign_device_list(&account, list)?);
            let (words, entropy) = identity::new_recovery_words();
            p.recovery = Some(hex::encode(entropy));
            recovery_words = Some(words);
            (store, p)
        }
        (StartMode::Link, false) => {
            check_pass(&cfg.passphrase)?;
            let store = Store::create(&cfg.dir, &cfg.passphrase)?;
            let (kp, olm, seeds, name) = fresh_device(&cfg);
            let mut p = base_persist(&kp, &olm, seeds, name);
            p.linking = Some(LinkingState { code: String::new(), started: now() });
            (store, p)
        }
        (StartMode::Restore { path, secret }, false) => {
            check_pass(&cfg.passphrase)?;
            let data = read_backup(path, secret)?;
            let store = Store::create(&cfg.dir, &cfg.passphrase)?;
            (store, restore_persist(&cfg, data, &os, class)?)
        }
    };
    if let Some(r) = &cfg.relays {
        p.net.relays = r.clone();
    }
    if let Some(l) = cfg.lan_only {
        p.net.lan_only = l;
    }
    p.lock_requested = false;

    let device_key = Keypair::from_protobuf_encoding(&unb64(&p.device_key)?)?;
    let account_key = match &p.account_key {
        Some(k) => Some(Keypair::from_protobuf_encoding(&unb64(k)?)?),
        None => None,
    };
    let olm = Account::from_pickle(std::mem::replace(&mut p.olm, Account::new().pickle()));
    let sessions = std::mem::take(&mut p.sessions)
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(Session::from_pickle).collect()))
        .collect();
    let mut group_out = HashMap::new();
    let mut group_in: HashMap<String, HashMap<String, InboundGroupSession>> = HashMap::new();
    for g in &mut p.groups {
        if let Some(o) = g.outbound.take() {
            group_out.insert(g.state.id.clone(), GroupSession::from_pickle(o));
        }
        let ins = std::mem::take(&mut g.inbound);
        group_in.insert(g.state.id.clone(), ins.into_iter().map(|(k, v)| (k, InboundGroupSession::from_pickle(v))).collect());
    }
    let peer_id = device_key.public().to_peer_id();
    let mut swarm = net::build_swarm(
        device_key.clone(),
        net::NetOpts { mdns: !cfg.no_mdns && !cfg.loopback, upnp: p.net.upnp && !p.net.lan_only && !cfg.loopback, relay_server: cfg.node || p.net.helper, bandwidth_kbps: p.net.bandwidth_kbps },
    )?;
    let port = cfg.port;
    let host = if cfg.loopback { "127.0.0.1" } else { "0.0.0.0" };
    swarm.listen_on(format!("/ip4/{host}/tcp/{port}").parse()?)?;
    swarm.listen_on(format!("/ip4/{host}/udp/{port}/quic-v1").parse()?)?;
    if !cfg.loopback {
        let _ = swarm.listen_on("/ip6/::/tcp/0".parse()?);
    }

    let mut e = Engine {
        store,
        p,
        passphrase: cfg.passphrase.clone(),
        account_key,
        device_key,
        peer_id,
        olm,
        sessions,
        group_out,
        group_in,
        swarm,
        ev,
        os,
        device_class: class,
        bot: cfg.bot,
        node: cfg.node,
        background: false,
        now_playing: String::new(),
        listen: vec![],
        external: vec![],
        presence: HashMap::new(),
        seen_addrs: HashMap::new(),
        typing: HashMap::new(),
        in_flight: HashMap::new(),
        in_flight_set: Default::default(),
        open_chats: Default::default(),
        open_groups: Default::default(),
        mailbox: None,
        loopback: cfg.loopback,
        no_direct: cfg.no_direct,
        observed: vec![],
        live_day: 0,
        last_signal_presence: 0,
        punched: HashMap::new(),
        internal_tx: itx,
        relay_status: vec![],
        nat: "unknown".into(),
        last_rdv_publish: 0,
        last_mail_fetch: 0,
        mail_fetching: false,
        files_rt: Default::default(),
        deferred: vec![],
        quitting: false,
    };
    if cfg.node {
        e.p.net.helper = true;
    }
    // Keep our own entry's OS/class current.
    e.refresh_own_entry();
    for c in e.p.contacts.clone() {
        for d in &c.devices {
            e.register_addrs(&d.entry.peer_id, &d.addrs);
        }
    }
    for (peer, addrs) in e.p.own_addrs.clone() {
        e.register_addrs(&peer, &addrs);
    }
    e.restart_mailbox();
    e.dial_bootstrap();
    e.save();
    if let Some(w) = recovery_words {
        e.emit(Event::RecoveryKey(w));
    }
    if matches!(cfg.mode, StartMode::Restore { .. }) {
        e.announce_device_list();
        e.notice("Account restored as a new device. Your contacts will see a new device, not a key change.");
    }
    Ok(e)
}

fn check_pass(p: &str) -> Result<()> {
    if p.chars().count() < 4 {
        bail!("passphrase too short (min. 4 characters)");
    }
    Ok(())
}

impl Engine {
    /// A manager updates its own entry when OS or class changed.
    fn refresh_own_entry(&mut self) {
        let me = self.peer_id.to_string();
        let (os, class) = (self.os.clone(), self.device_class);
        let Some(list) = self.p.devices.clone() else { return };
        let Some(e) = list.list.devices.iter().find(|d| d.peer_id == me) else { return };
        if e.os == os && e.class == class {
            return;
        }
        if let Some(ak) = &self.account_key {
            let mut l = list.list.clone();
            for d in &mut l.devices {
                if d.peer_id == me {
                    d.os = os.clone();
                    d.class = class;
                }
            }
            l.version += 1;
            if let Ok(s) = identity::sign_device_list(ak, l) {
                self.p.devices = Some(s);
            }
        }
    }

    pub fn restart_mailbox(&mut self) {
        if let Some(m) = self.mailbox.take() {
            tokio::spawn(async move { m.shutdown().await });
        }
        if self.p.net.lan_only || self.p.net.relays.is_empty() {
            return;
        }
        let relays = self.p.net.relays.clone();
        let tx = self.internal_tx.clone();
        // Connect synchronously enough to be usable right away.
        let mb = futures::executor::block_on(async { Mailbox::connect(&relays).await });
        let probe = mb.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let _ = tx.send(Internal::RelayStatus(probe.relay_status().await));
        });
        self.mailbox = Some(mb);
        self.last_mail_fetch = 0;
        self.last_rdv_publish = 0;
        self.live_day = 0;
    }

    // ================================================================ linking (new device side)

    pub fn link_code_refresh(&mut self) {
        let Some(l) = &self.p.linking else { return };
        let old = l.code.clone();
        let nonce = if old.is_empty() {
            hex::encode(rand::random::<[u8; 8]>())
        } else {
            identity::decode_link(&old).map(|c| c.nonce).unwrap_or_default()
        };
        let code = identity::LinkCode {
            v: 2,
            peer_id: self.peer_id.to_string(),
            curve: self.olm.curve25519_key().to_base64(),
            fallback: fallback_of(&self.olm),
            name: self.p.device_name.clone(),
            os: self.os.clone(),
            class: self.device_class,
            seeds: self.my_seeds(),
            addrs: self.my_addrs(),
            nonce,
        };
        let Ok(s) = identity::encode_link(&code) else { return };
        if s != old {
            self.p.linking.as_mut().unwrap().code = s.clone();
            self.save();
            self.emit(Event::LinkCode { code: s, words: identity::link_words(&code) });
        }
    }

    pub fn on_link_grant(&mut self, g: LinkGrant) -> Result<()> {
        identity::verify_device_list(&g.devices)?;
        let me = self.peer_id.to_string();
        let mine = g.devices.list.devices.iter().find(|d| d.peer_id == me).ok_or_else(|| anyhow!("grant does not include this device"))?;
        if mine.curve != self.olm.curve25519_key().to_base64() {
            bail!("grant lists a different key for this device");
        }
        if let Some(k) = &g.account_key {
            let kp = Keypair::from_protobuf_encoding(&unb64(k)?)?;
            if account_pk_of(&kp) != g.devices.list.account_pk {
                bail!("account key does not match");
            }
            self.account_key = Some(kp);
            self.p.account_key = Some(k.clone());
        }
        self.p.nick = g.nick;
        self.p.account_pk = g.devices.list.account_pk.clone();
        self.p.devices = Some(g.devices);
        for (peer, s) in g.own_seeds {
            if peer != me {
                self.p.own_seeds.insert(peer, s);
            }
        }
        for cs in g.contacts {
            self.merge_contact_sync(cs);
        }
        if let Some(v) = g.settings.away_msg {
            self.p.away_msg = v;
        }
        if let Some(v) = g.settings.profile {
            self.p.profile = v;
        }
        if let Some(v) = g.settings.folders {
            self.p.folders = v;
        }
        for gs in g.groups {
            if identity::verify_group(&gs).is_ok() {
                self.p.groups.push(GroupRec { state: gs, outbound: None, inbound: HashMap::new(), history: vec![], unread: 0, left: false });
            }
        }
        self.p.linking = None;
        self.save();
        // Tell the other devices about our seeds, and every contact about us.
        let _ = self.send_body("self", Body::Seeds(self.my_seeds()), Some(0));
        let owners: Vec<String> = self.p.contacts.iter().filter(|c| c.authorized && !c.removed).map(|c| c.id.clone()).collect();
        for o in owners {
            let _ = self.send_body(&o, Body::Seeds(self.my_seeds()), Some(0));
        }
        for gid in self.p.groups.iter().map(|g| g.state.id.clone()).collect::<Vec<_>>() {
            self.rotate_group_key(&gid);
        }
        self.emit(Event::Linked);
        self.emit_unlocked();
        self.emit_all();
        Ok(())
    }

    // ================================================================ linking (manager side)

    pub fn link_device(&mut self, code: &str, manager: bool, history_days: Option<u32>) -> Result<()> {
        let ak = self.account_key.clone().ok_or_else(|| anyhow!("only a manager device can link new devices"))?;
        let code = identity::decode_link(code)?;
        let mut list = self.p.devices.clone().context("device list")?.list;
        if list.devices.iter().any(|d| d.peer_id == code.peer_id) {
            bail!("that device is already linked");
        }
        let entry = DeviceEntry {
            peer_id: code.peer_id.clone(),
            curve: code.curve.clone(),
            fallback: code.fallback.clone(),
            name: code.name.clone(),
            os: code.os.clone(),
            class: code.class,
            added: now(),
            manager,
        };
        list.devices.push(entry.clone());
        list.version += 1;
        self.p.devices = Some(identity::sign_device_list(&ak, list)?);
        self.p.own_seeds.insert(code.peer_id.clone(), code.seeds.clone());
        self.p.own_addrs.insert(code.peer_id.clone(), code.addrs.clone());
        self.register_addrs(&code.peer_id, &code.addrs);
        self.save();

        let contacts: Vec<ContactSync> = self
            .p
            .contacts
            .iter()
            .map(|c| ContactSync {
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
            })
            .collect();
        let grant = LinkGrant {
            nick: self.p.nick.clone(),
            devices: self.p.devices.clone().unwrap(),
            account_key: if manager { self.p.account_key.clone() } else { None },
            contacts,
            own_seeds: self.p.own_seeds.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            settings: SettingsSync {
                status: Some(self.p.status),
                away_msg: Some(self.p.away_msg.clone()),
                auto_reply: Some(self.p.auto_reply),
                profile: Some(self.p.profile.clone()),
                folders: Some(self.p.folders.clone()),
                updated: self.p.settings_updated,
                nick: Some(self.p.nick.clone()),
                send_typing: Some(self.p.send_typing),
            },
            groups: self.p.groups.iter().filter(|g| !g.left).map(|g| g.state.clone()).collect(),
        };
        let t = Target { owner: "self".into(), entry, seeds: Some(code.seeds.clone()), addrs: code.addrs.clone() };
        self.send_to_target(&t, &Body::LinkGrant(Box::new(grant)), Some(0), None)?;
        // History, in chunks.
        if let Some(days) = history_days {
            let since = if days == 0 { i64::MIN } else { now() - days as i64 * 86_400 };
            let chunks: Vec<(String, Vec<HistoryLine>)> = self
                .p
                .contacts
                .iter()
                .filter(|c| !c.removed)
                .flat_map(|c| {
                    let lines: Vec<HistoryLine> = c
                        .history
                        .iter()
                        .filter(|l| l.ts >= since && !l.deleted)
                        .map(|l| HistoryLine { id: l.id, ts: l.ts, from_me: l.from_me, text: l.text.clone() })
                        .collect();
                    lines.chunks(200).map(|ch| (c.id.clone(), ch.to_vec())).collect::<Vec<_>>()
                })
                .collect();
            for (contact, lines) in chunks {
                self.send_to_target(&t, &Body::History { contact, lines }, Some(0), None)?;
            }
        }
        // Group keys so the new device can read group history.
        self.share_group_keys_with(&t);
        self.announce_device_list();
        self.save();
        self.emit_devices();
        self.notice(format!("Linked \"{}\".", code.name));
        Ok(())
    }

    /// Send our current device list to every contact, own device and group peer.
    pub fn announce_device_list(&mut self) {
        let Some(list) = self.p.devices.clone() else { return };
        let owners: Vec<String> = self.p.contacts.iter().filter(|c| !c.removed && c.authorized).map(|c| c.id.clone()).collect();
        for o in owners {
            let _ = self.send_body(&o, Body::DeviceList(list.clone()), Some(0));
        }
        let _ = self.send_body("self", Body::DeviceList(list.clone()), Some(0));
        self.announce_to_groups();
    }

    pub fn on_own_device_list(&mut self, list: SignedDeviceList) -> Result<()> {
        identity::verify_device_list(&list)?;
        if list.list.account_pk != self.p.account_pk {
            bail!("device list for another account");
        }
        let cur = self.p.devices.as_ref().map(|l| l.list.version).unwrap_or(0);
        if list.list.version <= cur {
            return Ok(());
        }
        let me = self.peer_id.to_string();
        if !list.list.devices.iter().any(|d| d.peer_id == me) {
            self.p.devices = Some(list);
            self.save();
            self.notice("This device was removed from the account on another device.");
            self.emit(Event::Locked);
            self.quitting = true;
            return Ok(());
        }
        let old: Vec<DeviceEntry> = self.p.devices.as_ref().map(|l| l.list.devices.clone()).unwrap_or_default();
        for d in &old {
            if !list.list.devices.iter().any(|n| n.peer_id == d.peer_id) {
                self.sessions.remove(&d.curve);
                self.p.own_seeds.remove(&d.peer_id);
            }
        }
        self.p.devices = Some(list);
        self.save();
        self.emit_devices();
        Ok(())
    }

    pub fn rename_device(&mut self, peer: &str, name: &str) -> Result<()> {
        let ak = self.account_key.clone().ok_or_else(|| anyhow!("only a manager device can rename devices"))?;
        let mut list = self.p.devices.clone().context("device list")?.list;
        let d = list.devices.iter_mut().find(|d| d.peer_id == peer).ok_or_else(|| anyhow!("no such device"))?;
        d.name = name.to_string();
        list.version += 1;
        self.p.devices = Some(identity::sign_device_list(&ak, list)?);
        if peer == self.peer_id.to_string() {
            self.p.device_name = name.to_string();
        }
        self.save();
        self.announce_device_list();
        self.emit_devices();
        Ok(())
    }

    pub fn revoke_device(&mut self, peer: &str, wipe: bool) -> Result<()> {
        let ak = self.account_key.clone().ok_or_else(|| anyhow!("only a manager device can remove devices"))?;
        if peer == self.peer_id.to_string() {
            bail!("use \"wipe account\" to remove this device");
        }
        let mut list = self.p.devices.clone().context("device list")?.list;
        let entry = list.devices.iter().find(|d| d.peer_id == peer).cloned().ok_or_else(|| anyhow!("no such device"))?;
        if wipe {
            let cmd = identity::sign_remote(&ak, peer, RemoteAction::Wipe, now())?;
            let t = Target {
                owner: "self".into(),
                entry: entry.clone(),
                seeds: self.p.own_seeds.get(peer).cloned(),
                addrs: self.p.own_addrs.get(peer).cloned().unwrap_or_default(),
            };
            self.send_to_target(&t, &Body::Remote(cmd), Some(0), None)?;
        }
        list.devices.retain(|d| d.peer_id != peer);
        list.version += 1;
        self.p.devices = Some(identity::sign_device_list(&ak, list)?);
        if !wipe {
            self.sessions.remove(&entry.curve);
            self.p.own_seeds.remove(peer);
        }
        self.save();
        self.announce_device_list();
        self.emit_devices();
        self.notice(format!("Removed \"{}\"{}.", entry.name, if wipe { "; it will erase itself when it next connects" } else { "" }));
        Ok(())
    }

    pub fn remote(&mut self, peer: &str, action: RemoteAction) -> Result<()> {
        let entry = self
            .p
            .devices
            .as_ref()
            .and_then(|l| l.list.devices.iter().find(|d| d.peer_id == peer).cloned())
            .ok_or_else(|| anyhow!("no such device"))?;
        let cmd = match (&self.account_key, action) {
            (Some(ak), _) => identity::sign_remote(ak, peer, action, now())?,
            (None, RemoteAction::Lock) => RemoteCommand { target: peer.to_string(), action, ts: now(), sig: String::new() },
            (None, RemoteAction::Wipe) => bail!("only a manager device can wipe devices"),
        };
        let t = Target { owner: "self".into(), entry, seeds: self.p.own_seeds.get(peer).cloned(), addrs: self.p.own_addrs.get(peer).cloned().unwrap_or_default() };
        self.send_to_target(&t, &Body::Remote(cmd), Some(0), None)?;
        self.p.remote_status.insert(peer.to_string(), "lock pending".into());
        self.save();
        self.emit_devices();
        self.notice("Lock sent. The device locks when it receives it.");
        Ok(())
    }

    pub fn on_remote(&mut self, cmd: RemoteCommand) -> Result<()> {
        if cmd.target != self.peer_id.to_string() {
            return Ok(());
        }
        // Confirm first: after a wipe there is nobody left to answer.
        let ack = Body::RemoteAck { action: cmd.action, ts: now() };
        if cmd.action == RemoteAction::Wipe {
            identity::verify_remote(&self.p.account_pk, &cmd)?;
        }
        let _ = self.send_body("self", ack, Some(0));
        match cmd.action {
            RemoteAction::Lock => {
                self.p.lock_requested = true;
                self.save();
                self.emit(Event::Locked);
                self.quitting = true;
            }
            RemoteAction::Wipe => {
                identity::verify_remote(&self.p.account_pk, &cmd)?;
                self.wipe_local();
            }
        }
        Ok(())
    }

    /// Crypto-erase: drop key material and delete the state file.
    pub fn wipe_local(&mut self) {
        let dir = self.store_dir();
        self.p.account_key = None;
        self.p.device_key.clear();
        self.p.contacts.clear();
        self.p.notes.clear();
        self.sessions.clear();
        let _ = std::fs::remove_file(Store::path_in(&dir));
        let _ = std::fs::remove_file(Store::path_in(&dir).with_extension("tmp"));
        self.emit(Event::Wiped);
        self.quitting = true;
    }

    pub fn store_dir(&self) -> std::path::PathBuf {
        self.store.dir()
    }

    // ================================================================ backups

    pub fn show_recovery_key(&mut self) -> Result<()> {
        // Words are shown once at creation; afterwards a new key can be issued.
        let (words, entropy) = identity::new_recovery_words();
        self.p.recovery = Some(hex::encode(entropy));
        self.save();
        self.emit(Event::RecoveryKey(words));
        self.notice("New recovery key issued. Backups made from now on open with it; older backups still need the old one.");
        Ok(())
    }

    fn backup_data(&self, include_files: bool) -> BackupData {
        let mut groups: Vec<GroupRec> = serde_json::from_value(serde_json::to_value(&self.p.groups).unwrap_or_default()).unwrap_or_default();
        for g in &mut groups {
            g.outbound = None;
            if let Some(ins) = self.group_in.get(&g.state.id) {
                g.inbound = ins.iter().map(|(k, s)| (k.clone(), s.pickle())).collect();
            }
        }
        let files = if include_files {
            self.p
                .files
                .iter()
                .filter(|f| !f.outgoing && f.state == FileState::Done)
                .filter_map(|f| {
                    let data = std::fs::read(&f.path).ok()?;
                    (data.len() < 100 * 1024 * 1024).then(|| (f.offer.name.clone(), b64(&data)))
                })
                .collect()
        } else {
            vec![]
        };
        BackupData {
            version: 2,
            nick: self.p.nick.clone(),
            account_pk: self.p.account_pk.clone(),
            account_key: self.p.account_key.clone(),
            devices: self.p.devices.clone(),
            own_seeds: self.p.own_seeds.clone(),
            contacts: self.p.contacts.clone(),
            pending: self.p.pending.clone(),
            groups,
            notes: self.p.notes.clone(),
            profile: self.p.profile.clone(),
            folders: self.p.folders.clone(),
            status: self.p.status,
            away_msg: self.p.away_msg.clone(),
            auto_reply: self.p.auto_reply,
            net: self.p.net.clone(),
            recovery: self.p.recovery.clone(),
            files,
            created: now(),
        }
    }

    pub fn export_backup(&mut self, path: &str, include_files: bool) -> Result<()> {
        let data = self.backup_data(include_files);
        let rec = self.p.recovery.as_ref().map(|h| hex::decode(h)).transpose()?;
        write_backup(Path::new(path), &data, &self.passphrase, rec.as_deref())?;
        self.notice(format!("Backup written to {path}."));
        Ok(())
    }

    pub fn import_history(&mut self, path: &str, secret: RestoreSecret) -> Result<()> {
        // An empty passphrase means "the one this device is unlocked with".
        let secret = match secret {
            RestoreSecret::Passphrase(p) if p.is_empty() => RestoreSecret::Passphrase(self.passphrase.clone()),
            s => s,
        };
        let data = read_backup(Path::new(path), &secret)?;
        if data.account_pk != self.p.account_pk {
            bail!("that backup belongs to another account");
        }
        let mut n = 0;
        for bc in data.contacts {
            if let Some(c) = self.p.contacts.iter_mut().find(|c| c.id == bc.id) {
                for l in bc.history {
                    if !c.history.iter().any(|h| h.id == l.id) {
                        c.history.push(l);
                        n += 1;
                    }
                }
                c.history.sort_by_key(|l| l.ts);
            }
        }
        for l in data.notes {
            if !self.p.notes.iter().any(|h| h.id == l.id) {
                self.p.notes.push(l);
                n += 1;
            }
        }
        self.save();
        self.notice(format!("Imported {n} messages."));
        Ok(())
    }

    pub fn backup_heartbeat(&mut self, now: i64) {
        let b = self.p.backup.clone();
        if b.dir.is_empty() || b.every_hours == 0 || now - b.last < b.every_hours as i64 * 3600 {
            return;
        }
        let t = time::OffsetDateTime::from_unix_timestamp(now).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let name = format!(
            "rogueim-{}-{:04}{:02}{:02}-{:02}{:02}.rimbackup",
            self.p.nick.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute()
        );
        let path = Path::new(&b.dir).join(name);
        match self.export_backup(&path.to_string_lossy(), b.include_files) {
            Ok(()) => {
                self.p.backup.last = now;
                self.save();
                // Keep only the newest N.
                if let Ok(rd) = std::fs::read_dir(&b.dir) {
                    let mut files: Vec<_> = rd
                        .flatten()
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".rimbackup") && e.file_name().to_string_lossy().starts_with("rogueim-"))
                        .collect();
                    files.sort_by_key(|e| e.file_name());
                    while files.len() > b.keep.max(1) as usize {
                        let _ = std::fs::remove_file(files.remove(0).path());
                    }
                }
            }
            Err(e) => self.notice(format!("Scheduled backup failed: {e:#}")),
        }
    }
}

// ---------------------------------------------------------------- backup file

#[derive(Serialize, Deserialize)]
struct BackupData {
    version: u32,
    nick: String,
    account_pk: String,
    account_key: Option<String>,
    devices: Option<SignedDeviceList>,
    own_seeds: HashMap<String, DeviceSeeds>,
    contacts: Vec<ContactRec>,
    pending: Vec<PendingRec>,
    groups: Vec<GroupRec>,
    notes: Vec<LineRec>,
    profile: Profile,
    folders: Vec<String>,
    status: Status,
    away_msg: String,
    auto_reply: bool,
    net: NetSettings,
    recovery: Option<String>,
    files: Vec<(String, String)>,
    created: i64,
}

#[derive(Serialize, Deserialize)]
struct BackupHeader {
    v: u32,
    salt: String,
    wrapped_pass: String,
    wrapped_recovery: Option<String>,
}

const BACKUP_MAGIC: &[u8; 5] = b"RIMB2";

fn aead_seal(key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>> {
    let nonce: [u8; 24] = rand::random();
    let c = XChaCha20Poly1305::new(&(*key).into());
    let mut out = nonce.to_vec();
    out.extend(c.encrypt(&XNonce::try_from(&nonce[..]).map_err(|_| anyhow!("nonce"))?, plain).map_err(|_| anyhow!("encrypt"))?);
    Ok(out)
}

fn aead_open(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 24 {
        bail!("damaged backup");
    }
    let c = XChaCha20Poly1305::new(&(*key).into());
    c.decrypt(&XNonce::try_from(&data[..24]).map_err(|_| anyhow!("nonce"))?, &data[24..]).map_err(|_| anyhow!("wrong passphrase / recovery key, or damaged backup"))
}

fn pass_key(pass: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut k = [0u8; 32];
    Argon2::default().hash_password_into(pass.as_bytes(), salt, &mut k).map_err(|e| anyhow!("argon2: {e}"))?;
    Ok(k)
}

fn recovery_key(entropy: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"rim-recovery-wrap-v2");
    h.update(entropy);
    h.finalize().into()
}

fn write_backup(path: &Path, data: &BackupData, pass: &str, recovery: Option<&[u8]>) -> Result<()> {
    let dk: [u8; 32] = rand::random();
    let salt: [u8; 16] = rand::random();
    let header = BackupHeader {
        v: 2,
        salt: b64(&salt),
        wrapped_pass: b64(&aead_seal(&pass_key(pass, &salt)?, &dk)?),
        wrapped_recovery: recovery.map(|e| aead_seal(&recovery_key(e), &dk).map(|x| b64(&x))).transpose()?,
    };
    let h = serde_json::to_vec(&header)?;
    let body = aead_seal(&dk, &serde_json::to_vec(data)?)?;
    let mut out = BACKUP_MAGIC.to_vec();
    out.extend((h.len() as u32).to_le_bytes());
    out.extend(h);
    out.extend(body);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn read_backup(path: &Path, secret: &RestoreSecret) -> Result<BackupData> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.len() < 9 || &raw[..5] != BACKUP_MAGIC {
        bail!("not a RogueIM v0.1 backup");
    }
    let hl = u32::from_le_bytes(raw[5..9].try_into()?) as usize;
    let header: BackupHeader = serde_json::from_slice(raw.get(9..9 + hl).ok_or_else(|| anyhow!("damaged backup"))?)?;
    let dk: Vec<u8> = match secret {
        RestoreSecret::Passphrase(p) => aead_open(&pass_key(p, &unb64(&header.salt)?)?, &unb64(&header.wrapped_pass)?)?,
        RestoreSecret::Recovery(words) => {
            let e = identity::recovery_key_from_words(words)?;
            let w = header.wrapped_recovery.ok_or_else(|| anyhow!("this backup has no recovery key"))?;
            aead_open(&recovery_key(&e), &unb64(&w)?)?
        }
    };
    let dk: [u8; 32] = dk.try_into().map_err(|_| anyhow!("damaged backup"))?;
    let plain = aead_open(&dk, &raw[9 + hl..])?;
    Ok(serde_json::from_slice(&plain)?)
}

fn restore_persist(cfg: &EngineConfig, data: BackupData, os: &str, class: DeviceClass) -> Result<Persist> {
    let ak_str = data.account_key.clone().ok_or_else(|| anyhow!("this backup came from a non-manager device and cannot restore the account; link this computer from a manager device instead"))?;
    let ak = Keypair::from_protobuf_encoding(&unb64(&ak_str)?)?;
    let (kp, olm, seeds, name) = fresh_device(cfg);
    let mut p = base_persist(&kp, &olm, seeds, name.clone());
    p.nick = data.nick;
    p.account_pk = data.account_pk;
    p.account_key = Some(ak_str);
    let mut list = data.devices.map(|d| d.list).unwrap_or(DeviceList { account_pk: p.account_pk.clone(), version: 0, devices: vec![] });
    list.devices.push(entry_for(&kp, &olm, &name, os, class, true));
    list.version += 1;
    p.devices = Some(identity::sign_device_list(&ak, list)?);
    for (k, v) in data.own_seeds {
        p.own_seeds.entry(k).or_insert(v);
    }
    p.contacts = data.contacts;
    p.pending = data.pending;
    p.groups = data.groups;
    p.notes = data.notes;
    p.profile = data.profile;
    p.folders = data.folders;
    p.status = data.status;
    p.away_msg = data.away_msg;
    p.auto_reply = data.auto_reply;
    p.net = data.net;
    p.recovery = data.recovery;
    // Received files come back into the downloads folder.
    if !data.files.is_empty() {
        let dir = directories::UserDirs::new().and_then(|u| u.download_dir().map(|d| d.join("RogueIM restored"))).unwrap_or_else(|| cfg.dir.join("files"));
        std::fs::create_dir_all(&dir).ok();
        for (name, b) in data.files {
            if let Ok(bytes) = unb64(&b) {
                let _ = std::fs::write(dir.join(sanitize(&name)), bytes);
            }
        }
    }
    Ok(p)
}

pub fn sanitize(name: &str) -> String {
    let n: String = name.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let n = n.trim().trim_start_matches('.').to_string();
    if n.is_empty() {
        "file".into()
    } else {
        n
    }
}
