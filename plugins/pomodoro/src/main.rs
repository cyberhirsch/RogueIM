//! Pomodoro plugin (PRD GD-2..4): work/break timer that sets Occupied (or DND)
//! with "Focusing — back at HH:MM" while you focus, restores your status at the
//! break, and keeps the timer in sync across your devices. Every N focus
//! sessions the break is the long one. Settings live in the plugin data dir.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rim_plugin_sdk::{Item, Plugin, ToHost, ToPlugin};
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Idle,
    Work,
    Break,
}

struct State {
    phase: Phase,
    ends_at: u64,
    paused_left: Option<u64>,
    work_min: u64,
    break_min: u64,
    /// Long break length, taken after every `every` completed focus sessions.
    long_min: u64,
    every: u32,
    /// The current break is the long one.
    long: bool,
    cycles: u32,
    dnd: bool,
    /// Play a sound when a phase ends.
    sound: bool,
}

impl Default for State {
    fn default() -> Self {
        State { phase: Phase::Idle, ends_at: 0, paused_left: None, work_min: 25, break_min: 5, long_min: 15, every: 4, long: false, cycles: 0, dnd: false, sound: true }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn data_dir() -> PathBuf {
    std::env::var_os("RIM_PLUGIN_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn clock(ts: u64) -> String {
    let off = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    time::OffsetDateTime::from_unix_timestamp(ts as i64)
        .map(|t| t.to_offset(off))
        .map(|t| format!("{:02}:{:02}", t.hour(), t.minute()))
        .unwrap_or_default()
}

impl State {
    fn left(&self) -> u64 {
        self.paused_left.unwrap_or_else(|| self.ends_at.saturating_sub(now()))
    }
    fn total(&self) -> u64 {
        match self.phase {
            Phase::Work => self.work_min * 60,
            Phase::Break => self.break_len() * 60,
            Phase::Idle => 1,
        }
    }
    /// Minutes of the current (or next) break.
    fn break_len(&self) -> u64 {
        if self.long { self.long_min } else { self.break_min }
    }
    /// A focus session ran to its end: count it and decide whether the break
    /// that follows is the long one.
    fn finish_work(&mut self) {
        self.cycles += 1;
        self.long = self.cycles.is_multiple_of(self.every.max(1));
    }
    /// Timer state shared with the user's other devices (includes the timer
    /// settings; `dnd` stays per device).
    fn to_json(&self) -> serde_json::Value {
        json!({
            "phase": match self.phase { Phase::Idle => "idle", Phase::Work => "work", Phase::Break => "break" },
            "ends_at": self.ends_at,
            "paused_left": self.paused_left,
            "long": self.long,
            "work_min": self.work_min,
            "break_min": self.break_min,
            "long_min": self.long_min,
            "cycles_before_long": self.every,
            "sound": self.sound,
        })
    }
    /// What `pomodoro.json` in the data dir holds.
    fn settings_json(&self) -> serde_json::Value {
        json!({
            "work_min": self.work_min,
            "break_min": self.break_min,
            "long_min": self.long_min,
            "cycles_before_long": self.every,
            "sound": self.sound,
            "dnd": self.dnd,
        })
    }
    /// Apply settings from `pomodoro.json`, synced state or an input; missing
    /// keys keep their current value.
    fn apply_settings(&mut self, v: &serde_json::Value) {
        let min = |k: &str, cur: u64| v[k].as_u64().map(|x| x.clamp(1, 180)).unwrap_or(cur);
        self.work_min = min("work_min", self.work_min);
        self.break_min = min("break_min", self.break_min);
        self.long_min = min("long_min", self.long_min);
        self.every = v["cycles_before_long"].as_u64().map(|x| x.clamp(1, 12) as u32).unwrap_or(self.every);
        self.sound = v["sound"].as_bool().unwrap_or(self.sound);
        self.dnd = v["dnd"].as_bool().unwrap_or(self.dnd);
    }
    fn load(&mut self) {
        if let Some(v) = std::fs::read(data_dir().join("pomodoro.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
            self.apply_settings(&v);
        }
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(data_dir().join("pomodoro.json"), self.settings_json().to_string());
    }
}

fn view(p: &mut Plugin, s: &State) {
    let left = s.left();
    let paused = if s.paused_left.is_some() { " (paused)" } else { "" };
    let label = match s.phase {
        Phase::Idle => "ready".to_string(),
        Phase::Work => format!("focus {:02}:{:02}{paused}", left / 60, left % 60),
        Phase::Break => format!("{} {:02}:{:02}{paused}", if s.long { "long break" } else { "break" }, left / 60, left % 60),
    };
    let mut items = vec![Item::text(&label, 0)];
    if s.phase != Phase::Idle {
        items.push(Item::progress(1.0 - left as f32 / s.total().max(1) as f32, 1));
    }
    match s.phase {
        Phase::Idle => items.push(Item::button("start", "start", 2)),
        _ => {
            items.push(Item::button(if s.paused_left.is_some() { "resume" } else { "pause" }, if s.paused_left.is_some() { "resume" } else { "pause" }, 2));
            items.push(Item::button("skip", "skip", 2));
            items.push(Item::button("reset", "reset", 2));
        }
    }
    items.push(Item::input("work", &s.work_min.to_string(), "work min", 3));
    items.push(Item::input("break", &s.break_min.to_string(), "break min", 3));
    items.push(Item::input("long", &s.long_min.to_string(), "long break min", 4));
    items.push(Item::input("every", &s.every.to_string(), "sessions before long break", 4));
    items.push(Item::check("dnd", "DND instead of Occupied", s.dnd, 5));
    items.push(Item::check("sound", "sound at phase end", s.sound, 6));
    let every = s.every.max(1);
    items.push(Item::dim(&format!("{} focus session(s) done, long break after {} more", s.cycles, every - s.cycles % every), 7));
    p.view(items);
}

fn begin(p: &mut Plugin, s: &mut State, phase: Phase) {
    s.phase = phase;
    s.paused_left = None;
    match phase {
        Phase::Work => {
            s.long = false;
            s.ends_at = now() + s.work_min * 60;
            let st = if s.dnd { "dnd" } else { "occupied" };
            p.send(&ToHost::Status { status: st.into(), away: format!("Focusing — back at {}", clock(s.ends_at)) });
        }
        Phase::Break => {
            s.ends_at = now() + s.break_len() * 60;
            p.send(&ToHost::RestoreStatus);
        }
        Phase::Idle => {
            s.long = false;
            p.send(&ToHost::RestoreStatus);
        }
    }
    p.send(&ToHost::State { state: s.to_json() });
}

fn main() {
    let Ok(mut p) = Plugin::connect("pomodoro") else {
        eprintln!("rim-plugin-pomodoro is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    let mut s = State::default();
    s.load();
    view(&mut p, &s);
    loop {
        match p.recv(Duration::from_secs(1)) {
            Some(ToPlugin::Stop) => {
                if s.phase == Phase::Work {
                    p.send(&ToHost::RestoreStatus);
                }
                break;
            }
            Some(ToPlugin::Click { id }) => match id.as_str() {
                "start" => begin(&mut p, &mut s, Phase::Work),
                "pause" => {
                    s.paused_left = Some(s.left());
                    p.send(&ToHost::State { state: s.to_json() });
                }
                "resume" => {
                    s.ends_at = now() + s.paused_left.take().unwrap_or(0);
                    p.send(&ToHost::State { state: s.to_json() });
                }
                "skip" => {
                    // A skipped focus session does not count towards the long break.
                    let next = if s.phase == Phase::Work { Phase::Break } else { Phase::Idle };
                    begin(&mut p, &mut s, next);
                }
                "reset" => begin(&mut p, &mut s, Phase::Idle),
                _ => {}
            },
            Some(ToPlugin::Input { id, value }) => {
                let key = match id.as_str() {
                    "work" => "work_min",
                    "break" => "break_min",
                    "long" => "long_min",
                    "every" => "cycles_before_long",
                    _ => "",
                };
                if let (false, Ok(v)) = (key.is_empty(), value.trim().parse::<u64>()) {
                    s.apply_settings(&json!({ key: v }));
                    s.save();
                    p.send(&ToHost::State { state: s.to_json() });
                }
            }
            Some(ToPlugin::Check { id, checked }) if id == "dnd" || id == "sound" => {
                if id == "dnd" {
                    s.dnd = checked;
                } else {
                    s.sound = checked;
                    p.send(&ToHost::State { state: s.to_json() });
                }
                s.save();
            }
            Some(ToPlugin::State { state }) => {
                // Another device started/paused/stopped the timer or changed a setting.
                s.phase = match state["phase"].as_str() {
                    Some("work") => Phase::Work,
                    Some("break") => Phase::Break,
                    _ => Phase::Idle,
                };
                s.ends_at = state["ends_at"].as_u64().unwrap_or(0);
                s.paused_left = state["paused_left"].as_u64();
                s.long = state["long"].as_bool().unwrap_or(false);
                let dnd = s.dnd;
                s.apply_settings(&state);
                s.dnd = dnd;
                s.save();
            }
            _ => {}
        }
        if s.phase != Phase::Idle && s.paused_left.is_none() && s.left() == 0 {
            if s.sound {
                p.send(&ToHost::Sound { name: "online".into() });
            }
            if s.phase == Phase::Work {
                s.finish_work();
                let kind = if s.long { "Long break" } else { "Break" };
                p.send(&ToHost::Notify { title: "Pomodoro".into(), body: format!("{kind} time — {} min.", s.break_len()) });
                begin(&mut p, &mut s, Phase::Break);
            } else {
                p.send(&ToHost::Notify { title: "Pomodoro".into(), body: "Break over.".into() });
                begin(&mut p, &mut s, Phase::Idle);
            }
        }
        view(&mut p, &s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fourth_break_is_long_by_default() {
        let mut s = State::default();
        assert_eq!((s.every, s.long_min, s.break_min, s.sound), (4, 15, 5, true));
        s.phase = Phase::Break;
        let lens: Vec<u64> = (0..8)
            .map(|_| {
                s.finish_work();
                s.break_len()
            })
            .collect();
        assert_eq!(lens, vec![5, 5, 5, 15, 5, 5, 5, 15]);
        assert_eq!(s.total(), 15 * 60);
    }

    #[test]
    fn cycles_setting_changes_the_rhythm() {
        let mut s = State::default();
        s.apply_settings(&json!({"cycles_before_long": 2, "long_min": 30}));
        let longs: Vec<bool> = (0..4)
            .map(|_| {
                s.finish_work();
                s.long
            })
            .collect();
        assert_eq!(longs, vec![false, true, false, true]);
        assert_eq!(s.break_len(), 30);
        // 0 is clamped to 1: every break is long.
        s.apply_settings(&json!({"cycles_before_long": 0}));
        assert_eq!(s.every, 1);
        s.finish_work();
        assert!(s.long);
    }

    #[test]
    fn settings_round_trip_and_sync_shape() {
        let mut a = State::default();
        a.apply_settings(&json!({"work_min": 50, "break_min": 10, "long_min": 20, "cycles_before_long": 3, "sound": false, "dnd": true}));
        let mut b = State::default();
        b.apply_settings(&a.settings_json());
        assert_eq!((b.work_min, b.break_min, b.long_min, b.every, b.sound, b.dnd), (50, 10, 20, 3, false, true));
        let st = a.to_json();
        assert_eq!(st["long_min"], 20);
        assert_eq!(st["cycles_before_long"], 3);
        assert_eq!(st["sound"], false);
        assert!(st.get("dnd").is_none());
    }
}
