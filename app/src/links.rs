//! Invite links and the `rim://` scheme.
//!
//! * An invite can be shared as a web link: `INVITE_PAGE#rim2:…`. The page
//!   runs in the browser only; everything after `#` never reaches a server.
//!   It offers "open in RogueIM", which uses `rim://add#rim2:…`.
//! * RogueIM registers itself for `rim://` (per user, no admin rights).
//! * One RogueIM per profile: a second start hands its link (or just "show")
//!   to the running one over a local socket and exits.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver};

use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions, Stream};

/// The invite page (GitHub Pages until RogueIM has its own domain).
pub const INVITE_PAGE: &str = "https://cyberhirsch.github.io/RogueIM/i/";

pub fn invite_link(code: &str) -> String {
    format!("{INVITE_PAGE}#{code}")
}

/// The invite inside a `rim://…` URL or an invite page link.
pub fn invite_from_url(url: &str) -> Option<String> {
    let i = url.find("rim2:")?;
    let code: String = url[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '=')).collect();
    (code.len() > 10).then_some(code)
}

fn socket_name(profile: &str) -> String {
    let safe: String = profile.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("rogueim-ui-{safe}")
}

/// Hand `msg` to a RogueIM already running with this profile. True if one took it.
pub fn hand_over(profile: &str, msg: &str) -> bool {
    let Ok(name) = socket_name(profile).to_ns_name::<GenericNamespaced>() else { return false };
    let Ok(mut s) = Stream::connect(name) else { return false };
    s.write_all(format!("{msg}\n").as_bytes()).is_ok() && s.flush().is_ok()
}

/// Receive what later starts hand over ("show" or a URL).
pub fn listen(profile: &str) -> Option<Receiver<String>> {
    let name = socket_name(profile).to_ns_name::<GenericNamespaced>().ok()?;
    let listener = ListenerOptions::new().name(name).create_sync().ok()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(conn).read_line(&mut line).is_ok() {
                let line = line.trim().to_string();
                if !line.is_empty() && tx.send(line).is_err() {
                    return;
                }
            }
        }
    });
    Some(rx)
}

/// Make `rim://` links open this RogueIM with this profile (current user only).
pub fn register_scheme(profile: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.to_string_lossy().to_string();
    let prof = if profile == "default" { String::new() } else { format!("--profile \"{profile}\" ") };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let reg = |args: &[&str]| std::process::Command::new("reg").args(args).creation_flags(0x0800_0000).output();
        let cmd = format!("\"{exe}\" {prof}\"%1\"");
        // Already pointing at us: nothing to do.
        if let Ok(o) = reg(&["query", r"HKCU\Software\Classes\rim\shell\open\command", "/ve"]) {
            if String::from_utf8_lossy(&o.stdout).contains(&cmd) {
                return;
            }
        }
        let _ = reg(&["add", r"HKCU\Software\Classes\rim", "/ve", "/d", "URL:RogueIM", "/f"]);
        let _ = reg(&["add", r"HKCU\Software\Classes\rim", "/v", "URL Protocol", "/d", "", "/f"]);
        let _ = reg(&["add", r"HKCU\Software\Classes\rim\shell\open\command", "/ve", "/d", &cmd, "/f"]);
    }
    #[cfg(target_os = "linux")]
    {
        let Some(home) = std::env::var_os("HOME") else { return };
        let dir = std::path::Path::new(&home).join(".local/share/applications");
        let file = dir.join("rogueim-url.desktop");
        let body = format!("[Desktop Entry]\nType=Application\nName=RogueIM\nExec=\"{exe}\" {prof}%u\nNoDisplay=true\nMimeType=x-scheme-handler/rim;\n");
        if std::fs::read_to_string(&file).map(|s| s == body).unwrap_or(false) {
            return;
        }
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::write(&file, body).is_ok() {
            let _ = std::process::Command::new("xdg-mime").args(["default", "rogueim-url.desktop", "x-scheme-handler/rim"]).output();
        }
    }
    // macOS: the scheme is declared in Info.plist (CFBundleURLTypes).
    let _ = (exe, prof);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_in_urls() {
        let code = "rim2:eyJ2IjoyLCJjYXJkIjp7fX0abc-_";
        assert_eq!(invite_from_url(&format!("rim://add#{code}")).as_deref(), Some(code));
        assert_eq!(invite_from_url(&format!("rim://add/{code}/")).as_deref(), Some(code));
        assert_eq!(invite_from_url(&invite_link(code)).as_deref(), Some(code));
        assert_eq!(invite_from_url("rim://add#nothing"), None);
    }
}
