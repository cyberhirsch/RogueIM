//! Start the mail plugin like RogueIM does and talk to it.
//!
//! The end-to-end tests need a local test mail server with three accounts,
//! for example GreenMail (https://greenmail-mail-test.github.io/greenmail/):
//!
//!   java -Dgreenmail.setup.test.smtp -Dgreenmail.setup.test.imap -Dgreenmail.users.login=email \
//!        -Dgreenmail.users=me:secret@example.org,anna:secret@example.org,new:secret@example.org \
//!        -jar greenmail-standalone.jar
//!   cargo test -p rim-plugin-mail -- --ignored --test-threads=1
//!
//! IMAP on 127.0.0.1:3143 and SMTP on 127.0.0.1:3025; other ports with
//! RIM_MAIL_TEST_IMAP / RIM_MAIL_TEST_SMTP. Use a fresh server for each run.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime};

use futures::TryStreamExt;
use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
use mail_parser::MessageParser;
use rim_plugin_sdk::{Item, NetContact, NetLine, ToHost, ToPlugin};

struct Host {
    child: std::process::Child,
    send: Box<dyn Write + Send>,
    rx: Receiver<ToHost>,
}

impl Host {
    fn start(name: &str, data: &Path) -> Host {
        let sock = format!("rogueim-test-mail-{name}-{}", std::process::id());
        let listener = ListenerOptions::new().name(sock.clone().to_ns_name::<GenericNamespaced>().unwrap()).create_sync().unwrap();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_rim-plugin-mail")).env("RIM_PLUGIN_SOCKET", &sock).env("RIM_PLUGIN_DATA", data).spawn().unwrap();
        let conn = listener.incoming().next().unwrap().unwrap();
        let (recv, send) = conn.split();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(recv).lines().map_while(Result::ok) {
                eprintln!("plugin: {l}");
                if let Ok(m) = serde_json::from_str::<ToHost>(&l) {
                    if tx.send(m).is_err() {
                        break;
                    }
                }
            }
        });
        Host { child, send: Box::new(send), rx }
    }

    fn tell(&mut self, m: ToPlugin) {
        let s = serde_json::to_string(&m).unwrap();
        self.send.write_all(format!("{s}\n").as_bytes()).unwrap();
        self.send.flush().unwrap();
    }

    /// The first message `f` accepts within `secs` seconds; others are skipped.
    fn wait<T>(&self, secs: u64, what: &str, mut f: impl FnMut(&ToHost) -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let m = self.rx.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap_or_else(|_| panic!("no {what} within {secs} s"));
            if let Some(t) = f(&m) {
                return t;
            }
        }
    }

    fn contacts(&self, what: &str, ok: impl Fn(&[NetContact]) -> bool) -> Vec<NetContact> {
        self.wait(30, what, |m| match m {
            ToHost::Contacts { list } if ok(list) => Some(list.clone()),
            _ => None,
        })
    }

    fn lines(&self, chat: &str, what: &str, ok: impl Fn(&[NetLine]) -> bool) -> Vec<NetLine> {
        self.wait(30, what, |m| match m {
            ToHost::Lines { chat: c, lines } if c == chat && ok(lines) => Some(lines.clone()),
            _ => None,
        })
    }

    fn sign_in(&mut self, address: &str) {
        let (imap, smtp) = ports();
        for (id, value) in [("address", address.to_string()), ("imap", format!("127.0.0.1:{imap}")), ("smtp", format!("127.0.0.1:{smtp}")), ("name", "Me".into())] {
            self.tell(ToPlugin::Edit { id: id.into(), value });
        }
        self.tell(ToPlugin::Input { id: "pass".into(), value: "secret".into() });
    }

    fn stop(mut self) {
        self.tell(ToPlugin::Stop);
        let _ = self.child.wait();
    }
}

fn data_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-mail-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ports() -> (u16, u16) {
    let get = |k: &str, d: u16| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    (get("RIM_MAIL_TEST_IMAP", 3143), get("RIM_MAIL_TEST_SMTP", 3025))
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

async fn imap(user: &str) -> async_imap::Session<tokio::net::TcpStream> {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", ports().0)).await.expect("a test mail server, see the top of this file");
    let mut c = async_imap::Client::new(tcp);
    c.read_response().await.unwrap();
    c.login(user, "secret").await.map_err(|(e, _)| e).unwrap()
}

fn mail(from: &str, to: &str, subject: &str, id: &str, ago_secs: u64, body: &str) -> Vec<u8> {
    lettre::Message::builder()
        .from(from.parse().unwrap())
        .to(to.parse().unwrap())
        .subject(subject)
        .message_id(Some(format!("<{id}>")))
        .date(SystemTime::now() - Duration::from_secs(ago_secs))
        .header(lettre::message::header::ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .unwrap()
        .formatted()
}

/// Mails in a folder of `user`, parsed: (subject, from, In-Reply-To, text).
async fn folder(user: &str, name: &str) -> Vec<(String, String, String, String)> {
    let mut s = imap(user).await;
    let n = s.select(name).await.unwrap().exists;
    let mut out = vec![];
    if n > 0 {
        for f in s.fetch(format!("1:{n}"), "BODY.PEEK[]").await.unwrap().try_collect::<Vec<_>>().await.unwrap() {
            let m = MessageParser::default().parse(f.body().unwrap()).unwrap();
            let from = m.from().and_then(|a| a.first()).and_then(|a| a.address()).unwrap_or("").to_string();
            let irt = m.in_reply_to().as_text().unwrap_or("").to_string();
            out.push((m.subject().unwrap_or("").to_string(), from, irt, m.body_text(0).unwrap_or_default().trim().to_string()));
        }
    }
    let _ = s.logout().await;
    out
}

async fn unseen(user: &str) -> usize {
    let mut s = imap(user).await;
    s.select("INBOX").await.unwrap();
    let n = s.search("UNSEEN").await.unwrap().len();
    let _ = s.logout().await;
    n
}

fn has_input(items: &[Item], id: &str) -> bool {
    items.iter().any(|i| i.kind == "input" && i.id == id)
}

#[test]
fn mail_says_it_is_not_encrypted() {
    let dir = data_dir("plain");
    let h = Host::start("plain", &dir);
    let (level, warning, hint) = h.wait(15, "channel", |m| match m {
        ToHost::Channel { level, warning, add_hint, .. } => Some((level.clone(), warning.clone(), add_hint.clone())),
        _ => None,
    });
    assert_eq!(level, "D");
    assert!(warning.contains("not end-to-end encrypted"));
    assert!(hint.contains("email"));
    let items = h.wait(15, "view", |m| match m {
        ToHost::View { items } => Some(items.clone()),
        _ => None,
    });
    assert!(has_input(&items, "address") && has_input(&items, "pass"));
    h.stop();
    let _ = std::fs::remove_dir_all(dir);
}

/// Contacts from "Sent", a chat, a threaded reply over SMTP, new mail, add and remove, sign out.
#[test]
#[ignore]
fn mail_end_to_end() {
    let rt = rt();
    rt.block_on(async {
        let mut s = imap("me@example.org").await;
        s.create("Sent").await.unwrap();
        s.append("Sent", Some("(\\Seen)"), None, mail("Me <me@example.org>", "Anna <anna@example.org>", "Lunch", "lunch-0@example.org", 7200, "Lunch this week?")).await.unwrap();
        s.append("Sent", Some("(\\Seen)"), None, mail("Me <me@example.org>", "Ben <ben@example.org>", "Keys", "keys-0@example.org", 3600, "Do you have my keys?")).await.unwrap();
        s.append("INBOX", None, None, mail("Anna <anna@example.org>", "me@example.org", "Re: Lunch", "lunch-1@example.org", 1800, "Lunch on Friday?\n\nOn Sun, 18 Oct 2026 at 10:00, Me <me@example.org> wrote:\n> Lunch this week?")).await.unwrap();
        s.append("INBOX", None, None, mail("Shop <news@shop.example>", "me@example.org", "Sale", "sale-1@shop.example", 900, "Buy now")).await.unwrap();
        s.logout().await.unwrap();
    });

    let dir = data_dir("e2e");
    let mut h = Host::start("e2e", &dir);
    h.sign_in("me@example.org");

    // The people I wrote to, nobody else; Anna's reply is unread.
    let list = h.contacts("Anna and Ben", |l| l.len() == 2);
    let anna = list.iter().find(|c| c.id == "anna@example.org").expect("Anna");
    assert_eq!((anna.name.as_str(), anna.unread, anna.info.as_str()), ("Anna", 1, "anna@example.org"));
    assert!(list.iter().any(|c| c.id == "ben@example.org" && c.name == "Ben" && c.unread == 0));
    let stored = keyring::Entry::new("RogueIM mail", "me@example.org@127.0.0.1").unwrap();
    assert_eq!(stored.get_password().unwrap(), "secret", "the password is kept once the login worked");

    // Opening the chat shows both mails, oldest first, the quote cut, and marks Anna's read.
    h.tell(ToPlugin::Open { chat: "anna@example.org".into() });
    h.contacts("Anna read", |l| l.iter().any(|c| c.id == "anna@example.org" && c.unread == 0));
    let lines = h.lines("anna@example.org", "Anna's chat", |l| l.len() == 2);
    assert!(lines[0].from_me && lines[0].text == "subject: Lunch\nLunch this week?", "{:?}", lines[0]);
    assert!(!lines[1].from_me && lines[1].who == "Anna" && lines[1].text == "Lunch on Friday?", "{:?}", lines[1]);
    assert_eq!(rt.block_on(unseen("me@example.org")), 1, "only the shop mail is still unread");

    // A reply goes out over SMTP, threaded, and its copy in "Sent" replaces the pending line.
    h.tell(ToPlugin::Send { chat: "anna@example.org".into(), text: "Friday works.".into() });
    h.lines("anna@example.org", "the pending line", |l| l.iter().any(|x| x.id.starts_with("p:") && x.state == "sending"));
    let lines = h.lines("anna@example.org", "the copy from Sent", |l| l.iter().any(|x| x.id.starts_with("s:") && x.text == "Friday works."));
    assert!(!lines.iter().any(|x| x.id.starts_with("p:")), "{lines:?}");
    assert_eq!(lines.len(), 3);
    let got: Vec<_> = rt.block_on(folder("anna@example.org", "INBOX")).into_iter().filter(|m| m.1 == "me@example.org").collect();
    assert_eq!(got,vec![("Re: Lunch".into(), "me@example.org".into(), "lunch-1@example.org".into(), "Friday works.".into())]);

    // New mail from Anna while her chat is open: lines and an alert (checked every minute).
    rt.block_on(async {
        let mut s = imap("me@example.org").await;
        s.append("INBOX", None, None, mail("Anna <anna@example.org>", "me@example.org", "Re: Lunch", "lunch-3@example.org", 0, "See you then!")).await.unwrap();
        s.logout().await.unwrap();
    });
    let (chat, name) = h.wait(90, "incoming", |m| match m {
        ToHost::Incoming { chat, name } => Some((chat.clone(), name.clone())),
        _ => None,
    });
    assert_eq!((chat.as_str(), name.as_str()), ("anna@example.org", "Anna"));

    // Add and remove by hand, kept in mail.json.
    h.tell(ToPlugin::Add { address: "Carl@Example.org".into() });
    h.contacts("Carl added", |l| l.iter().any(|c| c.id == "carl@example.org"));
    h.tell(ToPlugin::Remove { chat: "ben@example.org".into() });
    h.contacts("Ben removed", |l| !l.iter().any(|c| c.id == "ben@example.org"));
    let cfg = std::fs::read_to_string(dir.join("mail.json")).unwrap();
    assert!(cfg.contains("carl@example.org") && cfg.contains("ben@example.org") && !cfg.contains("secret"), "{cfg}");

    // Signing out forgets the password.
    h.tell(ToPlugin::Click { id: "signout".into() });
    h.contacts("an empty list", |l| l.is_empty());
    std::thread::sleep(Duration::from_millis(300));
    assert!(stored.get_password().is_err(), "the password is removed on sign out");
    h.stop();
    let _ = std::fs::remove_dir_all(dir);
}

/// A new mailbox has no "Sent": the first mail creates it and keeps a copy there.
#[test]
#[ignore]
fn mail_creates_sent() {
    let dir = data_dir("new");
    let mut h = Host::start("new", &dir);
    h.sign_in("new@example.org");
    h.contacts("an empty list", |l| l.is_empty());
    h.tell(ToPlugin::Add { address: "anna@example.org".into() });
    h.contacts("Anna added", |l| l.iter().any(|c| c.id == "anna@example.org"));
    h.tell(ToPlugin::Open { chat: "anna@example.org".into() });
    h.lines("anna@example.org", "an empty chat", |l| l.is_empty());
    h.tell(ToPlugin::Send { chat: "anna@example.org".into(), text: "Hello Anna\nhow are you?".into() });
    let lines = h.lines("anna@example.org", "the copy from Sent", |l| l.iter().any(|x| x.id.starts_with("s:")));
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "subject: Hello Anna\nHello Anna\nhow are you?");
    let rt = rt();
    let sent = rt.block_on(folder("new@example.org", "Sent"));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "Hello Anna");
    h.tell(ToPlugin::Click { id: "signout".into() });
    h.contacts("an empty list", |l| l.is_empty());
    h.stop();
    let _ = std::fs::remove_dir_all(dir);
}
