//! Pomodoro plugin (PRD GD-2..4): work/break timer that sets Occupied (or DND)
//! with "Focusing — back at HH:MM" while you focus, restores your status at the
//! break, and keeps the timer in sync across your devices.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rim_plugin_sdk::{Item, Plugin, ToHost, ToPlugin};
use serde_json::json;

#[derive(Clone, Copy, PartialEq)]
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
    cycles: u32,
    dnd: bool,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
            Phase::Break => self.break_min * 60,
            Phase::Idle => 1,
        }
    }
    fn to_json(&self) -> serde_json::Value {
        json!({
            "phase": match self.phase { Phase::Idle => "idle", Phase::Work => "work", Phase::Break => "break" },
            "ends_at": self.ends_at,
            "paused_left": self.paused_left,
            "work_min": self.work_min,
            "break_min": self.break_min,
        })
    }
}

fn view(p: &mut Plugin, s: &State) {
    let left = s.left();
    let label = match s.phase {
        Phase::Idle => "ready".to_string(),
        Phase::Work => format!("focus {:02}:{:02}{}", left / 60, left % 60, if s.paused_left.is_some() { " (paused)" } else { "" }),
        Phase::Break => format!("break {:02}:{:02}{}", left / 60, left % 60, if s.paused_left.is_some() { " (paused)" } else { "" }),
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
    items.push(Item::check("dnd", "DND instead of Occupied", s.dnd, 4));
    items.push(Item::dim(&format!("{} focus session(s) done", s.cycles), 5));
    p.view(items);
}

fn begin(p: &mut Plugin, s: &mut State, phase: Phase) {
    s.phase = phase;
    s.paused_left = None;
    match phase {
        Phase::Work => {
            s.ends_at = now() + s.work_min * 60;
            let st = if s.dnd { "dnd" } else { "occupied" };
            p.send(&ToHost::Status { status: st.into(), away: format!("Focusing — back at {}", clock(s.ends_at)) });
        }
        Phase::Break => {
            s.ends_at = now() + s.break_min * 60;
            p.send(&ToHost::RestoreStatus);
        }
        Phase::Idle => p.send(&ToHost::RestoreStatus),
    }
    p.send(&ToHost::State { state: s.to_json() });
}

fn main() {
    let Ok(mut p) = Plugin::connect("pomodoro") else {
        eprintln!("rim-plugin-pomodoro is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    let mut s = State { phase: Phase::Idle, ends_at: 0, paused_left: None, work_min: 25, break_min: 5, cycles: 0, dnd: false };
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
                    let next = if s.phase == Phase::Work { Phase::Break } else { Phase::Idle };
                    begin(&mut p, &mut s, next);
                }
                "reset" => begin(&mut p, &mut s, Phase::Idle),
                _ => {}
            },
            Some(ToPlugin::Input { id, value }) => {
                if let Ok(v) = value.trim().parse::<u64>() {
                    let v = v.clamp(1, 180);
                    if id == "work" {
                        s.work_min = v;
                    } else if id == "break" {
                        s.break_min = v;
                    }
                }
            }
            Some(ToPlugin::Check { id, checked }) if id == "dnd" => s.dnd = checked,
            Some(ToPlugin::State { state }) => {
                // Another device started/paused/stopped the timer.
                s.phase = match state["phase"].as_str() {
                    Some("work") => Phase::Work,
                    Some("break") => Phase::Break,
                    _ => Phase::Idle,
                };
                s.ends_at = state["ends_at"].as_u64().unwrap_or(0);
                s.paused_left = state["paused_left"].as_u64();
                s.work_min = state["work_min"].as_u64().unwrap_or(s.work_min);
                s.break_min = state["break_min"].as_u64().unwrap_or(s.break_min);
            }
            _ => {}
        }
        if s.phase != Phase::Idle && s.paused_left.is_none() && s.left() == 0 {
            p.send(&ToHost::Sound { name: "online".into() });
            if s.phase == Phase::Work {
                s.cycles += 1;
                p.send(&ToHost::Notify { title: "Pomodoro".into(), body: format!("Break time — {} min.", s.break_min) });
                begin(&mut p, &mut s, Phase::Break);
            } else {
                p.send(&ToHost::Notify { title: "Pomodoro".into(), body: "Break over.".into() });
                begin(&mut p, &mut s, Phase::Idle);
            }
        }
        view(&mut p, &s);
    }
}
