# Proposal: RIM as a hub for other messengers, plus "send to anyone"

Status: accepted 2026-10-18 and folded into the PRD (§4, §7.13, §9 transport honesty, §12 M4). This file keeps the
reasoning and design notes. Decisions: no browser windows, everything in RIM's own list and chat windows;
WhatsApp is in, behind a warning; email comes first.

## 1. Comparison

| | Native bridges (plugins) | Embedded web clients (Rambox/Ferdium style) |
|---|---|---|
| Fits RIM's look | Yes: contacts and chats appear in RIM's own plain-text buddy list and chat windows | No: a browser page inside RIM, with its own look |
| Unified list | Yes | No: one panel per service, only unread counts and notifications are unified |
| Memory / binary size | Small (Telegram's TDLib adds about 30 MB) | A browser engine per service (WebView2 / WKWebView / WebKitGTK), 150–400 MB RAM each |
| Terms of service | Email and Telegram fine; Signal unofficial; WhatsApp unofficial libraries can get the account banned | Fine: these are the official web clients |
| Maintenance | Protocol libraries break now and then (Signal, WhatsApp) | Web apps change, but the user just sees the official site |
| Signal | signal-cli as a linked device (unofficial, experimental) | Not possible: Signal has no web client |
| RIM's own E2E | Messages are decrypted on this computer, inside the plugin process | Same, inside the webview |

**Matrix + mautrix bridges on the Pi** (Beeper style) is the most complete option but adds a homeserver, a database and several bridge processes on the Pi, and all bridged messages lie decrypted there. It contradicts "no required infrastructure" more than either option above. Not recommended as the default; possible later as one more plugin ("Matrix account").

## 2. Recommendation (revised: no browser windows)

Every network is a native plugin. RIM draws everything itself: the plugin's contacts go into the buddy list,
its chats open in RIM's normal chat windows, docked or floating. No web pages anywhere.

| Network | How | Status |
|---|---|---|
| Email | IMAP/SMTP (async-imap, lettre) | open standard, no risk |
| Telegram | TDLib, Telegram's own client library | allowed; needs an api_id registered at my.telegram.org |
| Signal | presage (Rust, the library behind the Flare and gurk clients), as a linked device like Signal Desktop | unofficial: works, but can break with Signal updates |
| WhatsApp | whatsmeow (Go, the library behind the mautrix bridge and Beeper), as a linked device | unofficial and against WhatsApp's terms: the account can be blocked. Only behind an explicit warning, or left out |

Order: send to anyone + email, then Telegram, then Signal, then WhatsApp if wanted at all.

The web panel (Rambox approach) is dropped. The only other way to get WhatsApp into RIM's own windows would be a
hidden WhatsApp Web driven in the background: same ban risk as whatsmeow, plus a browser engine. No gain.

Plugins talk to RIM over JSON lines on a local socket, so they can be written in any language (Go for whatsmeow,
a TDLib wrapper for Telegram) without touching RIM's core. Licences fit as separate plugin programs:
whatsmeow MPL-2.0, presage and libsignal AGPL-3.0, TDLib Boost.

## 2a. How it looks in RIM

- **Buddy list**: folders `mail`, `telegram`, `signal`, `whatsapp` below the RIM contacts, collapsible like any
  folder. Each contact carries its network badge and colour (TH-4). Status where the network has one
  (Telegram: online / last seen; WhatsApp: online if the contact shares it; Signal and mail: none).
- **Chat window**: the same window as for RIM contacts, with the level banner (C or D) under the header.
  Text, replies, edits, pictures and files map to what RIM already shows. Everything else becomes a plain text
  line with an action: `[voice message 0:12] [play]`, `[video 0:41] [save]`, `[sticker]`, `[reaction from Anna]`,
  `[missed call]`.
- **Groups** of other networks appear as group chats in the network's folder.
- **Notifications and sounds**: as for RIM contacts, but always with the network badge.
- **Login** inside RIM's settings: QR code for Signal and WhatsApp (linked device), phone number and code for
  Telegram, server and password for mail. Sessions stay in the plugin's folder and the OS keychain.

The real work is the plugin API (HB-1): today plugins only fill sidebar gadgets. They need contacts and presence,
conversations, messages in and out (edits, replies, pictures, files), read state and the login flows. The plugin
never draws anything itself.

## 3. PRD amendment (draft)

**§4 Non-goals**, replace "Federation or bridges to other networks." with:

> - Federation of RIM's own protocol with other networks. RIM accounts stay keypairs; RIM contacts stay end-to-end encrypted.
> - Any bridge in the core. Other networks only come in through optional plugins (§7.13).

**§3 Goals 3** stays: "No phone number, no email. Identity is a keypair." Add: "Plugins for other networks may use the user's existing accounts there; RIM itself never needs one."

**New §7.13 Other networks (optional plugins)**

> Plugins that bring other messengers and email into RIM, for people who are tired of juggling apps. All off by default, each a separate process with declared capabilities, no access to RIM's keys, contacts or history. Their contacts appear in their own folder, marked with the network (e.g. `[mail]`, `[tg]`), and their chats carry a permanent line "not RIM-encrypted: goes through <service>".
>
> | ID | Requirement | Pri |
> |---|---|---|
> | HB-1 | Plugin API extension: a plugin can offer contacts, presence, chats and unread counts that RIM shows in the buddy list and chat windows; sending goes back to the plugin | P2 |
> | HB-2 | **Email** (IMAP/SMTP): folder of correspondents, threads as chats, plain text only, attachments as files | P2 |
> | HB-3 | **Send to anyone**: drag a file onto an email address or "copy link": RIM encrypts the file, uploads the ciphertext to a storage the user chooses (own node, WebDAV/Nextcloud, S3), and gives a link whose key is in the `#fragment`, so the host never sees the file. The recipient's browser decrypts it on the RIM website. Expiry and download limit per link | P2 |
> | HB-4 | **Telegram** via TDLib (official client library) | P2 |
> | HB-5 | **WhatsApp** via whatsmeow as a linked device; unofficial, only after a warning that WhatsApp may block the account | P3 |
> | HB-6 | **Signal** via presage as a linked device; unofficial, marked experimental | P3 |
> | HB-7 | Credentials and sessions of other networks only in the OS keychain / the plugin's own folder, never in RIM backups unless opted in | P2 |
> | HB-8 | RIM renders everything: contacts in the buddy list, chats in RIM's chat windows. Content RIM can't show becomes a text line with an action (`[voice message 0:12] [play]`) | P2 |

**New in §9 Security & privacy: transport honesty.** Applies to RIM's own paths as well as plugins.

> Every conversation shows one of four levels, always visible, in the buddy list, chat header, compose field and notifications:
>
> | Level | Meaning | Mark |
> |---|---|---|
> | **A** direct | RIM, end-to-end encrypted, direct peer-to-peer | none (the normal case) |
> | **B** relayed | RIM, end-to-end encrypted, but via relay, Nostr mailbox or buddy relay | `via relay` / `stored in network` |
> | **C** other network | goes through a third-party service or bridge; protection depends on it, and it is decrypted by that service's client or bridge | network badge `[tg]`, `[wa]` …, warning colour, banner |
> | **D** unencrypted | readable in transit or on servers (e.g. email without PGP/S-MIME) | red badge, banner |
>
> | ID | Requirement | Pri |
> |---|---|---|
> | TH-1 | The level is shown per conversation and per message (a message can go A or B depending on the path at that moment) | P1 |
> | TH-2 | C and D: a banner in the chat window and a line in the compose field that can't be dismissed | P1 |
> | TH-3 | D: the first send on each unencrypted channel asks for confirmation once | P1 |
> | TH-4 | Contacts of other networks never look like RIM contacts: own icon frame, colour and network badge in the list and in notifications | P1 |
> | TH-5 | Diagnostics (NW-6) lists every channel (RIM direct/relayed peers, each plugin) with its encryption and P2P status | P1 |
> | TH-6 | RIM bots that pass messages on (e.g. Rogue to an AI provider) are marked C: end-to-end to the bot, then readable by the provider | P1 |

Already there in v0.1: delivery states "stored in network", diagnostics split into "direct" and "only via relay". Missing: the per-conversation mark in the list and chat header, the per-message path, and TH-6 for Rogue (today only its welcome text warns).

**§7.6 File transfer**: FT-1 stays for RIM contacts. Add: "For people without RIM see HB-3."

**§12 Milestones**: new row after M3:

> | **M4 — Hub** | Plugin API for other networks (HB-1, HB-8), send to anyone (HB-3), email (HB-2), Telegram (HB-4); then Signal (HB-6) and, if wanted, WhatsApp (HB-5) |

## 4. Send to anyone, in detail

- The RIM side: per-file random key, AES-GCM in chunks, upload of the ciphertext only.
- Storage, user's choice: their RIM node on the Pi (new optional HTTP drop endpoint, needs it to be reachable from outside, e.g. a Cloudflare tunnel), WebDAV (Nextcloud), or S3-compatible.
- The link: `https://cyberhirsch.github.io/RogueIM/f/#<storage-url>,<key>`. The page is static, fetches the ciphertext and decrypts it in the browser. The key never reaches any server.
- "While I'm online" P2P links are not recommended: most home connections are not reachable from a browser outside.

## 5. Side note

The PRD is already behind the code: §4 still lists voice calls and AI features as non-goals, but v0.1 has voice calls, screen sharing and the Rogue AI bot. Worth fixing in the same edit.
