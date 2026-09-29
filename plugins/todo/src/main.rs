//! Todo plugin (PRD GD-6..8, GD-11): a todo.txt file or CalDAV VTODOs
//! (Nextcloud, Radicale, Baïkal, iCloud with an app password …). The CalDAV
//! password lives in the OS keychain. "Add as task" on a chat message lands here.
//! No network traffic unless CalDAV is configured.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rim_plugin_sdk::{Item, Plugin, ToHost, ToPlugin};
use serde_json::json;

#[derive(Clone)]
struct Task {
    id: String,
    text: String,
    done: bool,
    /// CalDAV: resource href and raw calendar data.
    href: String,
    ics: String,
    etag: String,
}

#[derive(Default)]
struct Config {
    backend: String,
    path: String,
    url: String,
    user: String,
}

fn data_dir() -> PathBuf {
    std::env::var_os("RIM_PLUGIN_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn load_config() -> Config {
    let v: serde_json::Value = std::fs::read(data_dir().join("todo.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Config { backend: s("backend"), path: s("path"), url: s("url"), user: s("user") }
}

fn save_config(c: &Config) {
    let _ = std::fs::create_dir_all(data_dir());
    let _ = std::fs::write(data_dir().join("todo.json"), json!({"backend": c.backend, "path": c.path, "url": c.url, "user": c.user}).to_string());
}

fn keychain(c: &Config) -> Option<keyring::Entry> {
    keyring::Entry::new("RogueIM todo (CalDAV)", &format!("{}@{}", c.user, c.url)).ok()
}

// ---------------------------------------------------------------- todo.txt

fn txt_load(path: &str) -> Vec<Task> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            let done = l.starts_with("x ");
            let text = if done { l[2..].trim().to_string() } else { l.trim().to_string() };
            Task { id: i.to_string(), text, done, href: String::new(), ics: String::new(), etag: String::new() }
        })
        .collect()
}

fn txt_write(path: &str, f: impl FnOnce(&mut Vec<String>)) -> Result<(), String> {
    let mut lines: Vec<String> = std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_string).collect();
    f(&mut lines);
    std::fs::write(path, lines.join("\n") + "\n").map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- CalDAV

fn unescape(s: &str) -> String {
    s.replace("&#13;", "\r").replace("&#xD;", "\r").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn ics_field(ics: &str, name: &str) -> String {
    // Unfold continuation lines first.
    let unfolded = ics.replace("\r\n ", "").replace("\n ", "");
    unfolded
        .lines()
        .find_map(|l| {
            let l = l.trim_end_matches('\r');
            let (k, v) = l.split_once(':')?;
            (k.split(';').next()? == name).then(|| v.replace("\\,", ",").replace("\\;", ";").replace("\\n", " "))
        })
        .unwrap_or_default()
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Vec<&'a str> {
    let mut out = vec![];
    let mut rest = s;
    while let Some(i) = rest.find(start) {
        let after = &rest[i + start.len()..];
        let Some(j) = after.find(end) else { break };
        out.push(&after[..j]);
        rest = &after[j + end.len()..];
    }
    out
}

struct Dav {
    agent: ureq::Agent,
    auth: String,
    url: String,
}

impl Dav {
    fn new(c: &Config, pass: &str) -> Dav {
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().new_agent();
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.user, pass)));
        let url = if c.url.ends_with('/') { c.url.clone() } else { format!("{}/", c.url) };
        Dav { agent, auth, url }
    }

    fn origin(&self) -> String {
        let after = self.url.find("://").map(|i| i + 3).unwrap_or(0);
        let end = self.url[after..].find('/').map(|j| after + j).unwrap_or(self.url.len());
        self.url[..end].to_string()
    }

    fn list(&self) -> Result<Vec<Task>, String> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter></c:calendar-query>"#;
        let req = ureq::http::Request::builder()
            .method(ureq::http::Method::from_bytes(b"REPORT").unwrap())
            .uri(&self.url)
            .header("Authorization", &self.auth)
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .map_err(|e| e.to_string())?;
        let mut res = self.agent.run(req).map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if status == 401 {
            return Err("CalDAV: wrong user or password".into());
        }
        if status >= 300 {
            return Err(format!("CalDAV: HTTP {status}"));
        }
        let mut tasks = vec![];
        // Namespace prefixes differ between servers; match on local names.
        for resp in text.split("response>").filter(|r| r.contains("VTODO")) {
            let href = between(resp, "href>", "<").first().map(|s| s.trim().to_string()).unwrap_or_default();
            let etag = between(resp, "getetag>", "<").first().map(|s| unescape(s.trim())).unwrap_or_default();
            let ics = between(resp, "calendar-data>", "<").first().map(|s| unescape(s)).unwrap_or_default();
            if ics.is_empty() {
                continue;
            }
            let summary = ics_field(&ics, "SUMMARY");
            let status = ics_field(&ics, "STATUS");
            tasks.push(Task { id: href.clone(), text: summary, done: status == "COMPLETED", href, ics, etag });
        }
        tasks.sort_by_key(|t| (t.done, t.text.to_lowercase()));
        Ok(tasks)
    }

    fn put(&self, href: &str, ics: &str, etag: Option<&str>) -> Result<(), String> {
        let url = if href.starts_with("http") { href.to_string() } else if href.starts_with('/') { format!("{}{href}", self.origin()) } else { format!("{}{href}", self.url) };
        let mut b = ureq::http::Request::builder().method("PUT").uri(url).header("Authorization", &self.auth).header("Content-Type", "text/calendar; charset=utf-8");
        if let Some(e) = etag.filter(|e| !e.is_empty()) {
            b = b.header("If-Match", e);
        }
        let res = self.agent.run(b.body(ics.to_string()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let s = res.status().as_u16();
        if s >= 300 {
            return Err(format!("CalDAV: HTTP {s}"));
        }
        Ok(())
    }

    fn add(&self, text: &str) -> Result<(), String> {
        let uid = format!("rogueim-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
        let stamp = stamp();
        let esc = text.replace('\\', "\\\\").replace(',', "\\,").replace(';', "\\;").replace('\n', "\\n");
        let ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//RogueIM//todo//EN\r\nBEGIN:VTODO\r\nUID:{uid}\r\nDTSTAMP:{stamp}\r\nCREATED:{stamp}\r\nSUMMARY:{esc}\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
        self.put(&format!("{uid}.ics"), &ics, None)
    }

    fn set_done(&self, t: &Task, done: bool) -> Result<(), String> {
        let mut out = vec![];
        let mut in_todo = false;
        for l in t.ics.replace("\r\n", "\n").lines() {
            if l.starts_with("BEGIN:VTODO") {
                in_todo = true;
            }
            if in_todo && (l.starts_with("STATUS") || l.starts_with("COMPLETED") || l.starts_with("PERCENT-COMPLETE")) {
                continue;
            }
            if l.starts_with("END:VTODO") {
                if done {
                    out.push("STATUS:COMPLETED".to_string());
                    out.push(format!("COMPLETED:{}", stamp()));
                    out.push("PERCENT-COMPLETE:100".to_string());
                } else {
                    out.push("STATUS:NEEDS-ACTION".to_string());
                }
                in_todo = false;
            }
            out.push(l.to_string());
        }
        self.put(&t.href, &(out.join("\r\n") + "\r\n"), Some(&t.etag))
    }
}

fn stamp() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

// ---------------------------------------------------------------- plugin

struct App {
    cfg: Config,
    tasks: Vec<Task>,
    status: String,
    new_text: String,
    pass_input: String,
    mtime: Option<SystemTime>,
    last_sync: u64,
}

impl App {
    fn dav(&self) -> Option<Dav> {
        let pass = keychain(&self.cfg)?.get_password().ok()?;
        Some(Dav::new(&self.cfg, &pass))
    }

    fn reload(&mut self) {
        match self.cfg.backend.as_str() {
            "txt" if !self.cfg.path.is_empty() => {
                self.tasks = txt_load(&self.cfg.path);
                self.mtime = std::fs::metadata(&self.cfg.path).and_then(|m| m.modified()).ok();
                self.status = format!("{} open", self.tasks.iter().filter(|t| !t.done).count());
            }
            "caldav" => match self.dav().map(|d| d.list()) {
                Some(Ok(t)) => {
                    self.tasks = t;
                    self.status = format!("{} open · synced", self.tasks.iter().filter(|t| !t.done).count());
                }
                Some(Err(e)) => self.status = e,
                None => self.status = "enter the CalDAV password".into(),
            },
            _ => {}
        }
        self.last_sync = now();
    }

    fn add(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let r = match self.cfg.backend.as_str() {
            "txt" => txt_write(&self.cfg.path, |l| l.push(text.to_string())),
            "caldav" => self.dav().ok_or("no password".to_string()).and_then(|d| d.add(text)),
            _ => Err("choose todo.txt or CalDAV first".into()),
        };
        if let Err(e) = r {
            self.status = e;
        }
        self.reload();
    }

    fn toggle(&mut self, id: &str, done: bool) {
        let Some(t) = self.tasks.iter().find(|t| t.id == id).cloned() else { return };
        let r = match self.cfg.backend.as_str() {
            "txt" => {
                let i: usize = id.parse().unwrap_or(usize::MAX);
                txt_write(&self.cfg.path, |l| {
                    if let Some(line) = l.get_mut(i) {
                        let body = line.strip_prefix("x ").unwrap_or(line).to_string();
                        *line = if done { format!("x {body}") } else { body };
                    }
                })
            }
            "caldav" => self.dav().ok_or("no password".to_string()).and_then(|d| d.set_done(&t, done)),
            _ => Ok(()),
        };
        if let Err(e) = r {
            self.status = e;
        }
        self.reload();
    }

    fn view(&self, p: &mut Plugin) {
        let mut items = vec![];
        let b = self.cfg.backend.as_str();
        items.push(Item::button("use-txt", if b == "txt" { "[todo.txt]" } else { "todo.txt" }, 0));
        items.push(Item::button("use-caldav", if b == "caldav" { "[CalDAV]" } else { "CalDAV" }, 0));
        match b {
            "txt" => {
                items.push(Item::input("path", &self.cfg.path, "path to todo.txt", 1));
            }
            "caldav" => {
                items.push(Item::input("url", &self.cfg.url, "calendar URL (…/calendars/user/tasks/)", 1));
                items.push(Item::input("user", &self.cfg.user, "user", 2));
                items.push(Item::input("pass", "", "password (kept in the OS keychain)", 2));
                items.push(Item::button("sync", "sync", 3));
            }
            _ => items.push(Item::dim("Pick a list: a local todo.txt file, or tasks on a CalDAV server.", 1)),
        }
        let mut row = 10;
        for t in self.tasks.iter().filter(|t| !t.done).chain(self.tasks.iter().filter(|t| t.done).take(5)) {
            items.push(Item::check(&format!("t:{}", t.id), &t.text, t.done, row));
            row += 1;
        }
        if !b.is_empty() {
            items.push(Item::input("new", &self.new_text, "new task — Enter to add", row + 1));
        }
        if !self.status.is_empty() {
            items.push(Item::dim(&self.status, row + 2));
        }
        p.view(items);
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn main() {
    let Ok(mut p) = Plugin::connect("todo") else {
        eprintln!("rim-plugin-todo is started by RogueIM (settings > plugins).");
        std::process::exit(2);
    };
    let mut a = App { cfg: load_config(), tasks: vec![], status: String::new(), new_text: String::new(), pass_input: String::new(), mtime: None, last_sync: 0 };
    a.reload();
    a.view(&mut p);
    loop {
        match p.recv(Duration::from_secs(2)) {
            Some(ToPlugin::Stop) => break,
            Some(ToPlugin::Click { id }) => match id.as_str() {
                "use-txt" => {
                    a.cfg.backend = "txt".into();
                    if a.cfg.path.is_empty() {
                        a.cfg.path = data_dir().join("todo.txt").to_string_lossy().to_string();
                    }
                    save_config(&a.cfg);
                    a.reload();
                }
                "use-caldav" => {
                    a.cfg.backend = "caldav".into();
                    save_config(&a.cfg);
                    a.reload();
                }
                "sync" => {
                    if !a.pass_input.is_empty() {
                        if let Some(k) = keychain(&a.cfg) {
                            let _ = k.set_password(&a.pass_input);
                        }
                        a.pass_input.clear();
                    }
                    a.reload();
                }
                _ => {}
            },
            Some(ToPlugin::Input { id, value }) => match id.as_str() {
                "path" => {
                    a.cfg.path = value;
                    save_config(&a.cfg);
                    a.reload();
                }
                "url" => {
                    a.cfg.url = value.trim().to_string();
                    save_config(&a.cfg);
                }
                "user" => {
                    a.cfg.user = value.trim().to_string();
                    save_config(&a.cfg);
                }
                "pass" => a.pass_input = value,
                "new" => a.add(&value),
                _ => {}
            },
            Some(ToPlugin::Check { id, checked }) => {
                if let Some(t) = id.strip_prefix("t:") {
                    a.toggle(t, checked);
                }
            }
            Some(ToPlugin::Task { text, from }) => {
                a.add(&format!("{text} (from {from})"));
                p.send(&ToHost::Notify { title: "Todo".into(), body: "Task added.".into() });
            }
            _ => {
                // Pick up edits made outside (todo.txt) and re-sync CalDAV every 5 minutes.
                let changed = a.cfg.backend == "txt" && std::fs::metadata(&a.cfg.path).and_then(|m| m.modified()).ok() != a.mtime;
                if changed || (a.cfg.backend == "caldav" && now() - a.last_sync > 300) {
                    a.reload();
                }
            }
        }
        a.view(&mut p);
    }
}
