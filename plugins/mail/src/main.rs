//! Mail plugin (PRD HB-2): your email in RogueIM's buddy list. People you
//! write to become contacts in a "mail" folder, a correspondent's mails one
//! chat. IMAP reads, SMTP sends, the password lives in the OS keychain.
//! Mail is not end-to-end encrypted, and RogueIM says so in every mail chat
//! (PRD TH-2, TH-3). On disk there are only the settings (mail.json), no mail.

mod imap;
mod smtp;
mod text;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use imap::{Account, Job, Up};
use rim_plugin_sdk::{Item, Plugin, ToHost, ToPlugin};
use serde::{Deserialize, Serialize};

const WARNING: &str = "mail is not end-to-end encrypted: your mail provider and the recipient's can read it";

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Config {
    address: String,
    name: String,
    /// "host:port"
    imap: String,
    smtp: String,
    /// Login name, when it isn't the address.
    user: String,
    /// Added by hand (address, name) and taken off the list.
    added: Vec<(String, String)>,
    removed: Vec<String>,
}

fn data_dir() -> PathBuf {
    std::env::var_os("RIM_PLUGIN_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn load_config() -> Config {
    std::fs::read(data_dir().join("mail.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_config(c: &Config) {
    let _ = std::fs::create_dir_all(data_dir());
    let _ = std::fs::write(data_dir().join("mail.json"), serde_json::to_string_pretty(c).unwrap_or_default());
}

fn login(c: &Config) -> String {
    if c.user.trim().is_empty() { c.address.trim().to_string() } else { c.user.trim().to_string() }
}

fn keychain(c: &Config) -> Option<keyring::Entry> {
    keyring::Entry::new("RogueIM mail", &format!("{}@{}", login(c), host_port(&c.imap, 993).0)).ok()
}

/// "host:port", or just "host" with the default port.
fn host_port(s: &str, default: u16) -> (String, u16) {
    let s = s.trim();
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap_or(default)),
        _ => (s.to_string(), default),
    }
}

/// IMAP and SMTP servers for an address: known providers, else imap./smtp. at its domain.
fn guess(address: &str) -> (String, String) {
    let domain = address.trim().rsplit_once('@').map(|(_, d)| d.to_lowercase()).unwrap_or_default();
    if domain.is_empty() {
        return (String::new(), String::new());
    }
    let (i, s) = match domain.as_str() {
        "gmail.com" | "googlemail.com" => ("imap.gmail.com:993", "smtp.gmail.com:465"),
        "outlook.com" | "outlook.de" | "hotmail.com" | "hotmail.de" | "live.com" | "live.de" | "msn.com" => ("outlook.office365.com:993", "smtp-mail.outlook.com:587"),
        "gmx.de" | "gmx.net" | "gmx.at" | "gmx.ch" => ("imap.gmx.net:993", "mail.gmx.net:465"),
        "web.de" => ("imap.web.de:993", "smtp.web.de:587"),
        "posteo.de" | "posteo.net" => ("posteo.de:993", "posteo.de:465"),
        "mailbox.org" => ("imap.mailbox.org:993", "smtp.mailbox.org:465"),
        "icloud.com" | "me.com" | "mac.com" => ("imap.mail.me.com:993", "smtp.mail.me.com:587"),
        "yahoo.com" | "yahoo.de" => ("imap.mail.yahoo.com:993", "smtp.mail.yahoo.com:465"),
        "t-online.de" | "magenta.de" => ("secureimap.t-online.de:993", "securesmtp.t-online.de:465"),
        "fastmail.com" | "fastmail.fm" => ("imap.fastmail.com:993", "smtp.fastmail.com:465"),
        "aol.com" => ("imap.aol.com:993", "smtp.aol.com:465"),
        _ => return (format!("imap.{domain}:993"), format!("smtp.{domain}:465")),
    };
    (i.to_string(), s.to_string())
}

fn account(c: &Config, pass: String) -> Result<Account, String> {
    let address = c.address.trim().to_lowercase();
    if !imap::valid_address(&address) {
        return Err("enter your email address".into());
    }
    let (imap_host, imap_port) = host_port(&c.imap, 993);
    let (smtp_host, smtp_port) = host_port(&c.smtp, 465);
    if imap_host.is_empty() || smtp_host.is_empty() {
        return Err("enter the IMAP and SMTP servers".into());
    }
    Ok(Account { address, name: c.name.trim().to_string(), user: login(c), pass, imap_host, imap_port, smtp_host, smtp_port, added: c.added.clone(), removed: c.removed.clone() })
}

fn now_hm() -> String {
    let t = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{:02}:{:02}", t.hour(), t.minute())
}

struct State {
    cfg: Config,
    /// Typed, not yet stored.
    pass: String,
    /// Stored in the keychain once the login worked.
    pending_pass: Option<String>,
    status: String,
    connected: bool,
    /// Settings shown while connected.
    form: bool,
    people: usize,
    unread: u32,
    checked: String,
    announced: (String, String),
    jobs: mpsc::Sender<Job>,
}

impl State {
    fn set(&mut self, id: &str, value: String) {
        match id {
            "address" => {
                let old = guess(&self.cfg.address);
                self.cfg.address = value.trim().to_string();
                let new = guess(&self.cfg.address);
                // Follow the address until the user picks servers by hand.
                if self.cfg.imap.is_empty() || self.cfg.imap == old.0 {
                    self.cfg.imap = new.0;
                }
                if self.cfg.smtp.is_empty() || self.cfg.smtp == old.1 {
                    self.cfg.smtp = new.1;
                }
            }
            "pass" => self.pass = value,
            "name" => self.cfg.name = value,
            "imap" => self.cfg.imap = value.trim().to_string(),
            "smtp" => self.cfg.smtp = value.trim().to_string(),
            "user" => self.cfg.user = value.trim().to_string(),
            _ => {}
        }
    }

    fn connect(&mut self) {
        let typed = !self.pass.is_empty();
        let pass = if typed { self.pass.clone() } else { keychain(&self.cfg).and_then(|k| k.get_password().ok()).unwrap_or_default() };
        if pass.is_empty() {
            self.status = "enter the password".into();
            return;
        }
        match account(&self.cfg, pass.clone()) {
            Ok(acc) => {
                save_config(&self.cfg);
                self.pending_pass = typed.then_some(pass);
                self.pass.clear();
                self.status = "connecting…".into();
                let _ = self.jobs.send(Job::Connect(acc));
            }
            Err(e) => self.status = e,
        }
    }

    fn sign_out(&mut self) {
        if let Some(k) = keychain(&self.cfg) {
            let _ = k.delete_credential();
        }
        let _ = self.jobs.send(Job::SignOut);
        self.connected = false;
        self.form = false;
        self.status = "signed out; the password was removed from the keychain".into();
    }

    fn add(&mut self, address: &str) {
        let a = address.trim().to_lowercase();
        if !imap::valid_address(&a) {
            self.status = format!("not an email address: {a}");
            return;
        }
        self.cfg.removed.retain(|r| r != &a);
        if !self.cfg.added.iter().any(|(x, _)| *x == a) {
            self.cfg.added.push((a.clone(), String::new()));
        }
        save_config(&self.cfg);
        let _ = self.jobs.send(Job::Add(a, String::new()));
    }

    fn remove(&mut self, address: &str) {
        let a = address.to_lowercase();
        self.cfg.added.retain(|(x, _)| *x != a);
        if !self.cfg.removed.contains(&a) {
            self.cfg.removed.push(a.clone());
        }
        save_config(&self.cfg);
        let _ = self.jobs.send(Job::Remove(a));
    }

    /// Tell RogueIM which servers we talk to (diagnostics).
    fn announce(&mut self, p: &mut Plugin) {
        let now = (host_port(&self.cfg.imap, 993).0, host_port(&self.cfg.smtp, 465).0);
        if now != self.announced {
            if !now.0.is_empty() {
                p.network(&now.0, "mail (IMAP)");
            }
            if !now.1.is_empty() {
                p.network(&now.1, "mail (SMTP)");
            }
            self.announced = now;
        }
    }

    fn view(&self, p: &mut Plugin) {
        let mut items = vec![];
        if self.connected && !self.form {
            items.push(Item::text(&self.cfg.address, 0));
            let checked = if self.checked.is_empty() { String::new() } else { format!(" · checked {}", self.checked) };
            items.push(Item::dim(&format!("{} people · {} unread{checked}", self.people, self.unread), 1));
            items.push(Item::button("check", "check now", 2));
            items.push(Item::button("settings", "settings", 2));
            items.push(Item::button("signout", "sign out", 2));
        } else {
            items.push(Item::input("address", &self.cfg.address, "your email address", 0));
            items.push(Item::input("pass", "", "password (kept in the OS keychain)", 1));
            items.push(Item::input("name", &self.cfg.name, "your name, for the From line", 2));
            items.push(Item::input("imap", &self.cfg.imap, "IMAP server:port (993 = TLS)", 3));
            items.push(Item::input("smtp", &self.cfg.smtp, "SMTP server:port (465 TLS, 587 STARTTLS)", 4));
            items.push(Item::input("user", &self.cfg.user, "login, if not the address", 5));
            items.push(Item::button("connect", "connect", 6));
            if self.connected {
                items.push(Item::button("settings", "close", 6));
            }
            items.push(Item::dim("Gmail, iCloud and Yahoo need an app password. Outlook.com no longer accepts passwords from other apps.", 7));
        }
        if !self.status.is_empty() {
            items.push(Item::dim(&self.status, 8));
        }
        items.push(Item::dim("Not end-to-end encrypted: mail providers can read it. People you write to appear under \"mail\".", 9));
        p.view(items);
    }
}

fn main() {
    let Ok(mut p) = Plugin::connect("mail") else {
        eprintln!("rim-plugin-mail is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    p.send(&ToHost::Channel { label: "mail".into(), level: "D".into(), warning: WARNING.into(), add_hint: "an email address".into() });
    let (jobs, job_rx) = mpsc::channel();
    let (up_tx, up_rx) = mpsc::channel();
    imap::spawn(job_rx, up_tx);
    let mut st = State { cfg: load_config(), pass: String::new(), pending_pass: None, status: String::new(), connected: false, form: false, people: 0, unread: 0, checked: String::new(), announced: Default::default(), jobs };
    st.announce(&mut p);
    if keychain(&st.cfg).and_then(|k| k.get_password().ok()).is_some() {
        st.connect();
    }
    st.view(&mut p);
    loop {
        let mut dirty = false;
        while let Ok(up) = up_rx.try_recv() {
            dirty = true;
            match up {
                Up::Status(s) => st.status = s,
                Up::Connected(c) => {
                    st.connected = c;
                    if c {
                        st.form = false;
                        st.checked = now_hm();
                        if let Some(pass) = st.pending_pass.take() {
                            if let Some(k) = keychain(&st.cfg) {
                                if let Err(e) = k.set_password(&pass) {
                                    st.status = format!("could not store the password in the keychain: {e}");
                                }
                            }
                        }
                    } else {
                        st.people = 0;
                        st.unread = 0;
                    }
                }
                Up::Host(h) => {
                    if let ToHost::Contacts { list } = &h {
                        st.people = list.len();
                        st.unread = list.iter().map(|c| c.unread).sum();
                        st.checked = now_hm();
                    }
                    p.send(&h);
                }
            }
        }
        match p.recv(Duration::from_millis(150)) {
            Some(ToPlugin::Stop) => break,
            Some(ToPlugin::Click { id }) => {
                dirty = true;
                match id.as_str() {
                    "connect" => st.connect(),
                    "check" => {
                        st.status = "checking…".into();
                        let _ = st.jobs.send(Job::Refresh);
                    }
                    "settings" => st.form = !st.form,
                    "signout" => st.sign_out(),
                    _ => {}
                }
            }
            Some(ToPlugin::Input { id, value }) => {
                st.set(&id, value);
                if id == "pass" {
                    st.connect();
                }
                dirty = true;
            }
            Some(ToPlugin::Edit { id, value }) => {
                // Only the address changes other fields (the guessed servers).
                dirty = id == "address";
                st.set(&id, value);
            }
            Some(ToPlugin::Open { chat }) => {
                let _ = st.jobs.send(Job::Open(chat));
            }
            Some(ToPlugin::Close { chat }) => {
                let _ = st.jobs.send(Job::Close(chat));
            }
            Some(ToPlugin::Send { chat, text }) => {
                let _ = st.jobs.send(Job::Send { to: chat, text });
            }
            Some(ToPlugin::Add { address }) => {
                st.add(&address);
                dirty = true;
            }
            Some(ToPlugin::Remove { chat }) => st.remove(&chat),
            _ => {}
        }
        if dirty {
            st.announce(&mut p);
            st.view(&mut p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_follow_the_address() {
        assert_eq!(guess("me@gmail.com").0, "imap.gmail.com:993");
        assert_eq!(guess("me@Example.ORG"), ("imap.example.org:993".into(), "smtp.example.org:465".into()));
        assert_eq!(guess("nothing"), (String::new(), String::new()));
    }

    #[test]
    fn host_and_port() {
        assert_eq!(host_port("imap.example.org:143", 993), ("imap.example.org".into(), 143));
        assert_eq!(host_port("imap.example.org", 993), ("imap.example.org".into(), 993));
        assert_eq!(host_port(" 127.0.0.1:1143 ", 993), ("127.0.0.1".into(), 1143));
    }

    #[test]
    fn typed_servers_stop_following_the_address() {
        let (jobs, _rx) = mpsc::channel();
        let mut st = State { cfg: Config::default(), pass: String::new(), pending_pass: None, status: String::new(), connected: false, form: false, people: 0, unread: 0, checked: String::new(), announced: Default::default(), jobs };
        st.set("address", "me@example.org".into());
        assert_eq!(st.cfg.imap, "imap.example.org:993");
        st.set("imap", "mail.example.org:993".into());
        st.set("address", "me@example.net".into());
        assert_eq!(st.cfg.imap, "mail.example.org:993");
        assert_eq!(st.cfg.smtp, "smtp.example.net:465");
    }
}
