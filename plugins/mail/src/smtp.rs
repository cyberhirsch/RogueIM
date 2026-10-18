//! Sending mail over SMTP with TLS (implicit on 465, STARTTLS otherwise;
//! plain only to this computer, e.g. a local bridge).

use std::net::Ipv4Addr;
use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::imap::{is_loopback, Account};

/// What a reply refers to: the mail it answers.
#[derive(Clone, Default)]
pub struct Reply {
    pub subject: String,
    pub message_id: String,
    pub references: String,
}

/// Build the mail; returns it with its Message-ID.
pub fn build(acc: &Account, to: &str, to_name: &str, text: &str, reply: Option<&Reply>) -> Result<(Message, String), String> {
    let from = Mailbox::new(Some(acc.display_name()), acc.address.parse().map_err(|e| format!("your address: {e}"))?);
    let to_box = Mailbox::new((!to_name.is_empty() && to_name != to).then(|| to_name.to_string()), to.parse().map_err(|e| format!("{to}: {e}"))?);
    let subject = match reply {
        Some(r) if !r.subject.trim().is_empty() => format!("Re: {}", crate::text::base_subject(&r.subject)),
        _ => first_line(text),
    };
    let domain = acc.address.rsplit_once('@').map(|(_, d)| d).unwrap_or("localhost");
    let id = format!("<{}.rim@{domain}>", unique());
    let mut b = Message::builder().from(from).to(to_box).subject(subject).message_id(Some(id.clone())).date_now();
    if let Some(r) = reply.filter(|r| !r.message_id.is_empty()) {
        b = b.in_reply_to(r.message_id.clone());
        let refs = if r.references.is_empty() { r.message_id.clone() } else { format!("{} {}", r.references, r.message_id) };
        b = b.references(refs);
    }
    let msg = b.header(ContentType::TEXT_PLAIN).body(text.to_string()).map_err(|e| e.to_string())?;
    Ok((msg, id))
}

/// Subject of a new conversation: the start of the first line.
fn first_line(text: &str) -> String {
    let l = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if l.chars().count() > 60 { l.chars().take(57).collect::<String>() + "…" } else if l.is_empty() { "(no subject)".into() } else { l.to_string() }
}

fn unique() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{t:x}.{:x}.{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

pub async fn send(acc: &Account, msg: &Message) -> Result<(), String> {
    let (host, port) = (acc.smtp_host.as_str(), acc.smtp_port);
    let tls = if port == 465 {
        Tls::Wrapper(TlsParameters::new_rustls(host.into()).map_err(|e| e.to_string())?)
    } else if is_loopback(host) {
        Tls::None
    } else {
        Tls::Required(TlsParameters::new_rustls(host.into()).map_err(|e| e.to_string())?)
    };
    let mailer: AsyncSmtpTransport<Tokio1Executor> = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(port)
        .tls(tls)
        .credentials(Credentials::new(acc.user.clone(), acc.pass.clone()))
        // Don't tell the server this computer's name.
        .hello_name(ClientId::Ipv4(Ipv4Addr::LOCALHOST))
        .timeout(Some(Duration::from_secs(30)))
        .build();
    mailer.send(msg.clone()).await.map(|_| ()).map_err(|e| format!("sending failed: {e}"))
}

/// Gmail and Outlook put mail sent over SMTP into "Sent" themselves.
pub fn server_saves_sent(smtp_host: &str) -> bool {
    let h = smtp_host.to_lowercase();
    ["gmail.com", "googlemail.com", "office365.com", "outlook.com"].iter().any(|d| h.ends_with(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc() -> Account {
        Account { address: "me@example.org".into(), name: "Me".into(), ..Default::default() }
    }

    #[test]
    fn new_mail_takes_the_first_line_as_subject() {
        let (m, id) = build(&acc(), "anna@example.org", "Anna", "Lunch on Friday?\nAt 12.", None).unwrap();
        let raw = String::from_utf8(m.formatted()).unwrap();
        assert!(raw.contains("Subject: Lunch on Friday?"));
        assert!(raw.contains("To: Anna <anna@example.org>"));
        assert!(raw.contains(&format!("Message-ID: {id}")));
        assert!(id.ends_with("@example.org>"));
    }

    #[test]
    fn reply_threads_with_the_last_mail() {
        let r = Reply { subject: "AW: Lunch".into(), message_id: "<a1@example.org>".into(), references: "<a0@example.org>".into() };
        let (m, _) = build(&acc(), "anna@example.org", "", "Yes", Some(&r)).unwrap();
        let raw = String::from_utf8(m.formatted()).unwrap();
        assert!(raw.contains("Subject: Re: Lunch"));
        assert!(raw.contains("In-Reply-To: <a1@example.org>"));
        assert!(raw.contains("References: <a0@example.org> <a1@example.org>"));
        assert!(raw.contains("To: anna@example.org"));
    }

    #[test]
    fn bad_address_is_an_error() {
        assert!(build(&acc(), "not an address", "", "x", None).is_err());
    }
}
