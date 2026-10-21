//! "send to someone" (PRD HB-3): one place to reach anyone in any contact list,
//! with a file of any size. A RIM contact with a direct connection gets the file
//! directly. Everyone else gets a link: the file is encrypted on this computer,
//! the ciphertext waits on the drop (rim-drop), and the key travels only in the
//! link, after the '#', so the drop never sees it.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use super::*;

pub const DEFAULT_DROP: &str = "https://drop.sebhirsch.com";
/// The download page that decrypts in the browser (site/f on GitHub Pages).
pub const PAGE: &str = "https://cyberhirsch.github.io/RogueIM/f/";

pub enum Progress {
    Bytes(u64, u64),
    Done { key: String, text: String },
    Failed(String),
}

pub struct Job {
    pub rx: Receiver<Progress>,
    pub name: String,
}

// ------------------------------------------------ the drop's token, in the OS keychain

fn keychain(profile: &str) -> Option<keyring::Entry> {
    keyring::Entry::new("RogueIM-drop", profile).ok()
}

pub fn set_token(profile: &str, token: &str) -> Result<(), String> {
    keychain(profile).ok_or("no keychain")?.set_password(token).map_err(|e| e.to_string())
}

pub fn token(profile: &str) -> Option<String> {
    keychain(profile)?.get_password().ok().filter(|t| !t.is_empty())
}

// ------------------------------------------------ recipients and their level (PRD TH-1)

pub struct Target {
    pub key: String,
    pub name: String,
    /// Where it goes, e.g. "RIM · direct" or "mail".
    pub place: String,
    pub level: &'static str,
}

/// How a conversation with `key` travels right now: A direct, B relayed or
/// stored in the network, C through another service, D unencrypted.
pub fn level(a: &App, key: &str) -> &'static str {
    match mode_of(key) {
        "notes" | "group" => "B",
        "bot" => "C",
        "net" => match net_key(key).and_then(|(p, _)| a.nets.get(p)) {
            Some(n) if n.level == "D" => "D",
            _ => "C",
        },
        _ if is_rogue(a, key) => "C",
        _ => match a.contact(key) {
            Some(c) if c.direct => "A",
            _ => "B",
        },
    }
}

pub fn level_meaning(level: &str) -> &'static str {
    match level {
        "A" => "level A: direct, end-to-end encrypted",
        "B" => "level B: end-to-end encrypted, via relay or mailbox",
        "C" => "level C: readable by the service in between",
        _ => "level D: not encrypted",
    }
}

/// Everyone who can be reached: RIM contacts, groups and every connected network.
pub fn targets(a: &App) -> Vec<Target> {
    let mut v = vec![];
    for c in a.contacts.iter().filter(|c| !c.awaiting && !c.ignored) {
        let level = level(a, &c.id);
        let place = match level {
            "A" => "RIM · direct",
            "B" => "RIM · mailbox",
            _ => "RIM · bot, passes it on",
        };
        v.push(Target { key: c.id.clone(), name: c.name.clone(), place: place.into(), level });
    }
    for g in &a.groups {
        v.push(Target { key: format!("g:{}", g.id), name: g.name.clone(), place: "RIM group".into(), level: "B" });
    }
    let mut nets: Vec<(&String, &Net)> = a.nets.iter().collect();
    nets.sort_by_key(|(p, _)| a.settings.plugins.iter().position(|x| x == *p).unwrap_or(usize::MAX));
    for (p, n) in nets {
        let level = if n.level == "D" { "D" } else { "C" };
        for c in &n.contacts {
            v.push(Target { key: format!("n:{p}:{}", c.id), name: c.name.clone(), place: n.label.clone(), level });
        }
    }
    v
}

/// Case-insensitive match on name or network.
pub fn matches(t: &Target, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || t.name.to_lowercase().contains(&q) || t.place.to_lowercase().contains(&q)
}

/// A RIM contact connected directly gets the file straight away; every other
/// recipient gets a link to the drop.
pub fn goes_direct(a: &App, key: &str) -> bool {
    mode_of(key) == "contact" && level(a, key) == "A"
}

// ------------------------------------------------ the upload

pub fn days_secs(days: u32) -> u64 {
    u64::from(days.clamp(1, 90)) * 86_400
}

/// Encrypt and upload on a worker thread; the message with the link comes back when done.
pub fn start(drop: &str, token: String, path: &Path, days: u32, key: &str, note: &str) -> Job {
    let (tx, rx) = channel();
    let (drop, path, key, note) = (drop.trim_end_matches('/').to_string(), path.to_path_buf(), key.to_string(), note.trim().to_string());
    let name = file_name(&path);
    let label = name.clone();
    std::thread::spawn(move || {
        let ptx = tx.clone();
        let r = rim_drop::client::upload(&drop, &token, PAGE, &path, days_secs(days), 0, move |done, total| {
            let _ = ptx.send(Progress::Bytes(done, total));
        });
        let _ = tx.send(match r {
            Ok(u) => Progress::Done { key, text: message(&note, &label, file_size(&path), &u.link, days) },
            Err(e) => Progress::Failed(e),
        });
    });
    Job { rx, name }
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into())
}

pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The text that carries the link. Whoever can read it can open the file.
pub fn message(note: &str, name: &str, size: u64, link: &str, days: u32) -> String {
    let note = note.trim();
    let head = if note.is_empty() { String::new() } else { format!("{note}\n\n") };
    let days = days.clamp(1, 90);
    format!("{head}{name} ({}): {link}\nEncrypted; the key is in the link. Deleted after {days} day{}.", human_size(size), if days == 1 { "" } else { "s" })
}

/// What happens when sending to `key`, in one line under the send button.
pub fn explain(a: &App, key: &str, file: Option<&PathBuf>) -> String {
    if key.is_empty() {
        return "Pick someone. Every contact list is searched: RIM, groups and connected networks.".into();
    }
    let lvl = level(a, key);
    let Some(_) = file else {
        return format!("Text goes as a normal message ({}).", level_meaning(lvl));
    };
    if goes_direct(a, key) {
        return "The file goes directly, end-to-end encrypted, no server in between.".into();
    }
    let via = match mode_of(key) {
        "net" => net_key(key).and_then(|(p, _)| a.nets.get(p)).map(|n| n.label.clone()).unwrap_or_default(),
        _ => "RIM".into(),
    };
    let reader = if matches!(lvl, "A" | "B") { "Only they can read the link." } else { "Whoever can read that channel can open the file." };
    format!("Encrypted here, stored on the drop for {} days, the link goes by {via}. {reader}", a.settings.drop_days.clamp(1, 90))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_carries_link_size_and_expiry() {
        let m = message("", "a.pdf", 2048, "https://x/f/#u,k", 1);
        assert!(m.starts_with("a.pdf (2 KB): https://x/f/#u,k\n"), "{m}");
        assert!(m.ends_with("Deleted after 1 day."), "{m}");
        let m = message("  for you  ", "a.pdf", 1, "L", 7);
        assert!(m.starts_with("for you\n\na.pdf"), "{m}");
        assert!(m.ends_with("7 days."), "{m}");
    }

    #[test]
    fn days_are_clamped() {
        assert_eq!(days_secs(0), 86_400);
        assert_eq!(days_secs(400), 90 * 86_400);
    }

    #[test]
    fn search_finds_names_and_networks() {
        let t = Target { key: "n:mail:a@b".into(), name: "Anna Berg".into(), place: "mail".into(), level: "D" };
        assert!(matches(&t, ""));
        assert!(matches(&t, "anna"));
        assert!(matches(&t, "MAIL"));
        assert!(!matches(&t, "signal"));
    }
}
