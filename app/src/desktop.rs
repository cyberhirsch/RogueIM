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

const RATE: u32 = 22_050;

/// Build a small 16-bit mono WAV from (frequency Hz, milliseconds) notes.
fn tones(notes: &[(f32, u32)]) -> Vec<u8> {
    let rate = RATE;
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
    wav_bytes(samples)
}

/// Retro "blip ... bloop": notes as (start Hz, end Hz, milliseconds, decay per
/// second); 0 Hz is a pause. The pitch sags towards the end of a note; the
/// octave overtone is the loudest part, which makes it bright and nasal.
fn chirp(notes: &[(f32, f32, u32, f32)]) -> Vec<u8> {
    let rate = RATE as f32;
    let mut samples: Vec<i16> = vec![];
    for &(f0, f1, ms, decay) in notes {
        let n = (RATE * ms / 1000) as usize;
        let mut phase = 0.0f32;
        for i in 0..n {
            let t = i as f32 / rate;
            let x = i as f32 / n as f32;
            let f = f0 + (f1 - f0) * x.powf(1.6);
            phase += f / rate * std::f32::consts::TAU;
            let env = (i as f32 / (0.004 * rate)).min(1.0) * (-decay * t).exp() * ((n - i) as f32 / (0.025 * rate)).min(1.0);
            let v = if f0 == 0.0 { 0.0 } else { (0.55 * phase.sin() + (2.0 * phase).sin() + 0.4 * (3.0 * phase).sin() + 0.15 * (4.0 * phase).sin()) * 0.14 * env };
            samples.push((v * i16::MAX as f32) as i16);
        }
    }
    wav_bytes(samples)
}

fn wav_bytes(samples: Vec<i16>) -> Vec<u8> {
    let rate = RATE;
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
        // short high blip, a pause, a longer tone a third lower that sags
        Sound::Message => chirp(&[(784.0, 784.0, 85, 6.0), (0.0, 0.0, 105, 0.0), (622.0, 596.0, 300, 3.5)]),
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
    last: (u32, bool, String),
    os: String,
    laptop: bool,
}

/// 3x5 pixel glyphs for the OS code on the tray icon (WIN, MAC, UBU, ...).
fn glyph(c: char) -> [u8; 5] {
    match c {
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'C' => [0b011, 0b100, 0b100, 0b100, 0b011],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b101, 0b111, 0b111, 0b111, 0b101],
        'O' => [0b010, 0b101, 0b101, 0b101, 0b010],
        'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'S' => [0b011, 0b100, 0b010, 0b001, 0b110],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
        'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
        _ => [0b111, 0b101, 0b101, 0b101, 0b111],
    }
}

/// 32x32 RGBA: our own OS code inside a monitor or laptop outline, in the
/// status colour (the same identity icon contacts see); filled when `blink`.
fn tray_rgba(rgb: u32, blink: bool, os: &str, laptop: bool) -> Vec<u8> {
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let mut px = vec![0u8; 32 * 32 * 4];
    let mut put = |x: usize, y: usize, on: bool| {
        let i = (y * 32 + x) * 4;
        px[i..i + 4].copy_from_slice(&if on { [r, g, b, 255] } else { [0, 0, 0, 0] });
    };
    for x in 2..30 {
        for t in 0..2 {
            put(x, 4 + t, true);
            put(x, 22 + t, true);
        }
    }
    for y in 4..24 {
        for t in 0..2 {
            put(2 + t, y, true);
            put(28 + t, y, true);
        }
    }
    if laptop {
        for x in 0..32 {
            put(x, 25, true);
            put(x, 26, true);
        }
    } else {
        for x in 13..19 {
            for y in 24..27 {
                put(x, y, true);
            }
        }
        for x in 8..24 {
            put(x, 27, true);
            put(x, 28, true);
        }
    }
    if blink {
        for x in 5..27 {
            for y in 7..21 {
                put(x, y, true);
            }
        }
    }
    // OS code, 3 glyphs of 3x5 scaled by 2, centred in the screen; inverted when blinking.
    for (n, c) in os.chars().take(3).enumerate() {
        let rows = glyph(c.to_ascii_uppercase());
        for (gy, bits) in rows.iter().enumerate() {
            for gx in 0..3 {
                if bits & (0b100 >> gx) != 0 {
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        put(6 + n * 7 + gx * 2 + dx, 9 + gy * 2 + dy, !blink);
                    }
                }
            }
        }
    }
    px
}

impl Tray {
    pub fn new(os: &str, laptop: bool) -> Option<Self> {
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
        let icon = tray_icon::Icon::from_rgba(tray_rgba(0x7cfc9a, false, os, laptop), 32, 32).ok()?;
        let tray = tray_icon::TrayIconBuilder::new().with_tooltip("RogueIM").with_icon(icon).with_menu(Box::new(menu)).build().ok()?;
        Some(Tray { icon: Some(tray), show_id: show.id().clone(), quit_id: quit.id().clone(), status_ids, last: (0, false, String::new()), os: os.to_string(), laptop })
    }

    /// Our own OS and device class, once known (after unlock).
    pub fn set_identity(&mut self, os: &str, laptop: bool) {
        self.os = os.to_string();
        self.laptop = laptop;
        self.last.0 = u32::MAX;
    }

    pub fn set(&mut self, rgb: u32, blink: bool, tooltip: &str) {
        if self.last.0 == rgb && self.last.1 == blink && self.last.2 == tooltip {
            return;
        }
        self.last = (rgb, blink, tooltip.to_string());
        if let Some(t) = &self.icon {
            let _ = t.set_icon(tray_icon::Icon::from_rgba(tray_rgba(rgb, blink, &self.os, self.laptop), 32, 32).ok());
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
    mgr: Option<global_hotkey::GlobalHotKeyManager>,
    keys: Vec<global_hotkey::hotkey::HotKey>,
    map: Vec<(u32, Hotkey)>,
}

/// Default bindings, in the order of `Hotkey::ALL`.
pub const DEFAULT_HOTKEYS: [&str; 5] = ["Ctrl+Alt+R", "Ctrl+Alt+M", "Ctrl+Alt+N", "Ctrl+Alt+A", "Ctrl+Alt+D"];

impl Hotkey {
    pub const ALL: [Hotkey; 5] = [Hotkey::ToggleBar, Hotkey::ReplyLast, Hotkey::Notes, Hotkey::Away, Hotkey::Dnd];
}

impl Hotkeys {
    pub fn none() -> Self {
        Hotkeys { mgr: None, keys: vec![], map: vec![] }
    }

    /// Register the given bindings ("Ctrl+Alt+R"; empty = off). Returns the
    /// hotkeys and a message for every binding that could not be used.
    pub fn new(specs: &[String]) -> (Self, Vec<String>) {
        use global_hotkey::hotkey::HotKey;
        let Ok(mgr) = global_hotkey::GlobalHotKeyManager::new() else { return (Hotkeys::none(), vec!["global hotkeys are not available here".into()]) };
        let mut hk = Hotkeys { mgr: None, keys: vec![], map: vec![] };
        let mut errors = vec![];
        for (spec, h) in specs.iter().zip(Hotkey::ALL) {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            match spec.parse::<HotKey>() {
                Ok(k) => match mgr.register(k) {
                    Ok(()) => {
                        hk.keys.push(k);
                        hk.map.push((k.id(), h));
                    }
                    Err(e) => errors.push(format!("{spec}: {e}")),
                },
                Err(e) => errors.push(format!("{spec}: {e}")),
            }
        }
        hk.mgr = Some(mgr);
        (hk, errors)
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

impl Drop for Hotkeys {
    fn drop(&mut self) {
        if let Some(m) = &self.mgr {
            let _ = m.unregister_all(&self.keys);
        }
    }
}

// ---------------------------------------------------------------- autostart

pub fn set_autostart(profile: &str, on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = if profile == "default" { "RogueIM".to_string() } else { format!("RogueIM ({profile})") };
    let al = auto_launch::AutoLaunchBuilder::new()
        .set_app_name(&name)
        .set_app_path(&exe.to_string_lossy())
        .set_args(&["--profile", profile, "--autostart"])
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

// ---------------------------------------------------------------- screen lock

/// Is the session's screen locked? None if the platform does not say.
pub fn screen_locked() -> Option<bool> {
    #[cfg(windows)]
    {
        // The input desktop cannot be opened while the lock screen owns it.
        use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, DESKTOP_SWITCHDESKTOP};
        unsafe {
            let h = OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP);
            if h.is_null() {
                return Some(true);
            }
            CloseDesktop(h);
            Some(false)
        }
    }
    #[cfg(target_os = "linux")]
    {
        let id = std::env::var("XDG_SESSION_ID").ok()?;
        let out = std::process::Command::new("loginctl").args(["show-session", &id, "-p", "LockedHint", "--value"]).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim() == "yes")
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ioreg").args(["-n", "Root", "-d1", "-a"]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        Some(s.split("<key>CGSSessionScreenIsLocked</key>").nth(1).map(|r| r.trim_start().starts_with("<true/>")).unwrap_or(false))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

// ---------------------------------------------------------------- passphrase in the OS keychain

fn keychain(profile: &str) -> Option<keyring::Entry> {
    keyring::Entry::new("RogueIM", profile).ok()
}

pub fn remembered(profile: &str) -> Option<String> {
    keychain(profile)?.get_password().ok()
}

pub fn remember(profile: &str, pass: &str) -> Result<(), String> {
    keychain(profile).ok_or("no keychain")?.set_password(pass).map_err(|e| e.to_string())
}

pub fn forget(profile: &str) {
    if let Some(e) = keychain(profile) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sound is a valid, non-silent WAV. With RIM_SOUND_DIR set, the
    /// sounds are also written there to listen to.
    #[test]
    fn sounds_render() {
        for (name, s) in [("message", Sound::Message), ("urgent", Sound::Urgent), ("online", Sound::Online), ("auth", Sound::Auth), ("file", Sound::File)] {
            let w = wav(s);
            assert_eq!(&w[..4], b"RIFF");
            assert!(w[44..].chunks(2).any(|c| i16::from_le_bytes([c[0], c[1]]).abs() > 1000), "{name} is silent");
            if let Some(dir) = std::env::var_os("RIM_SOUND_DIR") {
                std::fs::write(std::path::Path::new(&dir).join(format!("{name}.wav")), &w).unwrap();
            }
        }
    }
}
