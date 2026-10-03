//! Building the views sent to the UI.

use super::*;
use crate::identity::pretty_fingerprint;

impl Engine {
    pub fn line_view(&self, c: &ContactRec, l: &LineRec) -> LineView {
        let reply_to = l.reply_to.and_then(|rid| {
            c.history.iter().find(|x| x.id == rid).map(|x| {
                let snippet: String = x.text.chars().take(60).collect();
                (rid, snippet)
            })
        });
        LineView {
            id: l.id,
            ts: l.ts,
            from_me: l.from_me,
            who: if l.from_me { self.p.nick.clone() } else { c.petname.clone() },
            text: l.text.clone(),
            delivery: l.delivery,
            edited: l.edited,
            deleted: l.deleted,
            reply_to,
            urgent: l.urgent,
            expires: l.expires,
            file: l.file.as_ref().and_then(|f| self.file_view(f)),
            image: l.image.clone(),
        }
    }

    pub fn note_view(&self, l: &LineRec) -> LineView {
        LineView {
            id: l.id,
            ts: l.ts,
            from_me: true,
            who: self.p.nick.clone(),
            text: l.text.clone(),
            delivery: l.delivery,
            edited: false,
            deleted: false,
            reply_to: None,
            urgent: false,
            expires: None,
            file: l.file.as_ref().and_then(|f| self.file_view(f)),
            image: l.image.clone(),
        }
    }

    pub fn emit_contacts(&self) {
        let me = self.peer_id.to_string();
        let mut v: Vec<ContactView> = self
            .p
            .contacts
            .iter()
            .filter(|c| !c.removed)
            .map(|c| {
                let status = if c.authorized { self.contact_status(&c.id) } else { Status::Offline };
                let best = self.presence.values().filter(|r| r.owner == c.id).max_by_key(|r| r.at);
                let devices = c
                    .devices
                    .iter()
                    .map(|d| DeviceView {
                        peer_id: d.entry.peer_id.clone(),
                        name: d.entry.name.clone(),
                        os: d.entry.os.clone(),
                        class: d.entry.class,
                        status: self.presence.get(&d.entry.peer_id).map(|r| r.pres.status).unwrap_or(Status::Offline),
                        manager: d.entry.manager,
                        this_device: d.entry.peer_id == me,
                        last_seen: d.last_seen,
                        remote: String::new(),
                    })
                    .collect();
                ContactView {
                    id: c.id.clone(),
                    name: c.petname.clone(),
                    status,
                    os: best.map(|r| r.pres.os.clone()).unwrap_or_else(|| c.last_os.clone()),
                    device_class: best.map(|r| r.pres.device_class).unwrap_or(c.last_device),
                    on_battery: best.map(|r| r.pres.on_battery).unwrap_or(false),
                    background: best.map(|r| r.pres.background).unwrap_or(false),
                    away_msg: best.map(|r| r.pres.away_msg.clone()).unwrap_or_default(),
                    awaiting: c.awaiting,
                    unread: c.unread,
                    fingerprint: pretty_fingerprint(&c.id),
                    verified: c.verified,
                    folder: c.folder.clone(),
                    visibility: c.visibility,
                    ignored: c.ignored,
                    typing: self.typing.contains_key(&c.id),
                    bot: best.map(|r| r.pres.bot).unwrap_or(false),
                    devices,
                    introduced_by: c.introduced_by.clone(),
                    profile: c.profile.clone(),
                    now_playing: best.map(|r| r.pres.now_playing.clone()).unwrap_or_default(),
                    disappearing: c.disappearing,
                    urgent_allowed: c.urgent_allowed,
                    notify_online: c.notify_online,
                    auto_accept: c.auto_accept,
                }
            })
            .collect();
        v.sort_by_key(|c| (status_rank(c.status), c.name.to_lowercase()));
        self.emit(Event::Contacts(v));
    }

    pub fn emit_pending(&self) {
        let v = self
            .p
            .pending
            .iter()
            .map(|p| {
                let clash = self
                    .p
                    .contacts
                    .iter()
                    .find(|c| !c.removed && c.id != p.id && (c.petname.eq_ignore_ascii_case(&p.card.nick) || c.nick.eq_ignore_ascii_case(&p.card.nick)));
                PendingView {
                    id: p.id.clone(),
                    nick: p.card.nick.clone(),
                    fingerprint: pretty_fingerprint(&p.id),
                    text: p.text.clone(),
                    introduced_by: p.introduced_by.clone(),
                    warning: clash
                        .map(|c| format!("Same name as your contact \"{}\", but a DIFFERENT key. Could be an impersonator or a reinstall — verify before accepting.", c.petname))
                        .unwrap_or_default(),
                }
            })
            .collect();
        self.emit(Event::Pending(v));
    }

    pub fn emit_intros(&self) {
        let v = self.p.intros.iter().enumerate().map(|(i, r)| (i, r.intro.card.nick.clone(), r.from.clone(), r.intro.verified)).collect();
        self.emit(Event::Introductions(v));
    }

    pub fn emit_invites(&self) {
        let now = crate::identity::now();
        let v = self
            .p
            .invites
            .iter()
            .filter(|i| !i.revoked && i.uses_left != Some(0) && i.expires.map(|e| e > now).unwrap_or(true))
            .map(|i| InviteView { token: i.token.clone(), label: i.label.clone(), uses_left: i.uses_left, expires: i.expires, code: i.code.clone() })
            .collect();
        self.emit(Event::Invites(v));
    }

    pub fn emit_history(&self, id: &str) {
        let Some(c) = self.contact(id) else { return };
        self.emit(Event::History {
            id: id.to_string(),
            name: c.petname.clone(),
            fingerprint: pretty_fingerprint(id),
            lines: c.history.iter().map(|l| self.line_view(c, l)).collect(),
        });
    }

    pub fn emit_notes(&self) {
        self.emit(Event::Notes(self.p.notes.iter().map(|l| self.note_view(l)).collect()));
    }

    pub fn emit_devices(&self) {
        let me = self.peer_id.to_string();
        let Some(l) = &self.p.devices else { return };
        let v = l
            .list
            .devices
            .iter()
            .map(|d| DeviceView {
                peer_id: d.peer_id.clone(),
                name: d.name.clone(),
                os: d.os.clone(),
                class: d.class,
                status: if d.peer_id == me { self.p.status } else { self.presence.get(&d.peer_id).map(|r| r.pres.status).unwrap_or(Status::Offline) },
                manager: d.manager,
                this_device: d.peer_id == me,
                last_seen: if d.peer_id == me { crate::identity::now() } else { self.p.own_last_seen.get(&d.peer_id).copied().unwrap_or(0) },
                remote: self.p.remote_status.get(&d.peer_id).cloned().unwrap_or_default(),
            })
            .collect();
        self.emit(Event::Devices(v));
    }

    pub fn emit_settings(&self) {
        self.emit(Event::Settings {
            auto_reply: self.p.auto_reply,
            read_receipts: self.p.read_receipts,
            relays: self.p.net.relays.clone(),
            bootstrap: self.p.net.bootstrap.clone(),
            lan_only: self.p.net.lan_only,
            helper: self.p.net.helper,
            backup_dir: self.p.backup.dir.clone(),
            backup_hours: self.p.backup.every_hours,
            backup_keep: self.p.backup.keep,
            nick: self.p.nick.clone(),
            send_typing: self.p.send_typing,
            bandwidth_kbps: self.p.net.bandwidth_kbps,
            public_helpers: self.p.net.public_helpers,
        });
    }
}

/// Buddy-list order: the most reachable first, like ICQ.
fn status_rank(s: Status) -> u8 {
    match s {
        Status::FreeForChat => 0,
        Status::Online => 1,
        Status::Occupied => 2,
        Status::DoNotDisturb => 3,
        Status::Away => 4,
        Status::NotAvailable => 5,
        Status::Invisible => 6,
        Status::Offline => 7,
    }
}
