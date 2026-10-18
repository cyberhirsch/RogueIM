//! Mail as chat text: plain text body, quotes and signatures trimmed,
//! attachments named, subjects compared without "Re:"/"AW:" prefixes.

use mail_parser::{Message, MessageParser, MimeHeaders};

/// Longest text shown for one mail.
const MAX_CHARS: usize = 4000;

/// The new part of a mail, as a chat line; `more` names attachments known
/// from the mail's structure (big mails are not fetched whole).
pub fn chat_text(msg: &Message<'_>, more: &[String]) -> String {
    let body = msg.body_text(0).map(|b| b.to_string()).unwrap_or_default();
    let mut text = strip_quotes(&body);
    if text.is_empty() {
        text = body.trim().to_string();
    }
    if text.chars().count() > MAX_CHARS {
        text = text.chars().take(MAX_CHARS).collect::<String>() + " …";
    }
    let files: Vec<String> = msg
        .attachments()
        // Pictures inside the mail (a logo in the signature) are no attachments.
        .filter(|p| !(p.content_id().is_some() && !p.content_disposition().is_some_and(|d| d.is_attachment()) && p.content_type().is_some_and(|t| t.ctype().eq_ignore_ascii_case("image"))))
        .map(|p| p.attachment_name().map(str::to_string).unwrap_or_else(|| p.content_type().map(|t| format!("{}/{}", t.ctype(), t.subtype().unwrap_or("")).to_lowercase()).unwrap_or_else(|| "file".into())))
        .chain(more.iter().cloned())
        .map(|n| format!("[attachment: {n}]"))
        .collect();
    if !files.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&files.join("\n"));
    }
    if text.is_empty() { "[no text]".into() } else { text }
}

/// Drop quoted earlier mails, reply headers ("On … wrote:") and the signature.
pub fn strip_quotes(body: &str) -> String {
    let lines: Vec<&str> = body.lines().map(|l| l.trim_end()).collect();
    let mut keep: Vec<&str> = vec![];
    for (i, l) in lines.iter().enumerate() {
        if *l == "--" || *l == "-- " || is_reply_start(l, lines.get(i + 1).copied(), &lines[i + 1..]) {
            break;
        }
        if l.starts_with('>') {
            continue;
        }
        keep.push(l);
    }
    // No runs of blank lines, none at the ends.
    let mut out: Vec<&str> = vec![];
    for l in keep {
        if l.trim().is_empty() && out.last().map(|p: &&str| p.trim().is_empty()).unwrap_or(true) {
            continue;
        }
        out.push(l);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Where a quoted earlier mail begins.
fn is_reply_start(line: &str, next: Option<&str>, rest: &[&str]) -> bool {
    let t = line.trim();
    let lower = t.to_lowercase();
    if lower.starts_with("-----original message")
        || lower.starts_with("-----ursprüngliche nachricht")
        || lower.starts_with("-------- original message")
        || lower.starts_with("-------- ursprüngliche nachricht")
        || lower.starts_with("________________________________")
    {
        return true;
    }
    // "On Mon, 5 Oct 2026, Anna <a@b> wrote:" / "Am 05.10.2026 schrieb Anna:", sometimes
    // wrapped over two lines. It carries a date, so it has digits.
    let says = |s: &str| {
        let s = s.to_lowercase();
        s.contains("wrote") || s.contains("schrieb")
    };
    let colon = t.ends_with(':') || next.is_some_and(|n| n.trim().ends_with(':'));
    if (lower.starts_with("on ") || lower.starts_with("am ")) && t.chars().any(|c| c.is_ascii_digit()) && (says(t) || next.is_some_and(says)) && colon {
        return true;
    }
    if (lower.ends_with("wrote:") || lower.ends_with("schrieb:")) && next.is_some_and(|n| n.starts_with('>') || n.trim().is_empty()) {
        return true;
    }
    // Outlook: "From: …" followed by "Sent:"/"Date:" within the next lines.
    if lower.starts_with("from: ") || lower.starts_with("von: ") {
        return rest.iter().take(3).any(|n| {
            let n = n.trim().to_lowercase();
            n.starts_with("sent: ") || n.starts_with("gesendet: ") || n.starts_with("date: ") || n.starts_with("datum: ")
        });
    }
    false
}

/// A subject without reply and forward prefixes, for comparing.
pub fn base_subject(s: &str) -> String {
    let mut s = s.trim();
    loop {
        let lower = s.to_lowercase();
        let cut = ["re:", "aw:", "fwd:", "fw:", "wg:", "antw:"].iter().find(|p| lower.starts_with(*p)).map(|p| p.len());
        match cut {
            Some(n) => s = s[n..].trim_start(),
            None => break,
        }
    }
    s.to_string()
}

/// Decode a header value that may contain RFC 2047 encoded words.
pub fn decode_words(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw).trim().to_string();
    if !s.contains("=?") {
        return s;
    }
    let header = format!("Subject: {s}\r\n\r\n");
    MessageParser::default().parse_headers(header.as_bytes()).and_then(|m| m.subject().map(str::to_string)).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_signature_are_dropped() {
        let body = "Sounds good, see you at 8.\n\nAnna\n-- \nAnna Example\nwww.example.org\n";
        assert_eq!(strip_quotes(body), "Sounds good, see you at 8.\n\nAnna");
        let reply = "Yes!\n\nOn Mon, 5 Oct 2026 at 10:00, Ben <ben@example.org> wrote:\n> Dinner tonight?\n> Ben\n";
        assert_eq!(strip_quotes(reply), "Yes!");
        let wrapped = "Ja klar.\n\nAm 05.10.2026 um 10:00 schrieb Ben\n<ben@example.org>:\n> Essen?\n";
        assert_eq!(strip_quotes(wrapped), "Ja klar.");
        let outlook = "Fine by me.\n\n________________________________\nFrom: Ben\nSent: Monday\nSubject: x\n";
        assert_eq!(strip_quotes(outlook), "Fine by me.");
        let outlook_de = "Passt.\n\nVon: Ben <ben@example.org>\nGesendet: Montag, 5. Oktober 2026\nAn: Anna\n";
        assert_eq!(strip_quotes(outlook_de), "Passt.");
        let inline = "> first question\nanswer one\n> second\nanswer two\n";
        assert_eq!(strip_quotes(inline), "answer one\nanswer two");
    }

    #[test]
    fn a_line_mentioning_from_is_kept() {
        assert_eq!(strip_quotes("From: the start, it was clear.\nThanks"), "From: the start, it was clear.\nThanks");
    }

    #[test]
    fn subjects_compare_without_prefixes() {
        assert_eq!(base_subject("Re: AW: Fwd: Lunch"), "Lunch");
        assert_eq!(base_subject("RE:Lunch"), "Lunch");
        assert_eq!(base_subject("Rezept"), "Rezept");
    }

    #[test]
    fn encoded_words_are_decoded() {
        assert_eq!(decode_words(b"=?UTF-8?B?SsO8cmdlbg==?="), "Jürgen");
        assert_eq!(decode_words(b"=?ISO-8859-1?Q?M=FCller?="), "Müller");
        assert_eq!(decode_words(b"Plain Name"), "Plain Name");
    }

    #[test]
    fn chat_text_names_attachments() {
        let raw = b"From: Anna <anna@example.org>\r\nTo: me@example.org\r\nSubject: Photos\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHere they are.\r\n--b\r\nContent-Type: image/jpeg\r\nContent-Disposition: attachment; filename=\"beach.jpg\"\r\nContent-Transfer-Encoding: base64\r\n\r\n/9j/4AAQ\r\n--b--\r\n";
        let msg = MessageParser::default().parse(raw).unwrap();
        assert_eq!(chat_text(&msg, &[]), "Here they are.\n[attachment: beach.jpg]");
        assert_eq!(chat_text(&msg, &["big.pdf".into()]), "Here they are.\n[attachment: beach.jpg]\n[attachment: big.pdf]");
    }

    #[test]
    fn a_logo_in_the_signature_is_no_attachment() {
        let raw = b"From: a@b.c\r\nSubject: x\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n--r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Thanks</p><img src=\"cid:logo1\">\r\n--r\r\nContent-Type: image/png; name=\"image001.png\"\r\nContent-Disposition: inline; filename=\"image001.png\"\r\nContent-ID: <logo1>\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--r--\r\n";
        let msg = MessageParser::default().parse(raw).unwrap();
        assert_eq!(chat_text(&msg, &[]).trim(), "Thanks");
    }

    #[test]
    fn html_only_mail_becomes_text() {
        let raw = b"From: a@b.c\r\nSubject: x\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>there</b></p>";
        let msg = MessageParser::default().parse(raw).unwrap();
        assert_eq!(chat_text(&msg, &[]).trim(), "Hello there");
    }
}
