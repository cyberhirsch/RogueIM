#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! RogueIM desktop: a buddy list that is always docked to a screen edge and
//! always on top, chats in their own windows or docked into the bar.

mod desktop;
mod dock;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rim_core::engine::{RestoreSecret, StartMode};
use rim_core::store::Store;
use rim_core::*;
use slint::{CloseRequestResponse, Color, ComponentHandle, ModelRc, SharedString, VecModel};
use time::UtcOffset;

slint::include_modules!();

const DOCK_WIDTH: f32 = 280.0;
const STRIP_WIDTH: f32 = 5.0;

// ================================================================ settings & themes

/// Per-profile UI settings. Not secret, so plain JSON next to the state file.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
struct Settings {
    theme: String,
    dock_left: bool,
    monitor: String,
    split: f32,
    hide_offline: bool,
    sounds: bool,
    notify: bool,
    popups: bool,
    autostart: bool,
    autohide: bool,
    auto_away: u32,
    auto_na: u32,
    lock_idle: u32,
    collapsed_folders: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "graphite".into(),
            dock_left: false,
            monitor: String::new(),
            split: 0.5,
            hide_offline: false,
            sounds: true,
            notify: true,
            popups: true,
            autostart: false,
            autohide: false,
            auto_away: 10,
            auto_na: 30,
            lock_idle: 0,
            collapsed_folders: vec![],
        }
    }
}

fn load_settings(dir: &Path) -> Settings {
    std::fs::read(dir.join("ui.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_settings(dir: &Path, s: &Settings) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join("ui.json"), serde_json::to_vec_pretty(s).unwrap_or_default());
}

struct Palette {
    name: &'static str,
    dark: bool,
    bg: u32,
    panel: u32,
    line: u32,
    fg: u32,
    dim: u32,
    faint: u32,
    accent: u32,
    red: u32,
    hover: u32,
    on_accent: u32,
    other: u32,
}

/// Graphite (default, dark grey), Grey (Win9x-era silver), Green and Amber (terminal).
const THEMES: [Palette; 4] = [
    Palette { name: "graphite", dark: true, bg: 0x1e1f22, panel: 0x2a2c30, line: 0x3a3d42, fg: 0xd6d6d6, dim: 0x8a8f98, faint: 0x54585f, accent: 0xe0a040, red: 0xff6b6b, hover: 0x34373c, on_accent: 0x1e1f22, other: 0x6cb6ff },
    Palette { name: "grey", dark: false, bg: 0xc0c0c0, panel: 0xffffff, line: 0x808080, fg: 0x000000, dim: 0x404040, faint: 0x808080, accent: 0x000080, red: 0xa00000, hover: 0xd8d8e8, on_accent: 0xffffff, other: 0x7a3e9d },
    Palette { name: "green", dark: true, bg: 0x070a07, panel: 0x0d130e, line: 0x1c3322, fg: 0x7cfc9a, dim: 0x3f7a4f, faint: 0x24402c, accent: 0xffb000, red: 0xff5b5b, hover: 0x1c3322, on_accent: 0x070a07, other: 0x00e5ff },
    Palette { name: "amber", dark: true, bg: 0x0a0700, panel: 0x140e02, line: 0x3a2a08, fg: 0xffb000, dim: 0x9a6a00, faint: 0x4a3500, accent: 0xffd870, red: 0xff5b5b, hover: 0x3a2a08, on_accent: 0x0a0700, other: 0x9ad0ff },
];

fn palette(name: &str) -> &'static Palette {
    THEMES.iter().find(|p| p.name == name).unwrap_or(&THEMES[0])
}

fn rgb(c: u32) -> Color {
    Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn apply_theme(t: &Theme<'_>, p: &Palette) {
    t.set_bg(rgb(p.bg));
    t.set_panel(rgb(p.panel));
    t.set_line(rgb(p.line));
    t.set_fg(rgb(p.fg));
    t.set_dim(rgb(p.dim));
    t.set_faint(rgb(p.faint));
    t.set_accent(rgb(p.accent));
    t.set_red(rgb(p.red));
    t.set_hover(rgb(p.hover));
    t.set_on_accent(rgb(p.on_accent));
    t.set_other(rgb(p.other));
}

fn tint_rgb(s: Status, dark: bool) -> u32 {
    match (s, dark) {
        (Status::Online, true) => 0x7cfc9a,
        (Status::FreeForChat, true) => 0x00e5ff,
        (Status::Away, true) => 0xffb000,
        (Status::NotAvailable, true) => 0xff8c1a,
        (Status::Occupied, true) => 0xff5b5b,
        (Status::DoNotDisturb, true) => 0xff2f6d,
        (Status::Invisible, true) => 0x8a8a8a,
        (Status::Offline, true) => 0x4f5a52,
        (Status::Online, false) => 0x007a1f,
        (Status::FreeForChat, false) => 0x006d8f,
        (Status::Away, false) => 0x8a5a00,
        (Status::NotAvailable, false) => 0xb04a00,
        (Status::Occupied, false) => 0xb00000,
        (Status::DoNotDisturb, false) => 0x8a0030,
        (Status::Invisible, false) => 0x606060,
        (Status::Offline, false) => 0x8a8a8a,
    }
}

fn tint(s: Status, dark: bool) -> Color {
    rgb(tint_rgb(s, dark))
}

// ================================================================ app state

struct App {
    dir: PathBuf,
    profile: String,
    port: u16,
    engine: Option<EngineHandle>,
    rx: Option<Receiver<Event>>,
    main: slint::Weak<MainWindow>,
    chats: HashMap<String, ChatWindow>,
    histories: HashMap<String, Vec<LineView>>,
    files: HashMap<String, FileView>,
    contacts: Vec<ContactView>,
    groups: Vec<GroupView>,
    my_nick: String,
    my_status: Status,
    settings: Settings,
    layout: Vec<dock::Monitor>,
    docked_chat: Option<String>,
    pick: HashSet<String>,
    detail_id: String,
    group_id: String,
    notice_until: Option<Instant>,
    quitting: bool,
    pending_recovery: Option<(String, Option<Event>)>,
    auto_prev: Option<Status>,
    last_incoming: Option<String>,
    last_typing: HashMap<String, Instant>,
    collapsed: bool,
    hover_left_at: Option<Instant>,
    blink: bool,
    offset: UtcOffset,
    audio: desktop::Audio,
    tray: Option<desktop::Tray>,
    hotkeys: desktop::Hotkeys,
}

type AppRc = Rc<RefCell<App>>;

impl App {
    fn dark(&self) -> bool {
        palette(&self.settings.theme).dark
    }
    fn send(&self, c: Command) {
        if let Some(e) = &self.engine {
            e.send(c);
        }
    }
    fn contact(&self, id: &str) -> Option<&ContactView> {
        self.contacts.iter().find(|c| c.id == id)
    }
}

fn fmt_time(ts: i64, off: UtcOffset) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .map(|t| t.to_offset(off))
        .map(|t| format!("{:02}:{:02}", t.hour(), t.minute()))
        .unwrap_or_default()
}

fn fmt_date(ts: i64, off: UtcOffset) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .map(|t| t.to_offset(off))
        .map(|t| format!("{:02}.{:02}. {:02}:{:02}", t.day(), t.month() as u8, t.hour(), t.minute()))
        .unwrap_or_default()
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn file_state(s: FileState) -> &'static str {
    match s {
        FileState::Offered => "offered",
        FileState::Waiting => "waiting",
        FileState::Transferring => "transferring",
        FileState::Paused => "paused",
        FileState::Done => "done",
        FileState::Declined => "declined",
        FileState::Failed => "failed",
        FileState::NoDirect => "nodirect",
    }
}

fn chat_line(app: &App, l: &LineView) -> ChatLine {
    let f = l.file.as_ref().map(|f| app.files.get(&f.id).cloned().unwrap_or_else(|| f.clone()));
    ChatLine {
        id: l.id.to_string().into(),
        time: fmt_time(l.ts, app.offset).into(),
        who: l.who.clone().into(),
        text: l.text.clone().into(),
        mine: l.from_me,
        state: match (l.from_me, l.delivery) {
            (false, _) => "",
            (true, Delivery::Queued) => "queued",
            (true, Delivery::Stored) => "stored",
            (true, Delivery::Read) => "read",
            (true, Delivery::Failed) => "failed",
            (true, _) => "",
        }
        .into(),
        edited: l.edited,
        deleted: l.deleted,
        urgent: l.urgent,
        expires: l.expires.is_some(),
        reply_text: l.reply_to.as_ref().map(|(_, t)| t.clone()).unwrap_or_default().into(),
        file_id: f.as_ref().map(|f| f.id.clone()).unwrap_or_default().into(),
        file_name: f.as_ref().map(|f| f.name.clone()).unwrap_or_default().into(),
        file_size: f.as_ref().map(|f| human_size(f.size)).unwrap_or_default().into(),
        file_progress: f.as_ref().map(|f| if f.size == 0 { 1.0 } else { f.done as f32 / f.size as f32 }).unwrap_or(0.0),
        file_state: f.as_ref().map(|f| file_state(f.state)).unwrap_or("").into(),
    }
}

// ================================================================ chats

/// Chat keys: a contact id, "g:<group id>", or "notes".
fn mode_of(key: &str) -> &'static str {
    if key == "notes" {
        "notes"
    } else if key.starts_with("g:") {
        "group"
    } else {
        "contact"
    }
}

struct Header {
    name: String,
    fp: String,
    status: String,
    tint: Color,
    os: String,
    device: String,
    badge: String,
    away: String,
    info: String,
    typing: bool,
    authorized: bool,
}

fn header_for(app: &App, key: &str) -> Header {
    let dark = app.dark();
    let acc = rgb(palette(&app.settings.theme).accent);
    let empty = |name: &str, status: &str, device: &str, info: &str| Header {
        name: name.into(),
        fp: String::new(),
        status: status.into(),
        tint: acc,
        os: String::new(),
        device: device.into(),
        badge: String::new(),
        away: String::new(),
        info: info.into(),
        typing: false,
        authorized: true,
    };
    match mode_of(key) {
        "notes" => empty("note to self", "synced to your devices", "notes", ""),
        "group" => match app.groups.iter().find(|g| format!("g:{}", g.id) == key) {
            Some(g) => empty(&g.name, &format!("{} members", g.members.len()), "group", "megolm"),
            None => empty("group", "", "group", ""),
        },
        _ => match app.contact(key) {
            Some(c) => Header {
                name: c.name.clone(),
                fp: c.fingerprint.clone(),
                status: if c.awaiting { "awaiting authorization".into() } else { c.status.label().into() },
                tint: tint(c.status, dark),
                os: c.os.clone(),
                device: c.device_class.as_str().into(),
                badge: c.status.badge().into(),
                away: c.away_msg.clone(),
                info: [
                    if c.verified { "verified" } else { "" },
                    if c.device_class == DeviceClass::Mobile && c.background { "may reply late" } else { "" },
                    if c.on_battery { "on battery" } else { "" },
                ]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" · "),
                typing: c.typing,
                authorized: !c.awaiting,
            },
            None => {
                let mut h = empty("?", "", "", "");
                h.authorized = false;
                h
            }
        },
    }
}

fn apply_header_window(w: &ChatWindow, h: &Header) {
    w.set_name(h.name.clone().into());
    w.set_fp(h.fp.clone().into());
    w.set_status(h.status.clone().into());
    w.set_tint(h.tint);
    w.set_os(h.os.clone().into());
    w.set_device(h.device.clone().into());
    w.set_badge(h.badge.clone().into());
    w.set_away(h.away.clone().into());
    w.set_info(h.info.clone().into());
    w.set_typing(h.typing);
    w.set_authorized(h.authorized);
}

fn apply_header_docked(m: &MainWindow, h: &Header) {
    m.set_dchat_name(h.name.clone().into());
    m.set_dchat_status(h.status.clone().into());
    m.set_dchat_tint(h.tint);
    m.set_dchat_os(h.os.clone().into());
    m.set_dchat_device(h.device.clone().into());
    m.set_dchat_badge(h.badge.clone().into());
    m.set_dchat_away(h.away.clone().into());
    m.set_dchat_info(h.info.clone().into());
    m.set_dchat_typing(h.typing);
    m.set_dchat_authorized(h.authorized);
}

fn refresh_chat(app: &AppRc, key: &str) {
    let a = app.borrow();
    let lines: Vec<ChatLine> = a.histories.get(key).map(|v| v.iter().map(|l| chat_line(&a, l)).collect()).unwrap_or_default();
    let h = header_for(&a, key);
    if let Some(w) = a.chats.get(key) {
        apply_header_window(w, &h);
        w.set_lines(ModelRc::new(VecModel::from(lines.clone())));
    }
    if a.docked_chat.as_deref() == Some(key) {
        if let Some(m) = a.main.upgrade() {
            apply_header_docked(&m, &h);
            m.set_dchat_lines(ModelRc::new(VecModel::from(lines)));
        }
    }
}

fn refresh_all_chats(app: &AppRc) {
    let keys: Vec<String> = {
        let a = app.borrow();
        a.chats.keys().cloned().chain(a.docked_chat.clone()).collect()
    };
    for k in keys {
        refresh_chat(app, &k);
    }
}

fn request_history(app: &AppRc, key: &str) {
    let a = app.borrow();
    match mode_of(key) {
        "notes" => a.send(Command::OpenNotes),
        "group" => a.send(Command::OpenGroup { group: key[2..].to_string() }),
        _ => a.send(Command::OpenChat { id: key.to_string() }),
    }
}

fn close_history(app: &AppRc, key: &str) {
    let a = app.borrow();
    match mode_of(key) {
        "group" => a.send(Command::CloseGroup { group: key[2..].to_string() }),
        "contact" => a.send(Command::CloseChat { id: key.to_string() }),
        _ => {}
    }
}

fn chat_send(app: &AppRc, key: &str, text: &str, urgent: bool, reply: &str) {
    let a = app.borrow();
    match mode_of(key) {
        "notes" => a.send(Command::SendNote(text.to_string())),
        "group" => a.send(Command::GroupSend { group: key[2..].to_string(), text: text.to_string() }),
        _ => a.send(Command::SendText { id: key.to_string(), body: text.to_string(), reply_to: reply.parse().ok(), urgent }),
    }
}

fn chat_typed(app: &AppRc, key: &str) {
    if mode_of(key) != "contact" {
        return;
    }
    let mut a = app.borrow_mut();
    let due = a.last_typing.get(key).map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(true);
    if due {
        a.last_typing.insert(key.to_string(), Instant::now());
        a.send(Command::Typing { id: key.to_string(), typing: true });
    }
}

fn copy_to_clipboard(app: &AppRc, text: &str) {
    let ok = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())).is_ok();
    notice(app, if ok { "Copied to clipboard." } else { "Clipboard unavailable." });
}

fn chat_line_action(app: &AppRc, key: &str, action: &str, arg: &str) {
    match action {
        "copy" => copy_to_clipboard(app, arg),
        "delete" if mode_of(key) == "contact" => {
            if let Ok(id) = arg.parse() {
                app.borrow().send(Command::DeleteText { id: key.to_string(), msg: id });
            }
        }
        _ => {}
    }
}

fn chat_edit(app: &AppRc, key: &str, id: &str, text: &str) {
    if let Ok(msg) = id.parse() {
        app.borrow().send(Command::EditText { id: key.to_string(), msg, body: text.to_string() });
    }
}

fn chat_file_send(app: &AppRc, key: &str) {
    if mode_of(key) == "group" {
        return;
    }
    let Some(path) = rfd::FileDialog::new().set_title("Send a file (direct connection only)").pick_file() else { return };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let target = if key == "notes" { "self".to_string() } else { key.to_string() };
    // PRD PR-9: ask before big files to phones or laptops on battery.
    let (warn, name) = {
        let a = app.borrow();
        let warn = a.contact(key).map(|c| size > 25 * 1024 * 1024 && (c.device_class == DeviceClass::Mobile || c.on_battery)).unwrap_or(false);
        (warn, header_for(&a, key).name)
    };
    if warn {
        let r = rfd::MessageDialog::new()
            .set_title("Large file")
            .set_description(format!("{name} is on a phone or on battery. Send {} anyway?", human_size(size)))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        if r != rfd::MessageDialogResult::Yes {
            return;
        }
    }
    app.borrow().send(Command::SendFile { id: target, path: path.to_string_lossy().to_string() });
}

fn open_folder_of(path: &str) {
    let p = Path::new(path);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(p)).spawn();
}

fn chat_file_action(app: &AppRc, action: &str, file: &str) {
    let a = app.borrow();
    let file = file.to_string();
    match action {
        "accept" => a.send(Command::AcceptFile { file, dir: String::new() }),
        "decline" => a.send(Command::DeclineFile { file }),
        "cancel" => a.send(Command::CancelFile { file }),
        "pause" => a.send(Command::PauseFile { file }),
        "resume" => a.send(Command::ResumeFile { file }),
        "open" => {
            if let Some(f) = a.files.get(&file) {
                open_folder_of(&f.path);
            }
        }
        _ => {}
    }
}

fn move_window(w: &slint::Window, dx: f32, dy: f32) {
    let s = w.scale_factor();
    let p = w.position();
    w.set_position(slint::PhysicalPosition::new(p.x + (dx * s) as i32, p.y + (dy * s) as i32));
}

fn open_chat(app: &AppRc, key: &str) {
    if app.borrow().engine.is_none() {
        return;
    }
    if app.borrow().docked_chat.as_deref() == Some(key) {
        request_history(app, key);
        return;
    }
    let exists = app.borrow().chats.contains_key(key);
    if !exists {
        let w = ChatWindow::new().expect("chat window");
        apply_theme(&w.global::<Theme>(), palette(&app.borrow().settings.theme));
        w.set_mode(mode_of(key).into());
        let (ap, k) = (app.clone(), key.to_string());
        w.on_send(move |t, u, r| chat_send(&ap, &k, &t, u, &r));
        let (ap, k) = (app.clone(), key.to_string());
        w.on_typed(move || chat_typed(&ap, &k));
        let (ap, k) = (app.clone(), key.to_string());
        w.on_line_action(move |act, arg| chat_line_action(&ap, &k, &act, &arg));
        let (ap, k) = (app.clone(), key.to_string());
        w.on_edit_commit(move |id, t| chat_edit(&ap, &k, &id, &t));
        let (ap, k) = (app.clone(), key.to_string());
        w.on_file_send(move || chat_file_send(&ap, &k));
        let ap = app.clone();
        w.on_file_action(move |act, f| chat_file_action(&ap, &act, &f));
        let weak = w.as_weak();
        w.on_drag(move |dx, dy| {
            if let Some(w) = weak.upgrade() {
                if !w.window().is_maximized() {
                    move_window(w.window(), dx, dy);
                }
            }
        });
        let weak = w.as_weak();
        w.on_resize(move |dx, dy| {
            if let Some(w) = weak.upgrade() {
                let win = w.window();
                let sc = win.scale_factor();
                let sz = win.size();
                let nw = (sz.width as f32 + dx * sc).max(240.0 * sc) as u32;
                let nh = (sz.height as f32 + dy * sc).max(180.0 * sc) as u32;
                win.set_size(slint::PhysicalSize::new(nw, nh));
            }
        });
        let weak = w.as_weak();
        w.on_minimize(move || {
            if let Some(w) = weak.upgrade() {
                w.window().set_minimized(true);
            }
        });
        let weak = w.as_weak();
        w.on_toggle_maximize(move || {
            if let Some(w) = weak.upgrade() {
                let m = !w.window().is_maximized();
                w.window().set_maximized(m);
                w.set_is_max(m);
            }
        });
        let (ap, k) = (app.clone(), key.to_string());
        w.on_dock(move || dock_chat(&ap, &k));
        let (ap, k, weak) = (app.clone(), key.to_string(), w.as_weak());
        w.on_close_chat(move || {
            close_history(&ap, &k);
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
        let (ap, k) = (app.clone(), key.to_string());
        w.window().on_close_requested(move || {
            close_history(&ap, &k);
            CloseRequestResponse::HideWindow
        });
        app.borrow_mut().chats.insert(key.to_string(), w);
    }
    {
        let a = app.borrow();
        let _ = a.chats[key].show();
    }
    refresh_chat(app, key);
    request_history(app, key);
}

/// Move a chat into the lower half of the bar (only one at a time).
fn dock_chat(app: &AppRc, key: &str) {
    let prev = app.borrow().docked_chat.clone();
    if let Some(p) = &prev {
        if p != key {
            close_history(app, p);
        }
    }
    {
        let mut a = app.borrow_mut();
        if let Some(w) = a.chats.get(key) {
            let _ = w.hide();
        }
        a.docked_chat = Some(key.to_string());
        if let Some(m) = a.main.upgrade() {
            m.set_dchat_id(key.into());
            m.set_dchat_mode(mode_of(key).into());
            m.set_chat_docked(true);
        }
    }
    refresh_chat(app, key);
    request_history(app, key);
}

// ================================================================ main window content

fn notice(app: &AppRc, s: &str) {
    let mut a = app.borrow_mut();
    a.notice_until = Some(Instant::now() + Duration::from_secs(8));
    if let Some(m) = a.main.upgrade() {
        m.set_notice(s.into());
    }
}

fn plain_row(kind: &str, id: &str, name: &str, device: &str, tint: Color) -> ContactRow {
    ContactRow {
        kind: kind.into(),
        id: id.into(),
        name: name.into(),
        status: "".into(),
        tint,
        badge: "".into(),
        os: "".into(),
        device: device.into(),
        battery: false,
        unread: 0,
        awaiting: false,
        away: "".into(),
        verified: false,
        typing: false,
        bot: false,
        collapsed: false,
        count: 0,
    }
}

fn rebuild_contacts(app: &AppRc) {
    let a = app.borrow();
    let Some(m) = a.main.upgrade() else { return };
    let dark = a.dark();
    let acc = rgb(palette(&a.settings.theme).accent);
    let collapsed = |name: &str| a.settings.collapsed_folders.iter().any(|f| f == name);
    let row = |c: &ContactView| ContactRow {
        kind: "contact".into(),
        id: c.id.clone().into(),
        name: c.name.clone().into(),
        status: c.status.label().into(),
        tint: tint(c.status, dark),
        badge: c.status.badge().into(),
        os: c.os.clone().into(),
        device: c.device_class.as_str().into(),
        battery: c.on_battery,
        unread: c.unread as i32,
        awaiting: c.awaiting,
        away: c.away_msg.clone().into(),
        verified: c.verified,
        typing: c.typing,
        bot: c.bot,
        collapsed: false,
        count: 0,
    };
    let header = |name: &str, count: usize| {
        let mut r = plain_row("folder", name, name, "", acc);
        r.collapsed = collapsed(name);
        r.count = count as i32;
        r
    };
    let mut rows = vec![plain_row("notes", "notes", "note to self", "notes", acc)];
    if !a.groups.is_empty() {
        rows.push(header("groups", a.groups.len()));
        if !collapsed("groups") {
            for g in &a.groups {
                let mut r = plain_row("group", &g.id, &g.name, "group", acc);
                r.status = format!("{} members", g.members.len()).into();
                r.unread = g.unread as i32;
                rows.push(r);
            }
        }
    }
    let visible = |c: &&ContactView| !a.settings.hide_offline || c.status != Status::Offline || c.awaiting || c.unread > 0;
    let mut folders: Vec<String> = a.contacts.iter().map(|c| c.folder.clone()).filter(|f| !f.is_empty()).collect();
    folders.sort();
    folders.dedup();
    let needs_headers = !folders.is_empty() || !a.groups.is_empty();
    let unfiled: Vec<&ContactView> = a.contacts.iter().filter(|c| c.folder.is_empty()).filter(visible).collect();
    if needs_headers {
        rows.push(header("contacts", unfiled.len()));
    }
    if !(needs_headers && collapsed("contacts")) {
        rows.extend(unfiled.into_iter().map(row));
    }
    for f in folders {
        let list: Vec<&ContactView> = a.contacts.iter().filter(|c| c.folder == f).filter(visible).collect();
        rows.push(header(&f, list.len()));
        if !collapsed(&f) {
            rows.extend(list.into_iter().map(row));
        }
    }
    m.set_contacts(ModelRc::new(VecModel::from(rows)));
}

fn pick_rows(app: &AppRc, exclude: &[String]) {
    let a = app.borrow();
    let Some(m) = a.main.upgrade() else { return };
    let rows: Vec<PickRow> = a
        .contacts
        .iter()
        .filter(|c| !c.awaiting && !exclude.contains(&c.id))
        .map(|c| PickRow { id: c.id.clone().into(), name: c.name.clone().into(), checked: a.pick.contains(&c.id) })
        .collect();
    m.set_pick_rows(ModelRc::new(VecModel::from(rows)));
}

fn show_details(app: &AppRc, id: &str) {
    let (m, c) = {
        let a = app.borrow();
        let Some(m) = a.main.upgrade() else { return };
        let Some(c) = a.contact(id).cloned() else { return };
        (m, c)
    };
    app.borrow_mut().detail_id = id.to_string();
    m.set_d_id(id.into());
    m.set_d_name(c.name.clone().into());
    m.set_d_fp(c.fingerprint.clone().into());
    m.set_d_safety("".into());
    m.set_d_verified(c.verified);
    m.set_d_visibility(match c.visibility {
        Visibility::Normal => 0,
        Visibility::Visible => 1,
        Visibility::Invisible => 2,
    });
    m.set_d_ignored(c.ignored);
    m.set_d_folder(c.folder.clone().into());
    m.set_d_urgent(true);
    m.set_d_disappearing(0);
    m.set_d_introduced(c.introduced_by.clone().into());
    let p = &c.profile;
    let prof = [&p.about, &p.location, &p.homepage, &p.interests].iter().filter(|s| !s.is_empty()).map(|s| s.as_str()).collect::<Vec<_>>().join(" · ");
    m.set_d_profile(prof.into());
    m.set_d_devices(c.devices.iter().map(|d| format!("{} ({}, {})", d.name, d.os, d.status.label())).collect::<Vec<_>>().join(", ").into());
    pick_rows(app, &[id.to_string()]);
    m.set_panel(6);
}

fn show_group(app: &AppRc, gid: &str) {
    let (m, g) = {
        let a = app.borrow();
        let Some(m) = a.main.upgrade() else { return };
        let Some(g) = a.groups.iter().find(|g| g.id == gid).cloned() else { return };
        (m, g)
    };
    app.borrow_mut().group_id = gid.to_string();
    m.set_g_id(gid.into());
    m.set_g_name(g.name.clone().into());
    m.set_g_admin(g.admin);
    let members: Vec<MemberRow> = g.members.iter().map(|(acc, n, admin)| MemberRow { account: acc.clone().into(), name: n.clone().into(), admin: *admin }).collect();
    m.set_g_members(ModelRc::new(VecModel::from(members)));
    app.borrow_mut().pick.clear();
    let exclude: Vec<String> = g.members.iter().map(|(a, _, _)| a.clone()).collect();
    pick_rows(app, &exclude);
    m.set_panel(7);
}

fn set_my_status(app: &AppRc, s: Status) {
    let a = app.borrow();
    if let Some(m) = a.main.upgrade() {
        m.set_my_status(s.label().into());
        m.set_my_tint(tint(s, a.dark()));
        m.set_my_badge(s.badge().into());
    }
}

// ================================================================ docking

fn position_index(all: &[dock::Monitor], id: &str, left: bool) -> usize {
    let m = all.iter().position(|m| m.id == id).unwrap_or(0);
    m * 2 + if left { 0 } else { 1 }
}

/// Dock on the preferred monitor (or the primary) at the chosen edge.
/// Applied twice: moving to a monitor with a different DPI makes the window
/// system rescale it, so the second pass pins the final size.
fn redock(app: &AppRc) {
    let (m, left, pref, collapsed, autohide) = {
        let a = app.borrow();
        let Some(m) = a.main.upgrade() else { return };
        (m, a.settings.dock_left, a.settings.monitor.clone(), a.collapsed, a.settings.autohide)
    };
    let all = dock::monitors(m.window());
    let Some(mon) = dock::pick(m.window(), &pref) else { return };
    let pos = position_index(&all, &mon.id, left);
    m.set_can_move_left(pos > 0);
    m.set_can_move_right(pos + 1 < all.len() * 2);
    app.borrow_mut().layout = all;
    let (w, reserve) = if collapsed { (STRIP_WIDTH, false) } else { (DOCK_WIDTH, !autohide) };
    dock::dock(m.window(), w, left, &mon, reserve);
    let weak = m.as_weak();
    slint::Timer::single_shot(Duration::from_millis(250), move || {
        if let Some(m) = weak.upgrade() {
            dock::dock(m.window(), w, left, &mon, reserve);
        }
    });
}

fn set_collapsed(app: &AppRc, c: bool) {
    {
        let mut a = app.borrow_mut();
        if a.collapsed == c {
            return;
        }
        a.collapsed = c;
        a.hover_left_at = None;
        if let Some(m) = a.main.upgrade() {
            m.set_collapsed(c);
        }
    }
    redock(app);
}

// ================================================================ engine

fn spawn_engine(app: &AppRc, cfg: EngineConfig) {
    let (h, rx) = spawn(cfg);
    let mut a = app.borrow_mut();
    a.engine = Some(h);
    a.rx = Some(rx);
}

fn base_cfg(app: &AppRc, pass: &str) -> EngineConfig {
    let a = app.borrow();
    EngineConfig { dir: a.dir.clone(), passphrase: pass.to_string(), port: a.port, ..Default::default() }
}

fn lock(app: &AppRc, reason: &str) {
    let m = {
        let mut a = app.borrow_mut();
        a.send(Command::Shutdown);
        for w in a.chats.values() {
            let _ = w.hide();
        }
        a.chats.clear();
        a.histories.clear();
        a.docked_chat = None;
        a.auto_prev = None;
        a.main.upgrade()
    };
    if let Some(m) = m {
        m.set_chat_docked(false);
        m.set_logged_in(false);
        m.set_new_account(!Store::exists(&app.borrow().dir));
        m.set_login_stage("".into());
        m.set_login_error(reason.into());
        m.set_panel(0);
    }
}

fn play(app: &AppRc, s: desktop::Sound) {
    let on = app.borrow().settings.sounds;
    if on {
        app.borrow_mut().audio.play(s);
    }
}

fn handle_event(app: &AppRc, ev: Event) {
    let Some(m) = app.borrow().main.upgrade() else { return };
    match ev {
        Event::RecoveryKey(words) => {
            if m.get_logged_in() {
                copy_to_clipboard(app, &words);
                let _ = rfd::MessageDialog::new()
                    .set_title("New recovery key")
                    .set_description(format!("Write these 24 words down (also copied to the clipboard):\n\n{words}"))
                    .show();
            } else {
                m.set_recovery_words(words.clone().into());
                m.set_login_stage("recovery".into());
                m.set_busy(false);
                app.borrow_mut().pending_recovery = Some((words, None));
            }
        }
        ev @ Event::Unlocked { .. } => {
            let waiting = app.borrow().pending_recovery.is_some();
            if waiting {
                if let Some(p) = app.borrow_mut().pending_recovery.as_mut() {
                    p.1 = Some(ev);
                }
                return;
            }
            on_unlocked(app, ev);
        }
        Event::LoginFailed(msg) => {
            let mut a = app.borrow_mut();
            a.engine = None;
            a.rx = None;
            m.set_new_account(!Store::exists(&a.dir));
            m.set_busy(false);
            m.set_login_stage("".into());
            m.set_login_error(msg.into());
        }
        Event::LinkCode { code, words } => {
            m.set_busy(false);
            m.set_link_code(code.into());
            m.set_link_words(words.into());
            m.set_login_stage("link".into());
        }
        Event::Linked => notice(app, "This device is now linked to your account."),
        Event::Contacts(list) => {
            app.borrow_mut().contacts = list;
            rebuild_contacts(app);
            refresh_all_chats(app);
        }
        Event::Pending(list) => {
            let rows: Vec<PendingRow> = list
                .iter()
                .map(|p| PendingRow { id: p.id.clone().into(), nick: p.nick.clone().into(), fp: p.fingerprint.clone().into(), text: p.text.clone().into(), introduced_by: p.introduced_by.clone().into(), warning: p.warning.clone().into() })
                .collect();
            m.set_pending(ModelRc::new(VecModel::from(rows)));
        }
        Event::Introductions(list) => {
            let rows: Vec<IntroRow> = list.into_iter().map(|(i, n, f, v)| IntroRow { index: i as i32, name: n.into(), from: f.into(), verified: v }).collect();
            m.set_intros(ModelRc::new(VecModel::from(rows)));
        }
        Event::Invites(list) => {
            let off = app.borrow().offset;
            let rows: Vec<InviteRow> = list
                .iter()
                .map(|i| InviteRow {
                    token: i.token.clone().into(),
                    label: i.label.clone().into(),
                    info: format!(
                        "{}{}",
                        i.uses_left.map(|u| format!("{u} use(s) left")).unwrap_or_else(|| "unlimited".into()),
                        i.expires.map(|e| format!(", until {}", fmt_date(e, off))).unwrap_or_default()
                    )
                    .into(),
                    code: i.code.clone().into(),
                })
                .collect();
            m.set_invites(ModelRc::new(VecModel::from(rows)));
        }
        Event::Folders(f) => {
            let v: Vec<SharedString> = f.into_iter().map(Into::into).collect();
            m.set_folders(ModelRc::new(VecModel::from(v)));
        }
        Event::History { id, lines, .. } => {
            app.borrow_mut().histories.insert(id.clone(), lines);
            refresh_chat(app, &id);
        }
        Event::Notes(lines) => {
            app.borrow_mut().histories.insert("notes".into(), lines);
            refresh_chat(app, "notes");
        }
        Event::GroupHistory { group, lines, .. } => {
            let key = format!("g:{group}");
            app.borrow_mut().histories.insert(key.clone(), lines);
            refresh_chat(app, &key);
        }
        Event::Groups(list) => {
            app.borrow_mut().groups = list;
            rebuild_contacts(app);
            refresh_all_chats(app);
        }
        Event::Devices(list) => {
            let dark = app.borrow().dark();
            let rows: Vec<DeviceRow> = list
                .iter()
                .map(|d| DeviceRow {
                    peer: d.peer_id.clone().into(),
                    name: d.name.clone().into(),
                    os: d.os.clone().into(),
                    device: d.class.as_str().into(),
                    status: d.status.label().into(),
                    tint: tint(d.status, dark),
                    manager: d.manager,
                    this: d.this_device,
                })
                .collect();
            m.set_is_manager(list.iter().any(|d| d.this_device && d.manager));
            m.set_devices(ModelRc::new(VecModel::from(rows)));
        }
        Event::Invite(s) => {
            m.set_invite(s.into());
            m.set_panel(2);
        }
        Event::Notice(s) => notice(app, &s),
        Event::Status(s) => {
            app.borrow_mut().my_status = s;
            set_my_status(app, s);
        }
        Event::SafetyNumber { id, number } => {
            if app.borrow().detail_id == id {
                m.set_d_safety(number.into());
            }
        }
        Event::SearchResults(list) => {
            let off = app.borrow().offset;
            let rows: Vec<SearchRow> = list.iter().map(|(id, name, l)| SearchRow { id: id.clone().into(), name: name.clone().into(), time: fmt_date(l.ts, off).into(), text: l.text.clone().into() }).collect();
            m.set_results(ModelRc::new(VecModel::from(rows)));
        }
        Event::Incoming { id, name, urgent, .. } => {
            let (busy, open, notify, popups) = {
                let a = app.borrow();
                let open = a.docked_chat.as_deref() == Some(id.as_str()) || a.chats.get(&id).map(|w| w.window().is_visible()).unwrap_or(false);
                (a.my_status.is_busy(), open, a.settings.notify, a.settings.popups)
            };
            app.borrow_mut().last_incoming = Some(id.clone());
            if !busy || urgent {
                play(app, if urgent { desktop::Sound::Urgent } else { desktop::Sound::Message });
                if notify && !open {
                    desktop::notify(&format!("{}{name}", if urgent { "URGENT · " } else { "" }), "New message");
                }
                if popups && !open {
                    open_chat(app, &id);
                }
            }
        }
        Event::GroupIncoming { group, name, from, .. } => {
            let (busy, notify) = {
                let a = app.borrow();
                (a.my_status.is_busy(), a.settings.notify)
            };
            app.borrow_mut().last_incoming = Some(format!("g:{group}"));
            if !busy {
                play(app, desktop::Sound::Message);
                if notify {
                    desktop::notify(&name, &format!("New message from {from}"));
                }
            }
        }
        Event::ContactOnline { .. } => {
            if !app.borrow().my_status.is_busy() {
                play(app, desktop::Sound::Online);
            }
        }
        Event::AuthRequested { name } => {
            play(app, desktop::Sound::Auth);
            if app.borrow().settings.notify {
                desktop::notify("Authorization request", &format!("{name} wants to add you"));
            }
        }
        Event::FileOffered { from, name, size, .. } => {
            play(app, desktop::Sound::File);
            if app.borrow().settings.notify {
                desktop::notify(&format!("File from {from}"), &format!("{name} ({})", human_size(size)));
            }
        }
        Event::Files(list) => {
            {
                let mut a = app.borrow_mut();
                for (_, f) in list {
                    a.files.insert(f.id.clone(), f);
                }
            }
            refresh_all_chats(app);
        }
        Event::Net(n) => {
            let off = app.borrow().offset;
            let lines = |v: &[String]| v.iter().map(|a| format!("  {a}")).collect::<Vec<_>>().join("\n");
            let relays = n.relays.iter().map(|(r, ok)| format!("  {} {r}", if *ok { "up  " } else { "down" })).collect::<Vec<_>>().join("\n");
            let info = format!(
                "peer {}\nNAT: {}\nconnected peers: {}\nlistening:\n{}\nexternal:\n{}\nnostr relays:\n{}\nLAN only: {} · helper: {} · holding {} message(s)\nlast mailbox check: {}",
                n.peer_id,
                n.nat,
                n.connected_peers,
                lines(&n.listen),
                lines(&n.external),
                relays,
                n.lan_only,
                n.helper,
                n.held,
                if n.mailbox_last_fetch == 0 { "never".to_string() } else { fmt_date(n.mailbox_last_fetch, off) }
            );
            m.set_net_info(info.into());
        }
        Event::Profile(p) => {
            m.set_s_about(p.about.into());
            m.set_s_location(p.location.into());
            m.set_s_homepage(p.homepage.into());
            m.set_s_interests(p.interests.into());
        }
        Event::Settings { auto_reply, read_receipts, relays, bootstrap, lan_only, helper, backup_dir, backup_hours, backup_keep } => {
            m.set_s_auto_reply(auto_reply);
            m.set_s_read_receipts(read_receipts);
            m.set_s_relays(relays.join("\n").into());
            m.set_s_bootstrap(bootstrap.join("\n").into());
            m.set_s_lan_only(lan_only);
            m.set_s_helper(helper);
            m.set_s_backup_dir(backup_dir.into());
            m.set_s_backup_hours(backup_hours.to_string().into());
            m.set_s_backup_keep(backup_keep.to_string().into());
        }
        Event::Locked => lock(app, "Locked from another device. Unlock to continue."),
        Event::Wiped => lock(app, "This device was wiped. Its account data is gone from this computer."),
        Event::Stopped => {
            let mut a = app.borrow_mut();
            a.engine = None;
            a.rx = None;
            if a.quitting {
                let _ = slint::quit_event_loop();
            }
        }
    }
}

fn on_unlocked(app: &AppRc, ev: Event) {
    let Event::Unlocked { nick, fingerprint, os, device_class, status, away_msg } = ev else { return };
    let Some(m) = app.borrow().main.upgrade() else { return };
    {
        let mut a = app.borrow_mut();
        a.my_nick = nick.clone();
        a.my_status = status;
    }
    m.set_busy(false);
    m.set_login_error("".into());
    m.set_login_stage("".into());
    m.set_logged_in(true);
    m.set_my_nick(nick.into());
    m.set_my_fp(fingerprint.into());
    m.set_my_os(os.into());
    m.set_my_device(device_class.as_str().into());
    m.set_away_msg(away_msg.into());
    set_my_status(app, status);
    app.borrow().send(Command::NetInfo);
}

// ================================================================ main

fn parse_args() -> (String, u16) {
    let mut profile = "default".to_string();
    let mut port = 0u16;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--profile" => profile = it.next().unwrap_or(profile),
            "--port" => port = it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    (profile, port)
}

fn profile_dir(profile: &str) -> PathBuf {
    directories::ProjectDirs::from("net", "RogueIM", "RogueIM")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".rogueim"))
        .join(profile)
}

fn shutdown(app: &AppRc) {
    let m = app.borrow().main.upgrade();
    if let Some(m) = &m {
        dock::undock(m.window());
        let _ = m.hide();
    }
    let mut a = app.borrow_mut();
    for w in a.chats.values() {
        let _ = w.hide();
    }
    match a.engine.clone() {
        Some(e) => {
            a.quitting = true;
            e.send(Command::Shutdown);
            slint::Timer::single_shot(Duration::from_secs(3), || {
                let _ = slint::quit_event_loop();
            });
        }
        None => {
            let _ = slint::quit_event_loop();
        }
    }
}

fn main() {
    // Software rendering everywhere: crisp at small sizes, low memory, and no
    // GPU-driver surprises (the OpenGL path rendered nothing on some systems).
    // On Linux prefer X11 (incl. XWayland) so the bar can reserve screen space.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        let backend = if cfg!(target_os = "linux") && std::env::var_os("DISPLAY").is_some() { "winit-x11" } else { "winit" };
        let _ = slint::BackendSelector::new().backend_name(backend.into()).renderer_name("software".into()).select();
    }
    let (profile, port) = parse_args();
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let dir = profile_dir(&profile);
    let settings = load_settings(&dir);

    let main = MainWindow::new().expect("main window");
    apply_theme(&main.global::<Theme>(), palette(&settings.theme));
    main.set_theme_name(settings.theme.clone().into());
    let names: Vec<SharedString> = THEMES.iter().map(|p| p.name.into()).collect();
    main.set_theme_names(ModelRc::new(VecModel::from(names)));
    main.set_split(settings.split);
    main.set_docked(true);
    main.set_profile(profile.clone().into());
    main.set_new_account(!Store::exists(&dir));
    main.set_hotkeys_info(desktop::HOTKEYS_INFO.into());
    main.set_s_hide_offline(settings.hide_offline);
    main.set_s_sounds(settings.sounds);
    main.set_s_notify(settings.notify);
    main.set_s_popups(settings.popups);
    main.set_s_autostart(settings.autostart);
    main.set_s_autohide(settings.autohide);
    main.set_s_auto_away(settings.auto_away.to_string().into());
    main.set_s_auto_na(settings.auto_na.to_string().into());
    main.set_s_lock_idle(settings.lock_idle.to_string().into());
    let labels: Vec<SharedString> = Status::SELECTABLE.iter().map(|s| s.label().into()).collect();
    main.set_status_labels(ModelRc::new(VecModel::from(labels)));

    let app: AppRc = Rc::new(RefCell::new(App {
        dir,
        profile,
        port,
        engine: None,
        rx: None,
        main: main.as_weak(),
        chats: HashMap::new(),
        histories: HashMap::new(),
        files: HashMap::new(),
        contacts: vec![],
        groups: vec![],
        my_nick: String::new(),
        my_status: Status::Online,
        settings,
        layout: vec![],
        docked_chat: None,
        pick: HashSet::new(),
        detail_id: String::new(),
        group_id: String::new(),
        notice_until: None,
        quitting: false,
        pending_recovery: None,
        auto_prev: None,
        last_incoming: None,
        last_typing: HashMap::new(),
        collapsed: false,
        hover_left_at: None,
        blink: false,
        offset,
        audio: if std::env::var_os("RIM_NO_AUDIO").is_some() { desktop::Audio::none() } else { desktop::Audio::new() },
        tray: if std::env::var_os("RIM_NO_TRAY").is_some() { None } else { desktop::Tray::new() },
        hotkeys: if std::env::var_os("RIM_NO_HOTKEYS").is_some() { desktop::Hotkeys::none() } else { desktop::Hotkeys::new() },
    }));

    wire_login(&app, &main);
    wire_main(&app, &main);
    wire_settings(&app, &main);
    wire_docked_chat(&app, &main);

    // Pump engine events, tray menu and hotkeys on the UI thread.
    let pump = slint::Timer::default();
    {
        let app = app.clone();
        pump.start(slint::TimerMode::Repeated, Duration::from_millis(40), move || {
            let events: Vec<Event> = match &app.borrow().rx {
                Some(rx) => rx.try_iter().collect(),
                None => vec![],
            };
            for ev in events {
                handle_event(&app, ev);
            }
            poll_desktop(&app);
            let expired = app.borrow().notice_until.map(|t| Instant::now() > t).unwrap_or(false);
            if expired {
                app.borrow_mut().notice_until = None;
                if let Some(m) = app.borrow().main.upgrade() {
                    m.set_notice("".into());
                }
            }
        });
    }
    // Once a second: idle (auto-away, lock), auto-hide, tray blink, panel refreshes.
    let slow = slint::Timer::default();
    {
        let app = app.clone();
        let mut ticks = 0u64;
        let mut last_panel = 0;
        slow.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
            ticks += 1;
            if ticks % 2 == 0 {
                idle_tick(&app);
                autohide_tick(&app);
                tray_tick(&app);
            }
            let Some(m) = app.borrow().main.upgrade() else { return };
            let p = m.get_panel();
            if p != last_panel {
                last_panel = p;
                if p == 5 {
                    app.borrow_mut().pick.clear();
                    pick_rows(&app, &[]);
                }
                if p == 3 {
                    app.borrow().send(Command::NetInfo);
                }
            }
            if ticks % 10 == 0 && p == 3 && m.get_settings_tab() == 1 {
                app.borrow().send(Command::NetInfo);
            }
        });
    }
    // Follow monitor changes (plugged in/out, resolution, scaling).
    let watch = slint::Timer::default();
    {
        let app = app.clone();
        watch.start(slint::TimerMode::Repeated, Duration::from_secs(2), move || {
            let changed = {
                let a = app.borrow();
                a.main.upgrade().map(|m| a.layout != dock::monitors(m.window())).unwrap_or(false)
            };
            if changed {
                redock(&app);
            }
        });
    }

    main.show().expect("show");
    {
        let app = app.clone();
        slint::Timer::single_shot(Duration::from_millis(150), move || redock(&app));
    }
    slint::run_event_loop_until_quit().expect("event loop");
}

fn wire_login(app: &AppRc, main: &MainWindow) {
    let ap = app.clone();
    main.on_create(move |nick, p1, p2| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        if ap.borrow().engine.is_some() {
            return;
        }
        if p1 != p2 {
            m.set_login_error("The passphrases differ.".into());
            return;
        }
        m.set_login_error("".into());
        m.set_busy(true);
        let mut cfg = base_cfg(&ap, &p1);
        cfg.nick = Some(nick.to_string());
        spawn_engine(&ap, cfg);
    });
    let ap = app.clone();
    main.on_unlock(move |pass| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        if ap.borrow().engine.is_some() {
            return;
        }
        m.set_login_error("".into());
        m.set_busy(true);
        let cfg = base_cfg(&ap, &pass);
        spawn_engine(&ap, cfg);
    });
    let ap = app.clone();
    main.on_start_link(move |pass| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        if ap.borrow().engine.is_some() {
            return;
        }
        m.set_busy(true);
        let mut cfg = base_cfg(&ap, &pass);
        cfg.mode = StartMode::Link;
        spawn_engine(&ap, cfg);
    });
    let ap = app.clone();
    main.on_browse_backup(move || {
        if let Some(p) = rfd::FileDialog::new().add_filter("RogueIM backup", &["rimbackup"]).pick_file() {
            if let Some(m) = ap.borrow().main.upgrade() {
                m.set_restore_path(p.to_string_lossy().to_string().into());
            }
        }
    });
    let ap = app.clone();
    main.on_restore(move |path, words, secret, newpass| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        if ap.borrow().engine.is_some() {
            return;
        }
        m.set_busy(true);
        let mut cfg = base_cfg(&ap, &newpass);
        let secret = if words { RestoreSecret::Recovery(secret.to_string()) } else { RestoreSecret::Passphrase(secret.to_string()) };
        cfg.mode = StartMode::Restore { path: PathBuf::from(path.to_string()), secret };
        spawn_engine(&ap, cfg);
    });
    let ap = app.clone();
    main.on_recovery_done(move || {
        let pending = ap.borrow_mut().pending_recovery.take();
        if let Some((_, Some(ev))) = pending {
            on_unlocked(&ap, ev);
        }
    });
    let ap = app.clone();
    main.on_copy_text(move |t| copy_to_clipboard(&ap, &t));
}

fn wire_main(app: &AppRc, main: &MainWindow) {
    let ap = app.clone();
    main.on_set_status(move |i| {
        let s = Status::SELECTABLE[i as usize];
        let mut a = ap.borrow_mut();
        a.auto_prev = None;
        a.send(Command::SetStatus(s));
    });
    let ap = app.clone();
    main.on_new_invite(move |kind, label| {
        let (uses, ttl) = match kind {
            0 => (Some(1), None),
            1 => (Some(5), None),
            2 => (None, Some(86_400)),
            3 => (None, Some(7 * 86_400)),
            _ => (None, None),
        };
        ap.borrow().send(Command::NewInvite { uses, ttl_secs: ttl, label: label.to_string() });
    });
    let ap = app.clone();
    main.on_revoke_invite(move |t| ap.borrow().send(Command::RevokeInvite { token: t.to_string() }));
    let ap = app.clone();
    main.on_add_contact(move |inv, text| ap.borrow().send(Command::AddContact { invite: inv.to_string(), text: text.to_string() }));
    let ap = app.clone();
    main.on_accept(move |id| ap.borrow().send(Command::Accept { id: id.to_string() }));
    let ap = app.clone();
    main.on_deny(move |id| ap.borrow().send(Command::Deny { id: id.to_string() }));
    let ap = app.clone();
    main.on_accept_intro(move |i| ap.borrow().send(Command::AcceptIntroduction { index: i as usize, text: "We were introduced — please add me.".into() }));
    let ap = app.clone();
    main.on_dismiss_intro(move |i| ap.borrow().send(Command::DismissIntroduction { index: i as usize }));
    let ap = app.clone();
    main.on_open_chat(move |id| open_chat(&ap, &id));
    let ap = app.clone();
    main.on_open_notes(move || open_chat(&ap, "notes"));
    let ap = app.clone();
    main.on_open_group(move |gid| open_chat(&ap, &format!("g:{gid}")));
    let ap = app.clone();
    main.on_select_contact(move |id| {
        let docked = ap.borrow().docked_chat.clone();
        if docked.is_some() && docked.as_deref() != Some(id.as_str()) {
            dock_chat(&ap, &id);
        }
    });
    let ap = app.clone();
    main.on_toggle_folder(move |f| {
        {
            let mut a = ap.borrow_mut();
            let f = f.to_string();
            if let Some(i) = a.settings.collapsed_folders.iter().position(|x| *x == f) {
                a.settings.collapsed_folders.remove(i);
            } else {
                a.settings.collapsed_folders.push(f);
            }
            save_settings(&a.dir, &a.settings);
        }
        rebuild_contacts(&ap);
    });
    let ap = app.clone();
    main.on_show_details(move |id| show_details(&ap, &id));
    let ap = app.clone();
    main.on_show_group(move |gid| show_group(&ap, &gid));
    let ap = app.clone();
    main.on_remove_contact(move |id| ap.borrow().send(Command::Remove { id: id.to_string() }));
    let ap = app.clone();
    main.on_move_dock(move |dir| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        let all = dock::monitors(m.window());
        if all.is_empty() {
            return;
        }
        {
            let mut a = ap.borrow_mut();
            let cur = dock::pick(m.window(), &a.settings.monitor).map(|m| m.id).unwrap_or_default();
            let pos = position_index(&all, &cur, a.settings.dock_left) as i32;
            let next = (pos + dir).clamp(0, all.len() as i32 * 2 - 1) as usize;
            a.settings.monitor = all[next / 2].id.clone();
            a.settings.dock_left = next % 2 == 0;
            save_settings(&a.dir, &a.settings);
        }
        redock(&ap);
    });
    let ap = app.clone();
    main.on_quit(move || shutdown(&ap));
    let ap = app.clone();
    main.window().on_close_requested(move || {
        shutdown(&ap);
        CloseRequestResponse::HideWindow
    });
    let ap = app.clone();
    main.on_search(move |q| ap.borrow().send(Command::Search(q.to_string())));
    let ap = app.clone();
    main.on_pick_toggle(move |id| {
        {
            let mut a = ap.borrow_mut();
            let id = id.to_string();
            if !a.pick.remove(&id) {
                a.pick.insert(id);
            }
        }
        pick_rows(&ap, &[]);
    });
    let ap = app.clone();
    main.on_create_group(move |name| {
        let members: Vec<String> = ap.borrow_mut().pick.drain().collect();
        ap.borrow().send(Command::CreateGroup { name: name.to_string(), members });
    });
    let ap = app.clone();
    main.on_detail_safety(move || {
        let id = ap.borrow().detail_id.clone();
        ap.borrow().send(Command::SafetyNumber { id });
    });
    let ap = app.clone();
    main.on_detail_introduce(move |to| {
        let whom = ap.borrow().detail_id.clone();
        ap.borrow().send(Command::Introduce { to: to.to_string(), whom });
    });
    let ap = app.clone();
    main.on_detail_save(move || {
        let a = ap.borrow();
        let Some(m) = a.main.upgrade() else { return };
        let id = a.detail_id.clone();
        let Some(c) = a.contact(&id).cloned() else { return };
        let name = m.get_d_name().to_string();
        if !name.trim().is_empty() && name != c.name {
            a.send(Command::Rename { id: id.clone(), name });
        }
        if m.get_d_verified() != c.verified {
            a.send(Command::SetVerified { id: id.clone(), verified: m.get_d_verified() });
        }
        let vis = match m.get_d_visibility() {
            1 => Visibility::Visible,
            2 => Visibility::Invisible,
            _ => Visibility::Normal,
        };
        if vis != c.visibility {
            a.send(Command::SetVisibility { id: id.clone(), visibility: vis });
        }
        if m.get_d_ignored() != c.ignored {
            a.send(Command::SetIgnored { id: id.clone(), ignored: m.get_d_ignored() });
        }
        let folder = m.get_d_folder().trim().to_string();
        if folder != c.folder {
            a.send(Command::SetFolder { id: id.clone(), folder });
        }
        a.send(Command::SetUrgentAllowed { id: id.clone(), allowed: m.get_d_urgent() });
        let ttl = match m.get_d_disappearing() {
            1 => Some(300),
            2 => Some(3600),
            3 => Some(86_400),
            4 => Some(7 * 86_400),
            _ => None,
        };
        a.send(Command::SetDisappearing { id, ttl });
    });
    let ap = app.clone();
    main.on_group_rename(move || {
        let a = ap.borrow();
        if let Some(m) = a.main.upgrade() {
            a.send(Command::GroupRename { group: a.group_id.clone(), name: m.get_g_name().to_string() });
        }
    });
    let ap = app.clone();
    main.on_group_add(move |id| {
        let a = ap.borrow();
        a.send(Command::GroupAdd { group: a.group_id.clone(), id: id.to_string() });
    });
    let ap = app.clone();
    main.on_group_remove(move |acc| {
        let a = ap.borrow();
        a.send(Command::GroupRemove { group: a.group_id.clone(), account: acc.to_string() });
    });
    let ap = app.clone();
    main.on_group_leave(move || {
        let a = ap.borrow();
        a.send(Command::GroupLeave { group: a.group_id.clone() });
    });
    let ap = app.clone();
    main.on_expand(move || set_collapsed(&ap, false));
}

fn wire_settings(app: &AppRc, main: &MainWindow) {
    let ap = app.clone();
    main.on_set_theme(move |name| {
        let next = palette(&name);
        {
            let mut a = ap.borrow_mut();
            a.settings.theme = next.name.to_string();
            save_settings(&a.dir, &a.settings);
            for w in a.chats.values() {
                apply_theme(&w.global::<Theme>(), next);
            }
            if let Some(m) = a.main.upgrade() {
                apply_theme(&m.global::<Theme>(), next);
                m.set_theme_name(next.name.into());
            }
        }
        let s = ap.borrow().my_status;
        set_my_status(&ap, s);
        rebuild_contacts(&ap);
        refresh_all_chats(&ap);
    });
    let ap = app.clone();
    main.on_setting_toggled(move |name| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        let mut redock_needed = false;
        let mut err = None;
        {
            let mut a = ap.borrow_mut();
            match name.as_str() {
                "auto-reply" => a.send(Command::SetAutoReply(m.get_s_auto_reply())),
                "read-receipts" => a.send(Command::SetReadReceipts(m.get_s_read_receipts())),
                "hide-offline" => a.settings.hide_offline = m.get_s_hide_offline(),
                "sounds" => a.settings.sounds = m.get_s_sounds(),
                "notify" => a.settings.notify = m.get_s_notify(),
                "popups" => a.settings.popups = m.get_s_popups(),
                "autostart" => {
                    a.settings.autostart = m.get_s_autostart();
                    if let Err(e) = desktop::set_autostart(&a.profile, a.settings.autostart) {
                        err = Some(e);
                    }
                }
                "autohide" => {
                    a.settings.autohide = m.get_s_autohide();
                    redock_needed = true;
                }
                "lan-only" => a.send(Command::SetLanOnly(m.get_s_lan_only())),
                "helper" => a.send(Command::SetHelper(m.get_s_helper())),
                _ => {}
            }
            save_settings(&a.dir, &a.settings);
        }
        if let Some(e) = err {
            notice(&ap, &format!("Autostart: {e}"));
        }
        rebuild_contacts(&ap);
        if redock_needed {
            redock(&ap);
        }
    });
    let ap = app.clone();
    main.on_save_general(move || {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        {
            let mut a = ap.borrow_mut();
            a.settings.auto_away = m.get_s_auto_away().trim().parse().unwrap_or(0);
            a.settings.auto_na = m.get_s_auto_na().trim().parse().unwrap_or(0);
            a.settings.lock_idle = m.get_s_lock_idle().trim().parse().unwrap_or(0);
            save_settings(&a.dir, &a.settings);
            a.send(Command::SetAwayMessage(m.get_away_msg().to_string()));
        }
        notice(&ap, "Saved.");
    });
    let ap = app.clone();
    main.on_save_network(move || {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        let lines = |s: SharedString| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect::<Vec<_>>();
        ap.borrow().send(Command::SetRelays(lines(m.get_s_relays())));
        ap.borrow().send(Command::SetBootstrap(lines(m.get_s_bootstrap())));
        notice(&ap, "Network settings saved.");
    });
    let ap = app.clone();
    main.on_save_backup(move || {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        ap.borrow().send(Command::SetBackupSchedule {
            dir: m.get_s_backup_dir().to_string(),
            every_hours: m.get_s_backup_hours().trim().parse().unwrap_or(0),
            keep: m.get_s_backup_keep().trim().parse().unwrap_or(5),
        });
        notice(&ap, "Backup schedule saved.");
    });
    let ap = app.clone();
    main.on_save_profile(move || {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        ap.borrow().send(Command::SetProfile(Profile {
            about: m.get_s_about().to_string(),
            location: m.get_s_location().to_string(),
            homepage: m.get_s_homepage().to_string(),
            interests: m.get_s_interests().to_string(),
            updated: 0,
        }));
        notice(&ap, "Profile sent to your contacts.");
    });
    let ap = app.clone();
    main.on_backup_now(move || {
        let nick = ap.borrow().my_nick.clone();
        if let Some(p) = rfd::FileDialog::new().add_filter("RogueIM backup", &["rimbackup"]).set_file_name(format!("rogueim-{nick}.rimbackup")).save_file() {
            ap.borrow().send(Command::ExportBackup { path: p.to_string_lossy().to_string(), include_files: false });
        }
    });
    let ap = app.clone();
    main.on_browse_backup_dir(move || {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            if let Some(m) = ap.borrow().main.upgrade() {
                m.set_s_backup_dir(p.to_string_lossy().to_string().into());
            }
        }
    });
    let ap = app.clone();
    main.on_new_recovery_key(move || ap.borrow().send(Command::ShowRecoveryKey));
    let ap = app.clone();
    main.on_import_history(move || {
        let Some(p) = rfd::FileDialog::new().add_filter("RogueIM backup", &["rimbackup"]).pick_file() else { return };
        // A backup of this account opens with the passphrase it was made with;
        // for the usual case (same passphrase as now) the engine uses the current one.
        ap.borrow().send(Command::ImportHistory { path: p.to_string_lossy().to_string(), secret: RestoreSecret::Passphrase(String::new()) });
    });
    let ap = app.clone();
    main.on_link_changed(move || {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        match identity::decode_link(&m.get_link_input()) {
            Ok(c) => m.set_link_check_words(identity::link_words(&c).into()),
            Err(e) => {
                m.set_link_check_words("".into());
                notice(&ap, &format!("{e:#}"));
            }
        }
    });
    let ap = app.clone();
    main.on_link_device(move |manager, history| {
        let Some(m) = ap.borrow().main.upgrade() else { return };
        ap.borrow().send(Command::LinkDevice { code: m.get_link_input().to_string(), manager, history_days: if history { Some(0) } else { None } });
        m.set_link_input("".into());
        m.set_link_check_words("".into());
    });
    let ap = app.clone();
    main.on_device_remove(move |peer, wipe| ap.borrow().send(Command::RevokeDevice { peer: peer.to_string(), wipe }));
    let ap = app.clone();
    main.on_device_lock(move |peer| ap.borrow().send(Command::RemoteLock { peer: peer.to_string() }));
    let ap = app.clone();
    main.on_wipe_account(move || ap.borrow().send(Command::WipeAccount));
    let ap = app.clone();
    main.on_split_changed(move |v| {
        let mut a = ap.borrow_mut();
        a.settings.split = v;
        save_settings(&a.dir, &a.settings);
    });
}

fn wire_docked_chat(app: &AppRc, main: &MainWindow) {
    fn key(ap: &AppRc) -> Option<String> {
        ap.borrow().docked_chat.clone()
    }
    let ap = app.clone();
    main.on_dchat_send(move |t, u, r| {
        if let Some(k) = key(&ap) {
            chat_send(&ap, &k, &t, u, &r);
        }
    });
    let ap = app.clone();
    main.on_dchat_typed(move || {
        if let Some(k) = key(&ap) {
            chat_typed(&ap, &k);
        }
    });
    let ap = app.clone();
    main.on_dchat_line_action(move |a, i| {
        if let Some(k) = key(&ap) {
            chat_line_action(&ap, &k, &a, &i);
        }
    });
    let ap = app.clone();
    main.on_dchat_edit(move |i, t| {
        if let Some(k) = key(&ap) {
            chat_edit(&ap, &k, &i, &t);
        }
    });
    let ap = app.clone();
    main.on_dchat_file_send(move || {
        if let Some(k) = key(&ap) {
            chat_file_send(&ap, &k);
        }
    });
    let ap = app.clone();
    main.on_dchat_file_action(move |a, f| chat_file_action(&ap, &a, &f));
    let ap = app.clone();
    main.on_dchat_undock(move || {
        let Some(id) = ap.borrow_mut().docked_chat.take() else { return };
        if let Some(m) = ap.borrow().main.upgrade() {
            m.set_chat_docked(false);
        }
        open_chat(&ap, &id);
    });
    let ap = app.clone();
    main.on_dchat_close(move || {
        let id = ap.borrow_mut().docked_chat.take();
        if let Some(m) = ap.borrow().main.upgrade() {
            m.set_chat_docked(false);
        }
        if let Some(id) = id {
            close_history(&ap, &id);
        }
    });
}

// ================================================================ periodic

fn poll_desktop(app: &AppRc) {
    while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
        let (show, quit, status) = {
            let a = app.borrow();
            match &a.tray {
                Some(t) => (ev.id == t.show_id, ev.id == t.quit_id, t.status_ids.iter().find(|(id, _)| *id == ev.id).map(|(_, s)| *s)),
                None => (false, false, None),
            }
        };
        if show {
            let c = !app.borrow().collapsed;
            set_collapsed(app, c);
        } else if quit {
            shutdown(app);
        } else if let Some(s) = status {
            app.borrow().send(Command::SetStatus(s));
        }
    }
    while tray_icon::TrayIconEvent::receiver().try_recv().is_ok() {}
    let keys = app.borrow().hotkeys.poll();
    for k in keys {
        match k {
            desktop::Hotkey::ToggleBar => {
                let c = !app.borrow().collapsed;
                set_collapsed(app, c);
            }
            desktop::Hotkey::ReplyLast => {
                let last = app.borrow().last_incoming.clone();
                if let Some(k) = last {
                    set_collapsed(app, false);
                    open_chat(app, &k);
                }
            }
            desktop::Hotkey::Notes => open_chat(app, "notes"),
            desktop::Hotkey::Away | desktop::Hotkey::Dnd => {
                let target = if k == desktop::Hotkey::Away { Status::Away } else { Status::DoNotDisturb };
                let cur = app.borrow().my_status;
                let next = if cur == target { Status::Online } else { target };
                app.borrow().send(Command::SetStatus(next));
            }
        }
    }
}

/// Auto-away / auto-N/A after idle, restore on activity; lock after idle.
fn idle_tick(app: &AppRc) {
    let Some(idle) = desktop::idle_secs() else { return };
    let (away, na, lock_after, status, prev, active) = {
        let a = app.borrow();
        let active = a.engine.is_some() && a.main.upgrade().map(|m| m.get_logged_in()).unwrap_or(false);
        (a.settings.auto_away as u64 * 60, a.settings.auto_na as u64 * 60, a.settings.lock_idle as u64 * 60, a.my_status, a.auto_prev, active)
    };
    if !active {
        return;
    }
    if lock_after > 0 && idle >= lock_after {
        lock(app, "Locked after inactivity.");
        return;
    }
    if idle < 5 {
        if let Some(p) = prev {
            app.borrow_mut().auto_prev = None;
            app.borrow().send(Command::SetStatus(p));
        }
        return;
    }
    let manual_busy = matches!(status, Status::Occupied | Status::DoNotDisturb | Status::Invisible);
    if manual_busy && prev.is_none() {
        return;
    }
    if na > 0 && idle >= na && status != Status::NotAvailable {
        if prev.is_none() {
            app.borrow_mut().auto_prev = Some(status);
        }
        app.borrow().send(Command::SetStatus(Status::NotAvailable));
    } else if away > 0 && idle >= away && matches!(status, Status::Online | Status::FreeForChat) {
        app.borrow_mut().auto_prev = Some(status);
        app.borrow().send(Command::SetStatus(Status::Away));
    }
}

/// Collapse the auto-hidden bar once the mouse has left it for a moment.
fn autohide_tick(app: &AppRc) {
    let (autohide, collapsed) = {
        let a = app.borrow();
        (a.settings.autohide, a.collapsed)
    };
    if !autohide {
        if collapsed {
            set_collapsed(app, false);
        }
        return;
    }
    if collapsed {
        return;
    }
    let Some(m) = app.borrow().main.upgrade() else { return };
    let Some((cx, cy)) = dock::cursor() else { return };
    let (pos, size) = (m.window().position(), m.window().size());
    let inside = cx >= pos.x && cx < pos.x + size.width as i32 && cy >= pos.y && cy < pos.y + size.height as i32;
    if inside || m.get_panel() != 0 || !m.get_logged_in() {
        app.borrow_mut().hover_left_at = None;
        return;
    }
    let since = app.borrow().hover_left_at;
    match since {
        None => app.borrow_mut().hover_left_at = Some(Instant::now()),
        Some(t) if t.elapsed() > Duration::from_millis(1500) => set_collapsed(app, true),
        _ => {}
    }
}

fn tray_tick(app: &AppRc) {
    let (unread, collapsed) = {
        let mut a = app.borrow_mut();
        let unread: u32 = a.contacts.iter().map(|c| c.unread).sum::<u32>() + a.groups.iter().map(|g| g.unread).sum::<u32>();
        a.blink = unread > 0 && !a.blink;
        let color = tint_rgb(a.my_status, true);
        let tip = if unread > 0 { format!("RogueIM — {unread} unread") } else { format!("RogueIM — {}", a.my_status.label()) };
        let blink = a.blink;
        if let Some(t) = a.tray.as_mut() {
            t.set(color, blink, &tip);
        }
        (unread, a.collapsed)
    };
    // Unread while auto-hidden: pop the bar out.
    if unread > 0 && collapsed {
        set_collapsed(app, false);
    }
}
