#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! RogueIM desktop prototype: a buddy list that is always docked to a screen
//! edge and always on top, plus one window per chat.

mod dock;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rim_core::proto::Delivery;
use rim_core::store::Store;
use rim_core::{spawn, Command, ContactView, EngineConfig, EngineHandle, Event, LineView, Status};
use slint::{CloseRequestResponse, Color, ComponentHandle, ModelRc, SharedString, VecModel};
use time::UtcOffset;

slint::include_modules!();

const DOCK_WIDTH: f32 = 280.0;

// ---------- settings & themes ----------

/// Per-profile UI settings. Not secret, so plain JSON next to the state file.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
struct Settings {
    theme: String,
    dock_left: bool,
    /// Preferred monitor id; falls back to the primary while it is disconnected.
    monitor: String,
    /// Share of the bar given to the contact list when a chat is docked.
    split: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { theme: "graphite".into(), dock_left: false, monitor: String::new(), split: 0.5 }
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

/// Status colour; light themes get darker variants so they stay readable.
fn tint(s: Status, dark: bool) -> Color {
    rgb(match (s, dark) {
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
    })
}

// ---------- app state ----------

struct App {
    dir: PathBuf,
    port: u16,
    engine: Option<EngineHandle>,
    rx: Option<Receiver<Event>>,
    chats: HashMap<String, ChatWindow>,
    contacts: Vec<ContactView>,
    my_nick: String,
    my_status: Status,
    invite: String,
    notice_until: Option<Instant>,
    quitting: bool,
    settings: Settings,
    /// Monitor layout at the last dock, to detect changes.
    layout: Vec<dock::Monitor>,
    /// Chat shown in the lower half of the bar instead of its own window.
    docked_chat: Option<String>,
    main: slint::Weak<MainWindow>,
    offset: UtcOffset,
}

impl App {
    fn dark(&self) -> bool {
        palette(&self.settings.theme).dark
    }
}

fn contact_row(c: &ContactView, dark: bool) -> ContactRow {
    ContactRow {
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
        fp: c.fingerprint.clone().into(),
    }
}

fn fmt_time(ts: i64, off: UtcOffset) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .map(|t| t.to_offset(off))
        .map(|t| format!("{:02}:{:02}", t.hour(), t.minute()))
        .unwrap_or_default()
}

fn chat_line(l: &LineView, me: &str, them: &str, off: UtcOffset) -> ChatLine {
    ChatLine {
        time: fmt_time(l.ts, off).into(),
        who: if l.from_me { me.into() } else { them.into() },
        text: l.text.clone().into(),
        mine: l.from_me,
        state: match l.delivery {
            Delivery::Queued => "queued",
            Delivery::Failed => "failed",
            _ => "",
        }
        .into(),
    }
}

fn update_chat_header(w: &ChatWindow, c: &ContactView, dark: bool) {
    w.set_name(c.name.clone().into());
    w.set_fp(c.fingerprint.clone().into());
    w.set_status(if c.awaiting { "awaiting authorization".into() } else { c.status.label().into() });
    w.set_tint(tint(c.status, dark));
    w.set_badge(c.status.badge().into());
    w.set_os(c.os.clone().into());
    w.set_device(c.device_class.as_str().into());
    w.set_away(c.away_msg.clone().into());
    w.set_authorized(!c.awaiting);
}

fn update_docked_header(main: &MainWindow, c: &ContactView, dark: bool) {
    main.set_dchat_name(c.name.clone().into());
    main.set_dchat_status(if c.awaiting { "awaiting authorization".into() } else { c.status.label().into() });
    main.set_dchat_tint(tint(c.status, dark));
    main.set_dchat_badge(c.status.badge().into());
    main.set_dchat_os(c.os.clone().into());
    main.set_dchat_device(c.device_class.as_str().into());
    main.set_dchat_away(c.away_msg.clone().into());
    main.set_dchat_authorized(!c.awaiting);
}

/// Move a chat into the lower half of the bar (only one at a time).
fn dock_chat(app: &Rc<RefCell<App>>, id: &str) {
    let (main, engine) = {
        let mut a = app.borrow_mut();
        let Some(main) = a.main.upgrade() else { return };
        if let Some(w) = a.chats.get(id) {
            let _ = w.hide();
        }
        // Switching the dock closes the previous docked chat.
        if let (Some(prev), Some(e)) = (a.docked_chat.as_ref(), a.engine.as_ref()) {
            if prev != id {
                e.send(Command::CloseChat { id: prev.clone() });
            }
        }
        a.docked_chat = Some(id.to_string());
        if let Some(c) = a.contacts.iter().find(|c| c.id == id) {
            update_docked_header(&main, c, a.dark());
        }
        (main, a.engine.clone())
    };
    main.set_dchat_lines(ModelRc::new(VecModel::from(Vec::<ChatLine>::new())));
    main.set_dchat_id(id.into());
    main.set_chat_docked(true);
    if let Some(e) = engine {
        e.send(Command::OpenChat { id: id.to_string() });
    }
}

fn move_window(w: &slint::Window, dx: f32, dy: f32) {
    let s = w.scale_factor();
    let p = w.position();
    w.set_position(slint::PhysicalPosition::new(p.x + (dx * s) as i32, p.y + (dy * s) as i32));
}

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

fn open_chat(app: &Rc<RefCell<App>>, id: &str) {
    // Already docked in the bar: nothing to open.
    if app.borrow().docked_chat.as_deref() == Some(id) {
        return;
    }
    let mut a = app.borrow_mut();
    let Some(engine) = a.engine.clone() else { return };
    if !a.chats.contains_key(id) {
        let w = ChatWindow::new().expect("chat window");
        apply_theme(&w.global::<Theme>(), palette(&a.settings.theme));
        let eng = engine.clone();
        let cid = id.to_string();
        w.on_send(move |t| eng.send(Command::SendText { id: cid.clone(), body: t.to_string() }));
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
        let app2 = app.clone();
        let cid = id.to_string();
        w.on_dock(move || dock_chat(&app2, &cid));
        let eng = engine.clone();
        let cid = id.to_string();
        let weak = w.as_weak();
        w.on_close_chat(move || {
            eng.send(Command::CloseChat { id: cid.clone() });
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
        let eng = engine.clone();
        let cid = id.to_string();
        w.window().on_close_requested(move || {
            eng.send(Command::CloseChat { id: cid.clone() });
            CloseRequestResponse::HideWindow
        });
        if let Some(c) = a.contacts.iter().find(|c| c.id == id) {
            update_chat_header(&w, c, a.dark());
        }
        a.chats.insert(id.to_string(), w);
    }
    let _ = a.chats[id].show();
    engine.send(Command::OpenChat { id: id.to_string() });
}

fn refresh_contacts(app: &Rc<RefCell<App>>, main: &MainWindow) {
    let a = app.borrow();
    let dark = a.dark();
    let rows: Vec<ContactRow> = a.contacts.iter().map(|c| contact_row(c, dark)).collect();
    main.set_contacts(ModelRc::new(VecModel::from(rows)));
    for c in &a.contacts {
        if let Some(w) = a.chats.get(&c.id) {
            update_chat_header(w, c, dark);
        }
        if a.docked_chat.as_deref() == Some(c.id.as_str()) {
            update_docked_header(main, c, dark);
        }
    }
}

/// Docking positions run left to right across the desktop: monitor 1 left,
/// monitor 1 right, monitor 2 left, ... (monitors are sorted by x).
fn position_index(all: &[dock::Monitor], id: &str, left: bool) -> usize {
    let m = all.iter().position(|m| m.id == id).unwrap_or(0);
    m * 2 + if left { 0 } else { 1 }
}

/// Dock on the preferred monitor (or the primary) at the chosen edge.
/// Applied twice: moving to a monitor with a different DPI makes the window
/// system rescale it, so the second pass pins the final size.
fn redock(app: &Rc<RefCell<App>>, main: &MainWindow) {
    let (left, pref) = {
        let a = app.borrow();
        (a.settings.dock_left, a.settings.monitor.clone())
    };
    let all = dock::monitors();
    let Some(mon) = dock::pick(&pref) else { return };
    let pos = position_index(&all, &mon.id, left);
    main.set_can_move_left(pos > 0);
    main.set_can_move_right(pos + 1 < all.len() * 2);
    app.borrow_mut().layout = all;
    dock::dock(main.window(), DOCK_WIDTH, left, &mon);
    let weak = main.as_weak();
    slint::Timer::single_shot(Duration::from_millis(250), move || {
        if let Some(m) = weak.upgrade() {
            dock::dock(m.window(), DOCK_WIDTH, left, &mon);
        }
    });
}

/// Release the screen strip, hide chats, let the engine say goodbye, then quit.
fn shutdown(app: &Rc<RefCell<App>>, main: &MainWindow) {
    dock::undock(main.window());
    let _ = main.hide();
    let mut a = app.borrow_mut();
    for w in a.chats.values() {
        let _ = w.hide();
    }
    match a.engine.clone() {
        Some(e) => {
            a.quitting = true;
            e.send(Command::Shutdown);
            // Safety net if the engine never answers.
            slint::Timer::single_shot(Duration::from_secs(3), || {
                let _ = slint::quit_event_loop();
            });
        }
        None => {
            let _ = slint::quit_event_loop();
        }
    }
}

fn set_my_status(main: &MainWindow, s: Status, dark: bool) {
    main.set_my_status(s.label().into());
    main.set_my_tint(tint(s, dark));
    main.set_my_badge(s.badge().into());
}

fn handle_event(app: &Rc<RefCell<App>>, main: &MainWindow, ev: Event) {
    match ev {
        Event::Unlocked { nick, fingerprint, os, device_class, status, away_msg } => {
            let mut a = app.borrow_mut();
            a.my_nick = nick.clone();
            a.my_status = status;
            main.set_busy(false);
            main.set_logged_in(true);
            main.set_my_nick(nick.into());
            main.set_my_fp(fingerprint.into());
            main.set_my_os(os.into());
            main.set_my_device(device_class.as_str().into());
            main.set_away_msg(away_msg.into());
            set_my_status(main, status, a.dark());
        }
        Event::LoginFailed(msg) => {
            let mut a = app.borrow_mut();
            a.engine = None;
            a.rx = None;
            main.set_busy(false);
            main.set_login_error(msg.into());
        }
        Event::Contacts(list) => {
            app.borrow_mut().contacts = list;
            refresh_contacts(app, main);
        }
        Event::Pending(list) => {
            let rows: Vec<PendingRow> = list
                .iter()
                .map(|p| PendingRow { id: p.id.clone().into(), nick: p.nick.clone().into(), fp: p.fingerprint.clone().into(), text: p.text.clone().into() })
                .collect();
            main.set_pending(ModelRc::new(VecModel::from(rows)));
        }
        Event::History { id, name, lines, .. } => {
            let a = app.borrow();
            let rows: Vec<ChatLine> = lines.iter().map(|l| chat_line(l, &a.my_nick, &name, a.offset)).collect();
            if a.docked_chat.as_deref() == Some(id.as_str()) {
                main.set_dchat_lines(ModelRc::new(VecModel::from(rows.clone())));
            }
            if let Some(w) = a.chats.get(&id) {
                w.set_lines(ModelRc::new(VecModel::from(rows)));
            }
        }
        Event::Invite(s) => {
            app.borrow_mut().invite = s.clone();
            main.set_invite(s.into());
            main.set_panel(2);
        }
        Event::Notice(s) => {
            app.borrow_mut().notice_until = Some(Instant::now() + Duration::from_secs(8));
            main.set_notice(s.into());
        }
        Event::Status(s) => {
            app.borrow_mut().my_status = s;
            let dark = app.borrow().dark();
            set_my_status(main, s, dark);
        }
        Event::Incoming { id, .. } => {
            // ICQ-style: pop the chat open unless we're Occupied/DND.
            let (busy, open) = {
                let a = app.borrow();
                let busy = matches!(a.my_status, Status::Occupied | Status::DoNotDisturb);
                let open = a.docked_chat.as_deref() == Some(id.as_str())
                    || a.chats.get(&id).map(|w| w.window().is_visible()).unwrap_or(false);
                (busy, open)
            };
            if !busy && !open {
                open_chat(app, &id);
            }
        }
        Event::Stopped => {
            if app.borrow().quitting {
                let _ = slint::quit_event_loop();
            }
        }
    }
}

fn main() {
    let (profile, port) = parse_args();
    // Determine the local UTC offset before any threads exist.
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let dir = profile_dir(&profile);
    let settings = load_settings(&dir);

    let main = MainWindow::new().expect("main window");
    apply_theme(&main.global::<Theme>(), palette(&settings.theme));
    main.set_theme_name(settings.theme.clone().into());
    let names: Vec<SharedString> = THEMES.iter().map(|p| p.name.into()).collect();
    main.set_theme_names(ModelRc::new(VecModel::from(names)));
    main.set_split(settings.split);
    main.set_docked(dock::SUPPORTED);
    main.set_profile(profile.clone().into());
    main.set_new_account(!Store::exists(&dir));
    let labels: Vec<SharedString> = Status::SELECTABLE.iter().map(|s| s.label().into()).collect();
    main.set_status_labels(ModelRc::new(VecModel::from(labels)));

    let app = Rc::new(RefCell::new(App {
        dir,
        port,
        engine: None,
        rx: None,
        chats: HashMap::new(),
        contacts: vec![],
        my_nick: String::new(),
        my_status: Status::Online,
        invite: String::new(),
        notice_until: None,
        quitting: false,
        settings,
        layout: vec![],
        docked_chat: None,
        main: main.as_weak(),
        offset,
    }));

    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_set_away(move |t| {
            if let Some(e) = &app.borrow().engine {
                e.send(Command::SetAwayMessage(t.to_string()));
            }
            if let Some(m) = weak.upgrade() {
                m.set_away_msg(t);
            }
        });
    }
    {
        let app = app.clone();
        main.on_split_changed(move |v| {
            let mut a = app.borrow_mut();
            a.settings.split = v;
            save_settings(&a.dir, &a.settings);
        });
    }

    // chat docked in the bar
    {
        let app = app.clone();
        main.on_select_contact(move |id| {
            let docked = app.borrow().docked_chat.clone();
            if docked.is_some() && docked.as_deref() != Some(id.as_str()) {
                dock_chat(&app, &id);
            }
        });
    }
    {
        let app = app.clone();
        main.on_dchat_send(move |t| {
            let a = app.borrow();
            if let (Some(e), Some(id)) = (&a.engine, &a.docked_chat) {
                e.send(Command::SendText { id: id.clone(), body: t.to_string() });
            }
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_dchat_undock(move || {
            let Some(id) = app.borrow_mut().docked_chat.take() else { return };
            if let Some(m) = weak.upgrade() {
                m.set_chat_docked(false);
            }
            open_chat(&app, &id);
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_dchat_close(move || {
            let id = app.borrow_mut().docked_chat.take();
            if let Some(m) = weak.upgrade() {
                m.set_chat_docked(false);
            }
            if let (Some(id), Some(e)) = (id, app.borrow().engine.clone()) {
                e.send(Command::CloseChat { id });
            }
        });
    }

    // login / create
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_login(move |nick, pass| {
            let main = weak.unwrap();
            let mut a = app.borrow_mut();
            if a.engine.is_some() {
                return;
            }
            main.set_login_error("".into());
            main.set_busy(true);
            let (h, rx) = spawn(EngineConfig {
                dir: a.dir.clone(),
                passphrase: pass.to_string(),
                nick: Some(nick.to_string()),
                port: a.port,
                ..Default::default()
            });
            a.engine = Some(h);
            a.rx = Some(rx);
        });
    }

    let cmd = {
        let app = app.clone();
        move |c: Command| {
            if let Some(e) = &app.borrow().engine {
                e.send(c);
            }
        }
    };
    {
        let cmd = cmd.clone();
        main.on_set_status(move |i| cmd(Command::SetStatus(Status::SELECTABLE[i as usize])));
    }
    {
        let cmd = cmd.clone();
        main.on_new_invite(move || cmd(Command::NewInvite));
    }
    {
        let cmd = cmd.clone();
        main.on_add_contact(move |inv, text| cmd(Command::AddContact { invite: inv.to_string(), text: text.to_string() }));
    }
    {
        let cmd = cmd.clone();
        main.on_accept(move |id| cmd(Command::Accept { id: id.to_string() }));
    }
    {
        let cmd = cmd.clone();
        main.on_deny(move |id| cmd(Command::Deny { id: id.to_string() }));
    }
    {
        let cmd = cmd.clone();
        main.on_remove_contact(move |id| cmd(Command::Remove { id: id.to_string() }));
    }
    {
        let app = app.clone();
        main.on_open_chat(move |id| open_chat(&app, &id));
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_copy_invite(move || {
            let inv = app.borrow().invite.clone();
            let ok = arboard::Clipboard::new().and_then(|mut c| c.set_text(inv)).is_ok();
            if let Some(m) = weak.upgrade() {
                m.set_notice(if ok { "Invite copied to clipboard.".into() } else { "Clipboard unavailable — select and copy manually.".into() });
            }
            app.borrow_mut().notice_until = Some(Instant::now() + Duration::from_secs(5));
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_set_theme(move |name| {
            let main = weak.unwrap();
            let next = {
                let mut a = app.borrow_mut();
                let next = palette(&name);
                a.settings.theme = next.name.to_string();
                save_settings(&a.dir, &a.settings);
                for w in a.chats.values() {
                    apply_theme(&w.global::<Theme>(), next);
                }
                next
            };
            apply_theme(&main.global::<Theme>(), next);
            main.set_theme_name(next.name.into());
            let status = app.borrow().my_status;
            set_my_status(&main, status, next.dark);
            refresh_contacts(&app, &main);
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_move_dock(move |dir| {
            let main = weak.unwrap();
            let all = dock::monitors();
            if all.is_empty() {
                return;
            }
            {
                let mut a = app.borrow_mut();
                let cur = dock::pick(&a.settings.monitor).map(|m| m.id).unwrap_or_default();
                let pos = position_index(&all, &cur, a.settings.dock_left) as i32;
                let next = (pos + dir).clamp(0, all.len() as i32 * 2 - 1) as usize;
                a.settings.monitor = all[next / 2].id.clone();
                a.settings.dock_left = next % 2 == 0;
                save_settings(&a.dir, &a.settings);
            }
            redock(&app, &main);
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_quit(move || shutdown(&app, &weak.unwrap()));
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.window().on_close_requested(move || {
            shutdown(&app, &weak.unwrap());
            CloseRequestResponse::HideWindow
        });
    }

    // Pump engine events on the UI thread.
    let pump = slint::Timer::default();
    {
        let app = app.clone();
        let weak = main.as_weak();
        pump.start(slint::TimerMode::Repeated, Duration::from_millis(40), move || {
            let Some(main) = weak.upgrade() else { return };
            let events: Vec<Event> = match &app.borrow().rx {
                Some(rx) => rx.try_iter().collect(),
                None => vec![],
            };
            for ev in events {
                handle_event(&app, &main, ev);
            }
            let expired = app.borrow().notice_until.map(|t| Instant::now() > t).unwrap_or(false);
            if expired {
                app.borrow_mut().notice_until = None;
                main.set_notice("".into());
            }
        });
    }

    // Follow monitor changes (plugged in/out, resolution, scaling): re-dock,
    // moving back to the preferred monitor when it reappears.
    let watch = slint::Timer::default();
    {
        let app = app.clone();
        let weak = main.as_weak();
        watch.start(slint::TimerMode::Repeated, Duration::from_secs(2), move || {
            let Some(main) = weak.upgrade() else { return };
            if app.borrow().layout != dock::monitors() {
                redock(&app, &main);
            }
        });
    }

    main.show().expect("show");
    // Always docked: claim the screen strip once the frameless window exists.
    {
        let app = app.clone();
        let weak = main.as_weak();
        slint::Timer::single_shot(Duration::from_millis(150), move || {
            if let Some(m) = weak.upgrade() {
                redock(&app, &m);
            }
        });
    }
    slint::run_event_loop_until_quit().expect("event loop");
}
