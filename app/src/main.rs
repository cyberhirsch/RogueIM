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
struct Settings {
    theme: String,
    dock_left: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { theme: "grey".into(), dock_left: false }
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
}

/// Grey (default, Win9x-era silver), Graphite (dark grey), Green and Amber (terminal).
const THEMES: [Palette; 4] = [
    Palette { name: "grey", dark: false, bg: 0xc0c0c0, panel: 0xffffff, line: 0x808080, fg: 0x000000, dim: 0x404040, faint: 0x808080, accent: 0x000080, red: 0xa00000, hover: 0xd8d8e8, on_accent: 0xffffff },
    Palette { name: "graphite", dark: true, bg: 0x1e1f22, panel: 0x2a2c30, line: 0x3a3d42, fg: 0xd6d6d6, dim: 0x8a8f98, faint: 0x54585f, accent: 0xe0a040, red: 0xff6b6b, hover: 0x34373c, on_accent: 0x1e1f22 },
    Palette { name: "green", dark: true, bg: 0x070a07, panel: 0x0d130e, line: 0x1c3322, fg: 0x7cfc9a, dim: 0x3f7a4f, faint: 0x24402c, accent: 0xffb000, red: 0xff5b5b, hover: 0x1c3322, on_accent: 0x070a07 },
    Palette { name: "amber", dark: true, bg: 0x0a0700, panel: 0x140e02, line: 0x3a2a08, fg: 0xffb000, dim: 0x9a6a00, faint: 0x4a3500, accent: 0xffd870, red: 0xff5b5b, hover: 0x3a2a08, on_accent: 0x0a0700 },
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
    let mut a = app.borrow_mut();
    let Some(engine) = a.engine.clone() else { return };
    if !a.chats.contains_key(id) {
        let w = ChatWindow::new().expect("chat window");
        apply_theme(&w.global::<Theme>(), palette(&a.settings.theme));
        let eng = engine.clone();
        let cid = id.to_string();
        w.on_send(move |t| eng.send(Command::SendText { id: cid.clone(), body: t.to_string() }));
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
    }
}

fn set_my_status(main: &MainWindow, s: Status, dark: bool) {
    main.set_my_status(s.label().into());
    main.set_my_tint(tint(s, dark));
    main.set_my_badge(s.badge().into());
}

fn handle_event(app: &Rc<RefCell<App>>, main: &MainWindow, ev: Event) {
    match ev {
        Event::Unlocked { nick, fingerprint, os, device_class, status } => {
            let mut a = app.borrow_mut();
            a.my_nick = nick.clone();
            a.my_status = status;
            main.set_busy(false);
            main.set_logged_in(true);
            main.set_my_nick(nick.into());
            main.set_my_fp(fingerprint.into());
            main.set_my_os(os.into());
            main.set_my_device(device_class.as_str().into());
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
            if let Some(w) = a.chats.get(&id) {
                let rows: Vec<ChatLine> = lines.iter().map(|l| chat_line(l, &a.my_nick, &name, a.offset)).collect();
                w.set_lines(ModelRc::new(VecModel::from(rows)));
                let weak = w.as_weak();
                slint::Timer::single_shot(Duration::from_millis(40), move || {
                    if let Some(w) = weak.upgrade() {
                        w.invoke_scroll_to_bottom();
                    }
                });
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
                let open = a.chats.get(&id).map(|w| w.window().is_visible()).unwrap_or(false);
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
    main.set_edge(if settings.dock_left { "left".into() } else { "right".into() });
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
        offset,
    }));

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
        main.on_next_theme(move || {
            let main = weak.unwrap();
            let next = {
                let mut a = app.borrow_mut();
                let i = THEMES.iter().position(|p| p.name == a.settings.theme).unwrap_or(0);
                let next = &THEMES[(i + 1) % THEMES.len()];
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
            main.set_notice(format!("theme: {}", next.name).into());
            app.borrow_mut().notice_until = Some(Instant::now() + Duration::from_secs(3));
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.on_switch_edge(move || {
            let main = weak.unwrap();
            let left = {
                let mut a = app.borrow_mut();
                a.settings.dock_left = !a.settings.dock_left;
                save_settings(&a.dir, &a.settings);
                a.settings.dock_left
            };
            main.set_edge(if left { "left".into() } else { "right".into() });
            dock::undock(main.window());
            dock::dock(main.window(), DOCK_WIDTH, left);
        });
    }
    {
        let app = app.clone();
        let weak = main.as_weak();
        main.window().on_close_requested(move || {
            let main = weak.unwrap();
            dock::undock(main.window());
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

    main.show().expect("show");
    // Always docked: claim the screen strip once the frameless window exists.
    {
        let weak = main.as_weak();
        let left = app.borrow().settings.dock_left;
        slint::Timer::single_shot(Duration::from_millis(150), move || {
            if let Some(m) = weak.upgrade() {
                dock::dock(m.window(), DOCK_WIDTH, left);
            }
        });
    }
    slint::run_event_loop_until_quit().expect("event loop");
}
