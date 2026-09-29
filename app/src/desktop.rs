//! Desktop integration: sounds (generated, original), notifications, tray icon,
//! global hotkeys, start-with-the-computer, idle time.

use std::io::Cursor;

// ---------------------------------------------------------------- sounds

#[derive(Clone, Copy)]
pub enum Sound {
    Message,
    Urgent,
    Online,
    Auth,
    File,
}

/// Build a small 16-bit mono WAV from (frequency Hz, milliseconds) notes.
fn tones(notes: &[(f32, u32)]) -> Vec<u8> {
    let rate = 22_050u32;
    let mut samples: Vec<i16> = vec![];
    for &(f, ms) in notes {
        let n = rate * ms / 1000;
        for i in 0..n {
            let t = i as f32 / rate as f32;
            // soft attack/decay so it doesn't click
            let env = (i as f32 / 200.0).min(1.0) * ((n - i) as f32 / 400.0).min(1.0);
            let v = if f == 0.0 { 0.0 } else { (t * f * std::f32::consts::TAU).sin() * 0.25 * env };
            samples.push((v * i16::MAX as f32) as i16);
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

fn wav(s: Sound) -> Vec<u8> {
    match s {
        // two-note "blip-bloop", falling — our own, not ICQ's
        Sound::Message => tones(&[(988.0, 70), (0.0, 30), (740.0, 110)]),
        Sound::Urgent => tones(&[(1319.0, 80), (0.0, 50), (1319.0, 80), (0.0, 50), (1319.0, 120)]),
        Sound::Online => tones(&[(523.0, 60), (659.0, 60), (784.0, 120)]),
        Sound::Auth => tones(&[(392.0, 60), (0.0, 60), (392.0, 60)]),
        Sound::File => tones(&[(880.0, 160)]),
    }
}

pub struct Audio {
    sink: Option<rodio::MixerDeviceSink>,
    players: Vec<rodio::Player>,
}

impl Audio {
    pub fn none() -> Self {
        Audio { sink: None, players: vec![] }
    }

    pub fn new() -> Self {
        Audio { sink: rodio::DeviceSinkBuilder::open_default_sink().ok(), players: vec![] }
    }

    pub fn play(&mut self, s: Sound) {
        let Some(sink) = &self.sink else { return };
        self.players.retain(|p| !p.empty());
        if let Ok(p) = rodio::play(sink.mixer(), Cursor::new(wav(s))) {
            self.players.push(p);
        }
    }
}

// ---------------------------------------------------------------- notifications

pub fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    // Some backends block briefly; keep the UI thread free.
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new().appname("RogueIM").summary(&title).body(&body).show();
    });
}

// ---------------------------------------------------------------- tray

pub struct Tray {
    icon: Option<tray_icon::TrayIcon>,
    pub show_id: tray_icon::menu::MenuId,
    pub quit_id: tray_icon::menu::MenuId,
    pub status_ids: Vec<(tray_icon::menu::MenuId, rim_core::Status)>,
    last: (u32, bool),
}

/// 32x32 RGBA: a monitor outline in the status colour, filled when `blink`.
fn tray_rgba(rgb: u32, blink: bool) -> Vec<u8> {
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let mut px = vec![0u8; 32 * 32 * 4];
    let mut set = |x: usize, y: usize| {
        let i = (y * 32 + x) * 4;
        px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
    };
    for x in 2..30 {
        for t in 0..2 {
            set(x, 4 + t);
            set(x, 22 + t);
        }
    }
    for y in 4..24 {
        for t in 0..2 {
            set(2 + t, y);
            set(28 + t, y);
        }
    }
    for x in 13..19 {
        for y in 24..27 {
            set(x, y);
        }
    }
    for x in 8..24 {
        set(x, 27);
        set(x, 28);
    }
    if blink {
        for x in 6..26 {
            for y in 8..20 {
                set(x, y);
            }
        }
    }
    px
}

impl Tray {
    pub fn new() -> Option<Self> {
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        let menu = Menu::new();
        let show = MenuItem::new("Show / hide RogueIM", true, None);
        let mut status_ids = vec![];
        let _ = menu.append(&show);
        let _ = menu.append(&PredefinedMenuItem::separator());
        for s in rim_core::Status::SELECTABLE {
            let it = MenuItem::new(s.label(), true, None);
            status_ids.push((it.id().clone(), s));
            let _ = menu.append(&it);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let quit = MenuItem::new("Quit", true, None);
        let _ = menu.append(&quit);
        let icon = tray_icon::Icon::from_rgba(tray_rgba(0x7cfc9a, false), 32, 32).ok()?;
        let tray = tray_icon::TrayIconBuilder::new().with_tooltip("RogueIM").with_icon(icon).with_menu(Box::new(menu)).build().ok()?;
        Some(Tray { icon: Some(tray), show_id: show.id().clone(), quit_id: quit.id().clone(), status_ids, last: (0, false) })
    }

    pub fn set(&mut self, rgb: u32, blink: bool, tooltip: &str) {
        if self.last == (rgb, blink) {
            return;
        }
        self.last = (rgb, blink);
        if let Some(t) = &self.icon {
            let _ = t.set_icon(tray_icon::Icon::from_rgba(tray_rgba(rgb, blink), 32, 32).ok());
            let _ = t.set_tooltip(Some(tooltip));
        }
    }
}

// ---------------------------------------------------------------- hotkeys

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hotkey {
    ToggleBar,
    ReplyLast,
    Notes,
    Away,
    Dnd,
}

pub struct Hotkeys {
    _mgr: Option<global_hotkey::GlobalHotKeyManager>,
    map: Vec<(u32, Hotkey)>,
}

pub const HOTKEYS_INFO: &str = "hotkeys: Ctrl+Alt+R show/hide · Ctrl+Alt+M reply to last · Ctrl+Alt+N note to self · Ctrl+Alt+A away · Ctrl+Alt+D do not disturb";

impl Hotkeys {
    pub fn none() -> Self {
        Hotkeys { _mgr: None, map: vec![] }
    }

    pub fn new() -> Self {
        use global_hotkey::hotkey::{Code, HotKey, Modifiers};
        let Ok(mgr) = global_hotkey::GlobalHotKeyManager::new() else { return Hotkeys { _mgr: None, map: vec![] } };
        let mods = Some(Modifiers::CONTROL | Modifiers::ALT);
        let keys = [
            (HotKey::new(mods, Code::KeyR), Hotkey::ToggleBar),
            (HotKey::new(mods, Code::KeyM), Hotkey::ReplyLast),
            (HotKey::new(mods, Code::KeyN), Hotkey::Notes),
            (HotKey::new(mods, Code::KeyA), Hotkey::Away),
            (HotKey::new(mods, Code::KeyD), Hotkey::Dnd),
        ];
        let mut map = vec![];
        for (k, h) in keys {
            if mgr.register(k).is_ok() {
                map.push((k.id(), h));
            }
        }
        Hotkeys { _mgr: Some(mgr), map }
    }

    pub fn poll(&self) -> Vec<Hotkey> {
        let mut out = vec![];
        while let Ok(ev) = global_hotkey::GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state == global_hotkey::HotKeyState::Pressed {
                if let Some((_, h)) = self.map.iter().find(|(id, _)| *id == ev.id) {
                    out.push(*h);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------- autostart

pub fn set_autostart(profile: &str, on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = if profile == "default" { "RogueIM".to_string() } else { format!("RogueIM ({profile})") };
    let al = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(&name)
        .set_app_path(&exe.to_string_lossy())
        .set_args(&["--profile", profile])
        .build()
        .map_err(|e| e.to_string())?;
    if on { al.enable() } else { al.disable() }.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- idle time

/// Seconds since the last keyboard/mouse input, if the platform tells us.
pub fn idle_secs() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetTickCount;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        unsafe {
            if GetLastInputInfo(&mut li) != 0 {
                return Some((GetTickCount().wrapping_sub(li.dwTime) / 1000) as u64);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ioreg").args(["-c", "IOHIDSystem"]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let line = s.lines().find(|l| l.contains("HIDIdleTime"))?;
        let ns: u64 = line.rsplit('=').next()?.trim().parse().ok()?;
        Some(ns / 1_000_000_000)
    }
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("xprintidle").output().ok()?;
        let ms: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(ms / 1000)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
