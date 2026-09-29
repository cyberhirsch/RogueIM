//! Player plugin (PRD GD-9, GD-10): shows what is playing in whatever player is
//! active and controls it; optionally shares "listening to" with contacts.
//! * Windows: System Media Transport Controls (Spotify, browsers, VLC, …)
//! * Linux: MPRIS over D-Bus (any MPRIS player)
//! * macOS: no public API for other apps' playback. Spotify and Music.app are
//!   read and controlled via AppleScript; any other player is controlled with
//!   the system media keys (needs Privacy > Accessibility), without titles.

use std::time::Duration;

use rim_plugin_sdk::{Item, Plugin, ToHost, ToPlugin};

#[derive(Default, Clone, PartialEq)]
struct Now {
    title: String,
    artist: String,
    playing: bool,
    source: String,
}

#[derive(Clone, Copy)]
enum Action {
    Toggle,
    Next,
    Prev,
}

#[cfg(windows)]
mod backend {
    use super::{Action, Now};
    use windows::Media::Control::{GlobalSystemMediaTransportControlsSessionManager as Mgr, GlobalSystemMediaTransportControlsSessionPlaybackStatus as PS};

    fn session() -> Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession> {
        let mgr = Mgr::RequestAsync().ok()?.join().ok()?;
        mgr.GetCurrentSession().ok()
    }

    pub fn now() -> Now {
        let Some(s) = session() else { return Now::default() };
        let props = s.TryGetMediaPropertiesAsync().ok().and_then(|op| op.join().ok());
        let playing = s.GetPlaybackInfo().ok().and_then(|i| i.PlaybackStatus().ok()).map(|p| p == PS::Playing).unwrap_or(false);
        Now {
            title: props.as_ref().and_then(|p| p.Title().ok()).map(|h| h.to_string()).unwrap_or_default(),
            artist: props.as_ref().and_then(|p| p.Artist().ok()).map(|h| h.to_string()).unwrap_or_default(),
            playing,
            source: s.SourceAppUserModelId().map(|h| h.to_string()).unwrap_or_default(),
        }
    }

    pub fn act(a: Action) {
        let Some(s) = session() else { return };
        let _ = match a {
            Action::Toggle => s.TryTogglePlayPauseAsync().and_then(|op| op.join()),
            Action::Next => s.TrySkipNextAsync().and_then(|op| op.join()),
            Action::Prev => s.TrySkipPreviousAsync().and_then(|op| op.join()),
        };
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use super::{Action, Now};
    use zbus::blocking::{fdo::DBusProxy, Connection, Proxy};
    use zbus::zvariant::{OwnedValue, Value};

    fn player() -> Option<(Connection, String)> {
        let conn = Connection::session().ok()?;
        let names = DBusProxy::new(&conn).ok()?.list_names().ok()?;
        let players: Vec<String> = names.iter().map(|n| n.to_string()).filter(|n| n.starts_with("org.mpris.MediaPlayer2.")).collect();
        // Prefer one that is playing.
        let playing = players.iter().find(|n| status(&conn, n).as_deref() == Some("Playing")).cloned();
        let name = playing.or_else(|| players.first().cloned())?;
        Some((conn, name))
    }

    fn proxy<'a>(conn: &'a Connection, name: &'a str) -> Option<Proxy<'a>> {
        Proxy::new(conn, name.to_string(), "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player").ok()
    }

    fn status(conn: &Connection, name: &str) -> Option<String> {
        proxy(conn, name)?.get_property::<String>("PlaybackStatus").ok()
    }

    pub fn now() -> Now {
        let Some((conn, name)) = player() else { return Now::default() };
        let Some(p) = proxy(&conn, &name) else { return Now::default() };
        let meta: std::collections::HashMap<String, OwnedValue> = p.get_property("Metadata").unwrap_or_default();
        let title = meta.get("xesam:title").and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
        let artist = meta
            .get("xesam:artist")
            .and_then(|v| match Value::from(v.clone()) {
                Value::Array(a) => a.iter().filter_map(|x| if let Value::Str(s) = x { Some(s.to_string()) } else { None }).next(),
                _ => None,
            })
            .unwrap_or_default();
        Now { title, artist, playing: status(&conn, &name).as_deref() == Some("Playing"), source: name.trim_start_matches("org.mpris.MediaPlayer2.").to_string() }
    }

    pub fn act(a: Action) {
        let Some((conn, name)) = player() else { return };
        let Some(p) = proxy(&conn, &name) else { return };
        let m = match a {
            Action::Toggle => "PlayPause",
            Action::Next => "Next",
            Action::Prev => "Previous",
        };
        let _: Result<(), _> = p.call(m, &());
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use super::{Action, Now};

    fn osa(script: &str) -> String {
        std::process::Command::new("osascript").args(["-e", script]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
    }

    fn running(app: &str) -> bool {
        osa(&format!("application \"{app}\" is running")) == "true"
    }

    fn app() -> Option<&'static str> {
        ["Spotify", "Music"].into_iter().find(|a| running(a) && osa(&format!("tell application \"{a}\" to player state as string")) == "playing").or_else(|| ["Spotify", "Music"].into_iter().find(|a| running(a)))
    }

    pub fn now() -> Now {
        let Some(a) = app() else { return Now::default() };
        let title = osa(&format!("tell application \"{a}\" to name of current track"));
        let artist = osa(&format!("tell application \"{a}\" to artist of current track"));
        let playing = osa(&format!("tell application \"{a}\" to player state as string")) == "playing";
        Now { title, artist, playing, source: a.to_string() }
    }

    pub fn act(a: Action) {
        let Some(app) = app() else { return media_key(a) };
        let cmd = match a {
            Action::Toggle => "playpause",
            Action::Next => "next track",
            Action::Prev => "previous track",
        };
        osa(&format!("tell application \"{app}\" to {cmd}"));
    }

    /// Press and release a media key (play/pause, next, previous) as the
    /// keyboard would, so whatever player owns the media keys reacts.
    fn media_key(a: Action) {
        use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
        use objc2_core_graphics::{CGEvent, CGEventTapLocation};
        use objc2_foundation::NSPoint;
        // NX_KEYTYPE_PLAY / NEXT / PREVIOUS from IOKit's ev_keymap.h
        let key: isize = match a {
            Action::Toggle => 16,
            Action::Next => 17,
            Action::Prev => 18,
        };
        for down in [true, false] {
            let flags = if down { 0xa00 } else { 0xb00 };
            let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::SystemDefined,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags(flags),
                0.0,
                0,
                None,
                8, // NX_SUBTYPE_AUX_CONTROL_BUTTONS
                (key << 16) | flags as isize,
                -1,
            );
            if let Some(cg) = ev.and_then(|e| e.CGEvent()) {
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&cg));
            }
        }
    }

    /// Shown when no scriptable player runs.
    pub const HINT: &str = "Music/Spotify show titles; other players: buttons only (allow RogueIM in Privacy > Accessibility)";
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod backend {
    use super::{Action, Now};
    pub fn now() -> Now {
        Now::default()
    }
    pub fn act(_: Action) {}
}

fn label(n: &Now) -> String {
    match (n.title.is_empty(), n.artist.is_empty()) {
        (true, _) => String::new(),
        (false, true) => n.title.clone(),
        (false, false) => format!("{} – {}", n.artist, n.title),
    }
}

fn main() {
    let Ok(mut p) = Plugin::connect("player") else {
        eprintln!("rim-plugin-player is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    let mut share = std::fs::read_to_string(std::env::var("RIM_PLUGIN_DATA").unwrap_or_default() + "/share").map(|s| s.trim() == "1").unwrap_or(false);
    let mut last = Now { title: "\u{0}".into(), ..Default::default() };
    let mut shared = String::new();
    loop {
        let msg = p.recv(Duration::from_millis(1500));
        let got = msg.is_some();
        match msg {
            Some(ToPlugin::Stop) => {
                if !shared.is_empty() {
                    p.send(&ToHost::NowPlaying { text: String::new() });
                }
                break;
            }
            Some(ToPlugin::Click { id }) => match id.as_str() {
                "prev" => backend::act(Action::Prev),
                "toggle" => backend::act(Action::Toggle),
                "next" => backend::act(Action::Next),
                _ => {}
            },
            Some(ToPlugin::Check { id, checked }) if id == "share" => {
                share = checked;
                if let Ok(d) = std::env::var("RIM_PLUGIN_DATA") {
                    let _ = std::fs::create_dir_all(&d);
                    let _ = std::fs::write(format!("{d}/share"), if share { "1" } else { "0" });
                }
            }
            _ => {}
        }
        let n = backend::now();
        let want = if share && n.playing { label(&n) } else { String::new() };
        if want != shared {
            shared = want.clone();
            p.send(&ToHost::NowPlaying { text: want });
        }
        if n != last || got {
            last = n.clone();
            let mut items = vec![];
            let l = label(&n);
            items.push(if l.is_empty() { Item::dim("nothing playing", 0) } else { Item::text(&format!("{} {l}", if n.playing { "▶" } else { "❚❚" }), 0) });
            if !n.source.is_empty() {
                items.push(Item::dim(&n.source, 1));
            }
            #[cfg(target_os = "macos")]
            if n.source.is_empty() {
                items.push(Item::dim(backend::HINT, 1));
            }
            items.push(Item::button("prev", "|<", 2));
            items.push(Item::button("toggle", if n.playing { "pause" } else { "play" }, 2));
            items.push(Item::button("next", ">|", 2));
            items.push(Item::check("share", "show contacts what I'm listening to", share, 3));
            p.view(items);
        }
    }
}
