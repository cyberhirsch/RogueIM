//! The mailbox side: one IMAP connection on a worker thread. It finds the
//! people you write to (the "Sent" folder), counts their unread mail (INBOX),
//! loads a correspondent's mails as chat lines, checks for new mail every
//! minute and sends replies (SMTP, then a copy into "Sent").

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_imap::imap_proto::Address;
use async_imap::types::{Fetch, Flag, NameAttribute};
use async_imap::{Client, Session};
use futures::TryStreamExt;
use mail_parser::MessageParser;
use rim_plugin_sdk::{NetContact, NetLine, ToHost};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls;

use crate::smtp::{self, Reply};
use crate::text;

/// Sent mails scanned for correspondents, INBOX mails scanned for unread counts.
const SENT_SCAN: u32 = 500;
const INBOX_SCAN: u32 = 1000;
/// Mails per side (received / sent) loaded into a chat.
const CHAT_FETCH: usize = 30;
const CHAT_LINES: usize = 40;
/// Only the start of each mail is fetched (text comes first; attachments are named, not loaded).
const BODY_BYTES: u32 = 131_072;
const POLL: Duration = Duration::from_secs(60);
const RESCAN: Duration = Duration::from_secs(15 * 60);
const NET_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default)]
pub struct Account {
    pub address: String,
    pub name: String,
    pub user: String,
    pub pass: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// Added by hand / taken off the list (address, display name).
    pub added: Vec<(String, String)>,
    pub removed: Vec<String>,
}

impl Account {
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() { self.address.split('@').next().unwrap_or("").to_string() } else { self.name.trim().to_string() }
    }
}

pub enum Job {
    Connect(Account),
    Refresh,
    Open(String),
    Close(String),
    Send { to: String, text: String },
    Add(String, String),
    Remove(String),
    SignOut,
}

pub enum Up {
    /// Shown in the sidebar.
    Status(String),
    Connected(bool),
    Host(ToHost),
}

pub fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> Io for T {}
type Sess = Session<Box<dyn Io>>;

fn tls_connector() -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn tls(host: &str, tcp: TcpStream) -> Result<Box<dyn Io>, String> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: {e}"))?;
    let s = tls_connector().connect(name, tcp).await.map_err(|e| format!("TLS with {host}: {e}"))?;
    Ok(Box::new(s))
}

async fn timed<T, E: std::fmt::Display>(f: impl std::future::Future<Output = Result<T, E>>) -> Result<T, String> {
    match tokio::time::timeout(NET_TIMEOUT, f).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("the mail server did not answer in time".into()),
    }
}

/// Log in: TLS on 993, STARTTLS elsewhere; plain only to this computer.
async fn connect(acc: &Account) -> Result<Sess, String> {
    let (host, port) = (acc.imap_host.as_str(), acc.imap_port);
    let tcp = timed(TcpStream::connect((host, port))).await.map_err(|e| format!("{host}:{port}: {e}"))?;
    let stream: Box<dyn Io> = if port == 993 {
        tls(host, tcp).await?
    } else if is_loopback(host) {
        Box::new(tcp)
    } else {
        let mut c = Client::new(tcp);
        timed(async { c.read_response().await }).await?;
        timed(c.run_command_and_check_ok("STARTTLS", None)).await.map_err(|e| format!("{host} does not offer STARTTLS: {e}"))?;
        tls(host, c.into_inner()).await?
    };
    let mut client = Client::new(stream);
    if port == 993 || is_loopback(host) {
        // There is no greeting after STARTTLS, only on a fresh connection.
        timed(async { client.read_response().await }).await?;
    }
    match tokio::time::timeout(NET_TIMEOUT, client.login(&acc.user, &acc.pass)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err((e, _))) => Err(format!("login failed: {e}")),
        Err(_) => Err("the mail server did not answer in time".into()),
    }
}

async fn fetch_seq(s: &mut Sess, set: &str, query: &str) -> Result<Vec<Fetch>, String> {
    timed(async { s.fetch(set, query).await?.try_collect::<Vec<_>>().await }).await
}

async fn fetch_uid(s: &mut Sess, set: &str, query: &str) -> Result<Vec<Fetch>, String> {
    timed(async { s.uid_fetch(set, query).await?.try_collect::<Vec<_>>().await }).await
}

/// The folder with sent mail: marked \Sent, else a usual name.
async fn find_sent(s: &mut Sess) -> Result<Option<String>, String> {
    let names = timed(async { s.list(Some(""), Some("*")).await?.try_collect::<Vec<_>>().await }).await?;
    if let Some(n) = names.iter().find(|n| n.attributes().iter().any(|a| matches!(a, NameAttribute::Sent))) {
        return Ok(Some(n.name().to_string()));
    }
    const USUAL: [&str; 9] = ["Sent", "Sent Items", "Sent Messages", "Sent Mail", "INBOX.Sent", "INBOX/Sent", "Gesendet", "Gesendete Objekte", "Gesendete Elemente"];
    Ok(USUAL.iter().find_map(|u| names.iter().find(|n| n.name().eq_ignore_ascii_case(u))).map(|n| n.name().to_string()))
}

fn addr_of(a: &Address<'_>) -> Option<String> {
    let (m, h) = (a.mailbox.as_ref()?, a.host.as_ref()?);
    Some(format!("{}@{}", String::from_utf8_lossy(m), String::from_utf8_lossy(h)).to_lowercase())
}

fn name_of(a: &Address<'_>) -> String {
    a.name.as_ref().map(|n| text::decode_words(n)).unwrap_or_default()
}

fn seen(f: &Fetch) -> bool {
    f.flags().any(|fl| matches!(fl, Flag::Seen))
}

/// Safe to put into a quoted IMAP search string.
pub fn valid_address(a: &str) -> bool {
    a.len() < 255 && a.split_once('@').is_some_and(|(u, d)| !u.is_empty() && d.contains('.')) && !a.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\' || c.is_control())
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Default)]
struct Person {
    name: String,
    /// You wrote to them (from "Sent").
    wrote: bool,
    added: bool,
    unread: u32,
}

#[derive(Default)]
struct Worker {
    acc: Option<Account>,
    session: Option<Sess>,
    sent: Option<String>,
    people: HashMap<String, Person>,
    uid_validity: Option<u32>,
    last_uid: u32,
    open: HashSet<String>,
    lines: HashMap<String, Vec<NetLine>>,
    reply: HashMap<String, Reply>,
    last_scan: Option<Instant>,
    pending: u32,
    /// Message-ID of each line sent from here, to spot its copy in "Sent".
    sent_ids: HashMap<String, String>,
}

pub fn spawn(rx: Receiver<Job>, tx: Sender<Up>) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let mut w = Worker::default();
        // Jobs don't postpone the next look for new mail.
        let mut due = Instant::now() + POLL;
        loop {
            let job = match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => {
                    due = Instant::now() + POLL;
                    None
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            rt.block_on(w.handle(job, &tx));
        }
    });
}

impl Worker {
    async fn handle(&mut self, job: Option<Job>, tx: &Sender<Up>) {
        let r = match job {
            Some(Job::Connect(acc)) => {
                self.acc = Some(acc);
                self.session = None;
                self.people.clear();
                let _ = tx.send(Up::Status("connecting…".into()));
                self.scan(tx).await
            }
            Some(Job::Refresh) => self.scan(tx).await,
            Some(Job::Open(addr)) => {
                self.open.insert(addr.clone());
                self.load(&addr, tx).await
            }
            Some(Job::Close(addr)) => {
                self.open.remove(&addr);
                self.lines.remove(&addr);
                Ok(())
            }
            Some(Job::Send { to, text }) => self.send(&to, &text, tx).await,
            Some(Job::Add(addr, name)) => {
                if let Some(acc) = self.acc.as_mut() {
                    acc.removed.retain(|r| r != &addr);
                    if !acc.added.iter().any(|(a, _)| *a == addr) {
                        acc.added.push((addr.clone(), name.clone()));
                    }
                }
                let p = self.people.entry(addr).or_default();
                p.added = true;
                if p.name.is_empty() {
                    p.name = name;
                }
                self.emit_people(tx);
                Ok(())
            }
            Some(Job::Remove(addr)) => {
                if let Some(acc) = self.acc.as_mut() {
                    acc.removed.push(addr.clone());
                }
                self.people.remove(&addr);
                self.emit_people(tx);
                Ok(())
            }
            Some(Job::SignOut) => {
                if let Some(s) = self.session.as_mut() {
                    let _ = timed(s.logout()).await;
                }
                *self = Worker::default();
                let _ = tx.send(Up::Connected(false));
                let _ = tx.send(Up::Host(ToHost::Contacts { list: vec![] }));
                Ok(())
            }
            None => {
                if self.acc.is_none() {
                    return;
                }
                if self.last_scan.is_none_or(|t| t.elapsed() > RESCAN) {
                    self.scan(tx).await
                } else {
                    self.poll(tx).await
                }
            }
        };
        if let Err(e) = r {
            // Reconnect next time.
            self.session = None;
            let _ = tx.send(Up::Status(e));
        }
    }

    async fn session(&mut self) -> Result<&mut Sess, String> {
        if self.session.is_none() {
            let acc = self.acc.as_ref().ok_or("not signed in")?;
            let mut s = connect(acc).await?;
            self.sent = find_sent(&mut s).await?;
            self.session = Some(s);
        }
        Ok(self.session.as_mut().expect("session"))
    }

    fn me(&self) -> String {
        self.acc.as_ref().map(|a| a.address.to_lowercase()).unwrap_or_default()
    }

    /// Everyone you wrote to, with their unread mail.
    async fn scan(&mut self, tx: &Sender<Up>) -> Result<(), String> {
        let me = self.me();
        let acc = self.acc.clone().ok_or("not signed in")?;
        self.session().await?;
        let sent = self.sent.clone();
        let s = self.session.as_mut().expect("session");
        let mut found: HashMap<String, Person> = HashMap::new();
        if let Some(sent) = &sent {
            let n = timed(s.examine(sent)).await?.exists;
            if n > 0 {
                for f in fetch_seq(s, &format!("{}:{n}", n.saturating_sub(SENT_SCAN - 1).max(1)), "(UID ENVELOPE)").await? {
                    let Some(env) = f.envelope() else { continue };
                    for a in env.to.iter().flatten().chain(env.cc.iter().flatten()) {
                        let Some(addr) = addr_of(a) else { continue };
                        let p = found.entry(addr).or_default();
                        p.wrote = true;
                        let name = name_of(a);
                        if p.name.is_empty() && !name.is_empty() {
                            p.name = name;
                        }
                    }
                }
            }
        }
        for (addr, name) in &acc.added {
            let p = found.entry(addr.clone()).or_default();
            p.added = true;
            if p.name.is_empty() {
                p.name = name.clone();
            }
        }
        found.remove(&me);
        for r in &acc.removed {
            found.remove(r);
        }
        let mb = timed(s.select("INBOX")).await?;
        self.uid_validity = mb.uid_validity;
        let n = mb.exists;
        let mut last = 0;
        if n > 0 {
            for f in fetch_seq(s, &format!("{}:{n}", n.saturating_sub(INBOX_SCAN - 1).max(1)), "(UID FLAGS ENVELOPE)").await? {
                last = last.max(f.uid.unwrap_or(0));
                let Some(from) = f.envelope().and_then(|e| e.from.as_ref()).and_then(|v| v.first()) else { continue };
                let Some(addr) = addr_of(from) else { continue };
                if let Some(p) = found.get_mut(&addr) {
                    if !seen(&f) {
                        p.unread += 1;
                    }
                    if p.name.is_empty() {
                        p.name = name_of(from);
                    }
                }
            }
        }
        self.last_uid = last.max(mb.uid_next.unwrap_or(1).saturating_sub(1));
        self.people = found;
        self.last_scan = Some(Instant::now());
        let _ = tx.send(Up::Connected(true));
        let _ = tx.send(Up::Status(String::new()));
        self.emit_people(tx);
        // Chats left open stay current.
        for addr in self.open.clone() {
            self.load(&addr, tx).await?;
        }
        Ok(())
    }

    /// New mail since the last look.
    async fn poll(&mut self, tx: &Sender<Up>) -> Result<(), String> {
        let validity = self.uid_validity;
        let last = self.last_uid;
        let s = self.session().await?;
        let mb = timed(s.select("INBOX")).await?;
        if mb.uid_validity != validity {
            return self.scan(tx).await;
        }
        if mb.uid_next.is_some_and(|n| n <= last + 1) {
            let _ = tx.send(Up::Status(String::new()));
            return Ok(());
        }
        let fetched = fetch_uid(s, &format!("{}:*", last + 1), "(UID FLAGS ENVELOPE)").await?;
        let mut news: Vec<(String, String)> = vec![];
        for f in fetched {
            let uid = f.uid.unwrap_or(0);
            if uid <= last {
                continue;
            }
            self.last_uid = self.last_uid.max(uid);
            let Some(from) = f.envelope().and_then(|e| e.from.as_ref()).and_then(|v| v.first()) else { continue };
            let Some(addr) = addr_of(from) else { continue };
            if let Some(p) = self.people.get_mut(&addr) {
                if !seen(&f) {
                    p.unread += 1;
                    news.push((addr.clone(), if p.name.is_empty() { addr.clone() } else { p.name.clone() }));
                }
            }
        }
        if !news.is_empty() {
            self.emit_people(tx);
        }
        for (addr, name) in news {
            if self.open.contains(&addr) {
                self.load(&addr, tx).await?;
            }
            let _ = tx.send(Up::Host(ToHost::Incoming { chat: addr, name }));
        }
        let _ = tx.send(Up::Status(String::new()));
        Ok(())
    }

    /// A correspondent's recent mails as chat lines; their unread mail becomes read.
    async fn load(&mut self, addr: &str, tx: &Sender<Up>) -> Result<(), String> {
        if !valid_address(addr) {
            return Ok(());
        }
        let own_name = self.acc.as_ref().map(|a| a.display_name()).unwrap_or_default();
        let sent = self.sent.clone();
        let s = self.session().await?;
        let parser = MessageParser::default();
        let query = format!("(UID FLAGS BODY.PEEK[]<0.{BODY_BYTES}>)");
        // (line, subject, Message-ID, References)
        let mut got: Vec<(NetLine, String, String, String)> = vec![];

        timed(s.select("INBOX")).await?;
        let mut uids: Vec<u32> = timed(s.uid_search(format!("FROM \"{addr}\""))).await?.into_iter().collect();
        uids.sort_unstable();
        let uids: Vec<u32> = uids.into_iter().rev().take(CHAT_FETCH).collect();
        let mut unseen = vec![];
        if !uids.is_empty() {
            for f in fetch_uid(s, &uid_set(&uids), &query).await? {
                let Some(msg) = f.body().and_then(|b| parser.parse(b)) else { continue };
                let from = msg.from().and_then(|a| a.first()).and_then(|a| a.address()).map(str::to_lowercase).unwrap_or_default();
                if from != addr {
                    continue;
                }
                if !seen(&f) {
                    unseen.push(f.uid.unwrap_or(0));
                }
                let who = msg.from().and_then(|a| a.first()).and_then(|a| a.name()).map(str::to_string).unwrap_or_else(|| addr.to_string());
                got.push(line(&msg, format!("i:{}", f.uid.unwrap_or(0)), false, who));
            }
        }
        if !unseen.is_empty() {
            timed(async { s.uid_store(uid_set(&unseen), "+FLAGS.SILENT (\\Seen)").await?.try_collect::<Vec<_>>().await }).await?;
        }
        if let Some(sent) = &sent {
            timed(s.select(sent)).await?;
            let mut uids: Vec<u32> = timed(s.uid_search(format!("OR TO \"{addr}\" CC \"{addr}\""))).await?.into_iter().collect();
            uids.sort_unstable();
            let uids: Vec<u32> = uids.into_iter().rev().take(CHAT_FETCH).collect();
            if !uids.is_empty() {
                for f in fetch_uid(s, &uid_set(&uids), &query).await? {
                    let Some(msg) = f.body().and_then(|b| parser.parse(b)) else { continue };
                    let to_them = msg.to().into_iter().chain(msg.cc()).flat_map(|a| a.iter()).any(|a| a.address().is_some_and(|x| x.eq_ignore_ascii_case(addr)));
                    if !to_them {
                        continue;
                    }
                    got.push(line(&msg, format!("s:{}", f.uid.unwrap_or(0)), true, own_name.clone()));
                }
            }
        }
        let filed: HashSet<String> = got.iter().filter(|g| g.0.from_me).map(|g| g.2.clone()).collect();
        got.sort_by_key(|g| g.0.ts);
        let skip = got.len().saturating_sub(CHAT_LINES);
        let got: Vec<_> = got.into_iter().skip(skip).collect();
        // A line starts with its subject whenever the subject changes.
        let mut lines = vec![];
        let mut prev = String::new();
        for (mut l, subject, _, _) in got.iter().cloned() {
            let base = text::base_subject(&subject);
            if !base.is_empty() && !base.eq_ignore_ascii_case(&prev) {
                l.text = format!("subject: {base}\n{}", l.text);
            }
            prev = base;
            lines.push(l);
        }
        if let Some((_, subject, id, refs)) = got.last() {
            self.reply.insert(addr.to_string(), Reply { subject: subject.clone(), message_id: id.clone(), references: refs.clone() });
        }
        if let Some(p) = self.people.get_mut(addr) {
            if p.unread > 0 {
                p.unread = 0;
                self.emit_people(tx);
            }
        }
        // Keep lines still being sent, and sent ones until their copy is in "Sent".
        let pending: Vec<NetLine> = self
            .lines
            .get(addr)
            .map(|v| v.iter().filter(|l| l.id.starts_with("p:") && (l.state != "sent" || !self.sent_ids.get(&l.id).is_some_and(|m| filed.contains(m)))).cloned().collect())
            .unwrap_or_default();
        lines.extend(pending);
        self.lines.insert(addr.to_string(), lines.clone());
        let _ = tx.send(Up::Host(ToHost::Lines { chat: addr.to_string(), lines }));
        Ok(())
    }

    async fn send(&mut self, to: &str, text_: &str, tx: &Sender<Up>) -> Result<(), String> {
        let acc = self.acc.clone().ok_or("not signed in")?;
        let to = to.to_lowercase();
        let name = self.people.get(&to).map(|p| p.name.clone()).unwrap_or_default();
        self.pending += 1;
        let pid = format!("p:{}", self.pending);
        let mut l = NetLine { id: pid.clone(), ts: now(), from_me: true, who: acc.display_name(), text: text_.to_string(), state: "sending".into() };
        self.push_line(&to, l.clone(), tx);
        let built = smtp::build(&acc, &to, &name, text_, self.reply.get(&to));
        if let Ok((_, id)) = &built {
            self.sent_ids.insert(pid.clone(), id.clone());
        }
        let result = match &built {
            Ok((msg, _)) => smtp::send(&acc, msg).await,
            Err(e) => Err(e.clone()),
        };
        match result {
            Ok(()) => {
                l.state = "sent".into();
                self.push_line(&to, l, tx);
                if let Ok((msg, _)) = &built {
                    if !smtp::server_saves_sent(&acc.smtp_host) {
                        let raw = msg.formatted();
                        self.session().await?;
                        if self.sent.is_none() {
                            // A new mailbox may have no "Sent" yet; mail programs create it.
                            let s = self.session.as_mut().expect("session");
                            timed(s.create("Sent")).await.map_err(|e| format!("sent, but no copy kept: creating \"Sent\" failed: {e}"))?;
                            self.sent = Some("Sent".into());
                        }
                        let sent = self.sent.clone().expect("sent folder");
                        let s = self.session.as_mut().expect("session");
                        timed(s.append(&sent, Some("(\\Seen)"), None, raw)).await.map_err(|e| format!("sent, but not saved to {sent}: {e}"))?;
                    }
                }
                let p = self.people.entry(to.clone()).or_default();
                let new = !p.wrote && !p.added;
                p.wrote = true;
                if new {
                    self.emit_people(tx);
                }
                // The copy in "Sent" replaces the pending line.
                self.load(&to, tx).await
            }
            Err(e) => {
                l.state = "failed".into();
                l.text = format!("{text_}\n[not sent: {e}]");
                self.push_line(&to, l, tx);
                Err(e)
            }
        }
    }

    fn push_line(&mut self, chat: &str, l: NetLine, tx: &Sender<Up>) {
        let v = self.lines.entry(chat.to_string()).or_default();
        match v.iter_mut().find(|x| x.id == l.id) {
            Some(x) => *x = l,
            None => v.push(l),
        }
        let _ = tx.send(Up::Host(ToHost::Lines { chat: chat.to_string(), lines: v.clone() }));
    }

    fn emit_people(&self, tx: &Sender<Up>) {
        let mut list: Vec<NetContact> = self
            .people
            .iter()
            .filter(|(_, p)| p.wrote || p.added)
            .map(|(addr, p)| NetContact {
                id: addr.clone(),
                name: if p.name.is_empty() { addr.split('@').next().unwrap_or(addr).to_string() } else { p.name.clone() },
                status: String::new(),
                info: addr.clone(),
                unread: p.unread,
            })
            .collect();
        list.sort_by_key(|c| c.name.to_lowercase());
        let _ = tx.send(Up::Host(ToHost::Contacts { list }));
    }
}

/// One mail as a chat line, with what a reply needs (subject, Message-ID, References).
fn line(msg: &mail_parser::Message<'_>, id: String, mine: bool, who: String) -> (NetLine, String, String, String) {
    let ts = msg.date().map(|d| d.to_timestamp()).unwrap_or(0);
    let subject = msg.subject().unwrap_or("").to_string();
    let mid = msg.message_id().map(|m| format!("<{m}>")).unwrap_or_default();
    let refs = msg.references().as_text_list().map(|v| v.iter().map(|r| format!("<{r}>")).collect::<Vec<_>>().join(" ")).or_else(|| msg.references().as_text().map(|r| format!("<{r}>"))).unwrap_or_default();
    (NetLine { id, ts, from_me: mine, who, text: text::chat_text(msg), state: String::new() }, subject, mid, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_for_searches() {
        assert!(valid_address("anna.b+x@example.org"));
        assert!(!valid_address("anna\"@example.org"));
        assert!(!valid_address("no at sign"));
        assert!(!valid_address("a@b"));
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("imap.example.org"));
    }
}
