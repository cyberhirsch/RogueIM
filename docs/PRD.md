# RogueIM — Product Requirements Document

**Product:** RogueIM (short: RIM)
**Status:** v0.1 alpha built · 2026-09-29 (see §0)
**License:** GPL-3.0-or-later
**Stack:** see [TECH_STACK.md](TECH_STACK.md)

---

## 0. Status of v0.1 (alpha)

All P0 and P1 requirements are implemented, with the deviations below. Everything was built and hand-tested
on Windows 10; Linux and macOS builds compile in CI but have not been run by hand. Nothing has been audited.

| ID | v0.1 |
|---|---|
| ID-2, CT-7 | No QR codes. A desktop cannot scan them and there is no mobile client yet, so they would be for show. Invites, link codes and safety numbers are text to copy or compare. QR returns with the mobile companion. |
| ID-5 | Nickname, about, location, homepage and interests are available. There is **no avatar**: images conflict with the plain-text rule (MS-9). |
| CT-4, GD-1 | Folders and plugin sections are reordered with ^ / v buttons, not drag and drop. |
| CT-8 | Met by construction. The account key *is* the identity, and devices not signed by it are rejected. A different key is a different contact: it arrives as a new authorization request, with a warning when the name matches an existing contact. |
| MS-5 | Instead of a tabbed window, one chat can be docked into the bar. Clicking a contact switches the docked chat. |
| DK-2 | **Open.** There is no layer-shell backend. On Wayland the bar runs through XWayland with X11 struts; whether space is reserved depends on the compositor. |
| DK-5 | Replaced by the owner's design: the bar is always docked. `<` / `>` step through every edge of every monitor (2 monitors = 4 positions), and the position is remembered. |
| ST-1 | Themes are graphite (default), grey, green and amber. They replace Classic / Terminal / Plain light. The monospace font (Hack) is bundled. |
| PR-3 | A locked screen sets Away. This is detected on Windows (input desktop), Linux (logind `LockedHint`) and macOS (`ioreg`). |
| NW-3 | The helper bandwidth cap is enforced per relayed circuit (a byte budget over the circuit lifetime) and takes effect after a restart. |
| NW-6 | Diagnostics show NAT status, connected peers (named when known), relay circuits, listen and external addresses, Nostr relay health, held mail, and each plugin's network destinations (GD-11). |

## 1. One-liner

ICQ's buddy-list feel, rebuilt with no servers and end-to-end encryption by default. Desktop first.

## 2. Problem

- Modern messengers are phone-first, centrally operated, and tie identity to a phone number.
- Centralised services see the social graph, can be shut down, blocked, or compelled.
- Desktop users lost the lightweight, always-there buddy list: glance at who is online, double-click, talk.

## 3. Goals

1. **No required infrastructure.** Every feature works with zero RIM-operated servers.
2. **E2EE by default**, forward secrecy, no plaintext at rest.
3. **No phone number, no email.** Identity is a keypair.
4. **The ICQ experience**: docked buddy list, statuses, away messages, separate chat windows, sounds, authorization requests.
5. **Lightweight**: small binary, low RAM, instant startup.
6. **Open**: GPL-3.0-or-later code and a published protocol specification.

## 4. Non-goals

- Full-featured mobile apps. RIM is desktop first. A lightweight **mobile companion** (chat + presence) is planned later so contacts can be reachable from their phone.
- Voice/video calls.
- Anonymity against a global network observer (Tor transport is a later option).
- Public chat rooms / large communities (>100 members).
- Public user directory — discovery is by invite only.
- Federation or bridges to other networks.
- Rich text, emoji, stickers, GIFs, link previews, remote content, AI features. RIM should feel nerdy and retro — for people who miss ICQ.
- Any telemetry.

**Design rule:** nothing exists just for show. Every element must do real work.

## 5. Users

| Persona | Need |
|---|---|
| **Nostalgic power user** | Wants ICQ back, on desktop, without a corporation behind it |
| **Privacy-conscious professional** | Journalist, researcher, activist; needs E2EE without phone-number identity |
| **Small team / friend group** | Chat and file sharing on LAN or internet without a SaaS account |
| **Self-hoster** | Runs an always-on node (Pi, NAS, VPS) to improve reachability and to run bots |

## 6. Product principles

1. Nothing leaves the device unencrypted except what the network needs to route (IP, device PeerId).
2. Security prompts are rare and meaningful (key change, unverified contact).
3. Looks retro, behaves modern: keyboard-driven, fast, accessible.
4. Honest status: the UI always tells the truth about delivery (sent / stored in network / delivered / read).

---

## 7. Features

Priority: **P0** = MVP, **P1** = v1.0, **P2** = later.

### 7.1 Account & identity

| ID | Requirement | Pri |
|---|---|---|
| ID-1 | Create account offline: generate account key + first device key, choose nickname, set passphrase | P0 |
| ID-2 | Display own fingerprint and invite link / QR | P0 |
| ID-3 | Unlock with passphrase; optional OS-keychain "remember me" | P0 |
| ID-4 | **Full encrypted backup**: account key, devices, contacts (petnames, verification state), groups + keys, settings, all conversations, optionally received files — one file | P0 |
| ID-4a | Backup unlocked by passphrase or 24-word recovery key; recovery key offered at account creation | P0 |
| ID-4b | Scheduled automatic backups to a user-chosen folder, keep last N | P1 |
| ID-4c | Restore: full (new machine) or history-only (into existing device) | P0 |
| ID-5 | Profile: nickname, "about", optional details (ICQ-style info card), shared only with authorized contacts. No avatar (plain text only, MS-9) | P1 |
| ID-6 | Multiple accounts on one install | P2 |
| ID-7 | **Multi-device**: one account on several devices (e.g. desktop + laptop). Every device sends and receives all messages, including ones sent from the other devices | P1 |
| ID-7a | Link a device by scanning a QR / entering a code on an existing device, confirmed by comparing a short word string | P1 |
| ID-7b | Contact list, petnames, verification state, groups, settings and read markers stay in sync across own devices | P1 |
| ID-7c | Newly linked device can pull history from an existing one (all / last N days / none) over a direct connection | P1 |
| ID-7d | Device manager: list own devices (OS, class, last seen), rename, revoke. Only "manager" devices can link/revoke | P1 |
| ID-7e | Protocol is multi-device from MVP (every install is a device), so ID-7 ships without breaking v1 clients | P0 |
| ID-7f | **Remote logout** from the device manager: *Lock* (require passphrase, stays linked) or *Log out & wipe* (revoke + erase keys and history on that device). Offline devices execute it on next connect; status shown as pending/done | P1 |

### 7.2 Contacts (buddy list)

| ID | Requirement | Pri |
|---|---|---|
| CT-1 | Add contact via invite link / QR / pasted ID (full key) | P0 |
| CT-2 | Buddy-list names are local petnames; the peer's own nickname is only a suggestion on add | P0 |
| CT-3 | **Authorization request** flow: request with message → accept / deny / ignore. Presence and profile only visible after mutual auth | P0 |
| CT-4 | Groups in buddy list (collapsible, drag & drop), rename, reorder | P0 |
| CT-5 | Sort by status then name; "hide offline" toggle | P0 |
| CT-6 | Visible list / invisible list / ignore list (ICQ semantics) | P1 |
| CT-7 | Safety-number verification (compare digits or scan QR); verified badge | P0 |
| CT-8 | Key-change warning blocks sending until acknowledged. A contact adding a device signed by their account key is not a key change (quiet notice only) | P0 |
| CT-9 | **Trusted introductions**: send a contact to a buddy as a signed introduction ("Anna introduced Ben; Anna has verified Ben"). Recipient sees who vouched and at what verification level; can accept the vouch as verification or verify directly | P1 |
| CT-10 | **Invite links are single-use or expiring** (user picks: one use / N uses / 24 h / 7 d / permanent). A used or expired token is rejected before any contact request is shown. Active invites listed and revocable | P0 |

### 7.3 Presence

| ID | Requirement | Pri |
|---|---|---|
| PR-1 | Statuses: Online, Away, N/A, Occupied, DND, Free for Chat, Invisible, Offline | P0 |
| PR-2 | Custom away message, auto-reply on first message while Away/N/A | P1 |
| PR-3 | Auto-Away after N minutes idle, auto-N/A after M minutes, on screen lock | P1 |
| PR-4 | Presence only sent to authorized contacts; Invisible sends nothing | P0 |
| PR-5 | "Contact is online" notification + sound (per-contact toggle) | P1 |
| PR-6 | **OS icon as status icon.** Each contact shows the OS they are currently on (Windows, macOS, Linux + distro, iOS, Android), tinted by status with a status-shape badge. Replaces ICQ's flower. Own tray icon uses own OS | P0 |
| PR-7 | OS is sent only inside signed presence to authorized contacts; Invisible sends nothing. With several devices online, the buddy list shows the most recently active one; all online devices on hover | P0 |
| PR-8 | **Device class: desktop / laptop / mobile.** The OS glyph sits in a monitor, laptop or phone frame. A desktop user always sees when a contact is on their phone. Part of the presence protocol from v1 | P0 |
| PR-9 | Device class changes behaviour, not just the icon: files > 25 MB to a mobile contact, or a laptop on battery, ask for confirmation; mobile contacts show "may reply late" when the app is backgrounded; typing indicators are not sent to backgrounded mobile contacts; laptop on battery shown on hover | P1 |

### 7.4 Messaging

| ID | Requirement | Pri |
|---|---|---|
| MS-1 | 1:1 E2EE text messages, Double Ratchet | P0 |
| MS-2 | Delivery states: sending → sent → stored in network → delivered → read (read receipts optional, default off) | P0 |
| MS-3 | **Offline messages** via Nostr relay mailbox, retained 14 days | P0 |
| MS-3a | Additional offline layers: DHT mailbox and buddy relays, used in parallel with Nostr | P1 |
| MS-4 | Typing indicator (toggleable) | P1 |
| MS-5 | Separate window per conversation (default) or tabbed window | P0 |
| MS-6 | Local encrypted history with search | P0 |
| MS-7 | Edit / delete-for-everyone (best effort, clearly labelled) | P1 |
| MS-8 | Disappearing messages per chat | P1 |
| MS-9 | **Plain text only**: no rich text, no markdown, no emoji picker, no stickers, no inline images, no remote content. Text shown exactly as typed | P0 |
| MS-10 | Replies/quotes | P1 |
| MS-11 | Message to multiple contacts at once (ICQ "multiple recipients") | P2 |
| MS-12 | **Note to self**: a conversation with your own account, synced across own devices; text and files (files via direct connection between own devices) | P1 |
| MS-13 | **Urgent flag**: a message marked urgent breaks through DND/Occupied once (sound + popup). Recipient can disable urgent per contact; max 3 urgent messages per contact per hour | P1 |

### 7.5 Group chat

| ID | Requirement | Pri |
|---|---|---|
| GR-1 | Private groups up to 100 members, invite-only, Megolm E2EE | P1 |
| GR-2 | Admin roles: invite, remove, rename; removal triggers key rotation | P1 |
| GR-3 | Group offline catch-up via members' replicas | P1 |

### 7.6 File transfer

| ID | Requirement | Pri |
|---|---|---|
| FT-1 | Files go **only over a direct connection** between the two devices, drag & drop. Never through relays, Nostr or mailboxes | P0 |
| FT-2 | End-to-end encrypted on top of the encrypted connection: per-file key, sent inside the chat's Double Ratchet channel | P0 |
| FT-3 | Accept/decline prompt showing name, size, sender fingerprint; auto-accept per contact optional | P0 |
| FT-4 | Progress, pause/resume across reconnects, hash verified before the file is released to the user | P0 |
| FT-5 | Both sides must be online. If no direct path can be made, the transfer is queued and retried automatically when a path appears, with a clear "no direct connection" state and a hint (same LAN, IPv6, enable UPnP) | P0 |
| FT-6 | No size limit imposed by RIM | P0 |

### 7.7 Notifications & sounds

| ID | Requirement | Pri |
|---|---|---|
| NT-1 | Tray icon = own OS icon in own status; blinks on unread message | P0 |
| NT-2 | OS notifications (content hidden by default: "New message from X") | P0 |
| NT-3 | Event sounds: message, urgent message, contact online, auth request, file incoming — original assets, per-event toggle | P1 |
| NT-4 | DND/Occupied suppress sounds and popups (except MS-13 urgent) | P0 |

### 7.8 Network & nodes

| ID | Requirement | Pri |
|---|---|---|
| NW-1 | Works on LAN with no internet (mDNS) | P0 |
| NW-2 | Works behind typical home NAT (hole punching, relay fallback for chat) | P0 |
| NW-3 | "Help the network" toggle: act as DHT server / relay when publicly reachable, with bandwidth cap | P1 |
| NW-4 | Headless node mode (`rim-cli --node`) for always-on relay/bootstrap/mailbox | P1 |
| NW-5 | **One-command node**: official Docker image and Raspberry Pi package (.deb, arm64) for `rim-cli --node` | P1 |
| NW-6 | Connection diagnostics panel (NAT type, peers, relays, Nostr relay health) | P1 |
| NW-7 | Tor transport | P2 |
| NW-8 | Nostr layer: user-editable relay list (default ~5 independent relays) | P0 |
| NW-9 | Contacts find each other's current address via encrypted Nostr rendezvous, no bootstrap list needed | P0 |
| NW-10 | LAN-only mode: disables Nostr and WAN DHT entirely | P1 |
| NW-11 | **Mailbox cleanup**: after fetching, the recipient requests deletion of the mailbox event from the relays (NIP-09), so ciphertext doesn't linger for 14 days | P0 |
| NW-12 | Custom bootstrap list editable in settings (last-resort fallback) | P1 |

### 7.9 Bots & automation

| ID | Requirement | Pri |
|---|---|---|
| BT-1 | **Bot mode**: `rim-cli` runs as its own RIM account (e.g. on a Pi) and sends messages to authorized contacts — alerts for backups, servers, disks | P1 |
| BT-2 | Local interface for scripts: `rim-cli send <contact> <text>`, stdin piping, and a local-only Unix socket / named pipe JSON API | P1 |
| BT-3 | Bots are ordinary contacts: same auth, same E2EE, marked with a bot badge; they receive commands only from contacts on their allowlist | P1 |

### 7.10 Window & docking

| ID | Requirement | Pri |
|---|---|---|
| DK-1 | Buddy list docks to the left or right screen edge, reserving screen space, on **Windows and Linux X11** | P0 |
| DK-2 | Same with reserved space on KDE / wlroots Wayland (layer-shell) | P1 |
| DK-3 | macOS and GNOME Wayland (no API to reserve space): docked = edge-snapped, always on top, on all desktops/Spaces | P1 |
| DK-4 | Auto-hide: collapses to a thin strip, reveals on edge hover or unread message | P1 |
| DK-5 | Drag to an edge to dock, drag away to float; remembers edge and monitor | P0 |
| DK-6 | Multi-monitor and DPI changes handled without the bar ending up off-screen | P0 |

### 7.11 Sidebar gadgets

Gadgets are **plugins** in the docked sidebar below the buddy list. A plugin that isn't enabled isn't loaded at all. Each runs as a separate process with only the capabilities it declares, so it can never see keys or message content except where stated. v1 ships first-party signed plugins only.

| ID | Requirement | Pri |
|---|---|---|
| GD-1 | **Everything optional, loaded only when needed.** All plugins are off by default; the sidebar is just the buddy list until the user enables one. A disabled plugin is not loaded — no process, no code, no network. Enabled ones appear as collapsible sections, drag to reorder; layout synced across own devices | P1 |
| GD-1a | Plugin manager: enable/disable, show requested capabilities before enabling, show resource use. Plugins can also be uninstalled entirely (installer offers them as optional components) | P1 |
| GD-1b | Third-party plugins with user-confirmed signatures and capability prompts | P2 |
| GD-2 | **Pomodoro timer**: work/break lengths, cycle count, sound at phase end (original assets) | P1 |
| GD-3 | Pomodoro drives presence: during focus, status switches to Occupied (or DND, user choice) with away message "Focusing — back at HH:MM"; restored at break | P1 |
| GD-4 | Pomodoro timer state syncs across own devices (start on desktop, laptop shows the same countdown) | P1 |
| GD-5 | Shared pomodoro with a contact: both see the same timer (co-working) | P2 |
| GD-6 | **Todo list** with backends: local **todo.txt** file (P1), **CalDAV VTODO** e.g. Nextcloud (P1), Todoist, Microsoft To Do, Google Tasks (P2). Add, complete, reorder; one list per backend | P1 |
| GD-7 | Message → task: right-click a chat message, "Add as task" (text + contact name), goes to the chosen todo backend | P1 |
| GD-8 | Service credentials (OAuth tokens, CalDAV passwords) stored in the OS keychain, never in backups unless the user opts in | P1 |
| GD-9 | **Player controls** for whatever player is active: title/artist, play/pause, previous, next. Windows: any app using the system media controls (Spotify, browsers, VLC, foobar2000 …); Linux: any MPRIS player; macOS: see risk | P1 |
| GD-10 | **Now playing as status** (opt-in, off by default): current track shown to authorized contacts under your name, ICQ/MSN-era "listening to" | P1 |
| GD-11 | All gadget network access is opt-in per service and shown in diagnostics; with no service configured, gadgets make no network calls | P1 |

### 7.12 Settings, keyboard & data

| ID | Requirement | Pri |
|---|---|---|
| ST-1 | Themes: Classic (ICQ-era) / Terminal (green or amber on black) / Plain light. Monospace or pixel-style fonts bundled | P1 |
| ST-2 | Start with OS, start minimised to tray | P0 |
| ST-3 | Lock app after idle (require passphrase) | P1 |
| ST-4 | Wipe account (crypto-erase DB and keys) with confirmation | P0 |
| ST-5 | History export (plaintext/HTML) on explicit request | P2 |
| ST-6 | **Global hotkeys** (rebindable): show/hide buddy list, reply to last message, open "note to self", set status (Online / Away / DND) | P1 |

---

## 8. Key user flows

1. **First run:** Welcome → create or restore → nickname + passphrase → recovery key shown, user confirms it is stored → invite link + QR (single-use by default) → buddy list.
2. **Add friend:** Paste invite link → contact card with fingerprint → "Request authorization" + message → friend sees request → accept → both show each other's presence.
3. **Introduction:** Anna sends Ben's card to Carl → Carl sees "introduced by Anna (verified)" → one click to request authorization.
4. **Chat:** Double-click buddy → chat window opens → type, Enter sends → status ticks advance.
5. **Offline message:** Buddy offline → "stored in network" → buddy logs in, fetches mailbox, relays asked to delete → sender sees "delivered".
6. **Link laptop:** Laptop: "Link to existing account" shows QR → desktop scans → compare words → choose history range → laptop joins, buddy list syncs.
7. **Key change:** Buddy restores without their account key → banner "Safety number changed" → sending blocked until user accepts or verifies.

---

## 9. Security & privacy requirements

**Threat model — protect against:**
- Passive network observers reading content.
- Malicious relays / DHT nodes / Nostr relays reading, altering or replaying messages.
- Device theft while locked; stolen device after revocation.
- Spam from strangers.

**Accepted exposure (documented to users):**
- IP addresses are visible to contacts and to DHT/relay peers.
- DHT nodes can observe that some PeerId stores/fetches mailbox records (mailbox keys rotate and are unlinkable to identity without the shared secret).
- Nostr relays see the IP of whoever posts or fetches, plus timing and ciphertext size. They cannot read content or link events to a RIM identity.
- A compromised unlocked device exposes everything on it.

**Requirements:**
- SEC-1 All payloads E2EE; no fallback to plaintext.
- SEC-2 Forward secrecy and post-compromise security for 1:1.
- SEC-3 History DB encrypted at rest (SQLCipher, Argon2id key).
- SEC-4 Keys zeroized in memory after use; never logged.
- SEC-5 All parsers fuzzed; no panic on malformed input.
- SEC-6 Unsolicited contact requests require a valid invite token (CT-10) or an introduction (CT-9), plus proof-of-work and rate limits.
- SEC-7 Independent security audit before 1.0.
- SEC-8 No telemetry, no crash reporting without explicit opt-in per report.
- SEC-9 **Published protocol specification** (wire format, key schedule, mailbox derivation, device lists) versioned in `docs/protocol/`, kept in sync with the code; the audit covers spec and implementation.

---

## 10. Non-functional requirements

| Metric | Target |
|---|---|
| Installer size | ≤ 15 MB |
| Cold start to buddy list | ≤ 1 s (unlocked), mid-range laptop |
| Idle RAM | ≤ 60 MB with buddy list + 5 chat windows open |
| Idle CPU | ≤ 1 % average |
| Idle bandwidth (not a relay) | ≤ 5 KB/s |
| Message latency, direct | ≤ 300 ms p95 same continent |
| Time to first contact reachable after login | ≤ 10 s p90 |
| Offline delivery success (recipient online within 7 days) | ≥ 99 % in network sim |
| Accessibility | Full keyboard navigation, screen-reader labels, scalable text |
| i18n | English + German at 1.0; strings externalised |

---

## 11. Success metrics

No telemetry, so measured via:
- GitHub release downloads, stars, issues.
- Number of `rim-cli --node` Docker pulls / package installs.
- Opt-in anonymous network census (count of DHT peers seen by volunteer nodes).
- Network simulation benchmarks in CI (delivery rate, latency, NAT success).
- Community-reported NAT traversal success via diagnostics panel copy-paste.

---

## 12. Milestones

| Milestone | Scope |
|---|---|
| **M0 — Spike** | Two CLI peers exchange E2EE messages over LAN and via relay; protocol spec draft; Slint chat window with 10 000 plain-text messages and 5 windows open, RAM measured; Win32 AppBar docking of a Slint window |
| **M1 — MVP (P0)** | Desktop app: account, single/expiring invites, auth, buddy list, statuses, OS/device icons, 1:1 chat, **Nostr-only** offline mailbox + cleanup, history, direct file transfer, backup/restore, docking on Windows + X11, tray |
| **M2 — Beta** | P1: multi-device + remote logout, scheduled backups, introductions, note to self, urgent flag, hotkeys, DHT mailbox + buddy relays, docking on Wayland/macOS, headless node + Docker/Pi package, bot mode, plugin host + first-party plugins (pomodoro, todo.txt/CalDAV, player), diagnostics, themes, sounds, fuzzing + external audit |
| **M3 — 1.0** | Groups, audit fixes, signed releases on 3 OSes, protocol spec 1.0, docs |
| **Later** | Mobile companion (another device on the same account), Tor, voice, MLS groups |

---

## 13. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| MVP relies on Nostr alone for offline delivery | Med | Post to 3–5 relays; DHT + buddy relays follow in M2; user-editable relay list |
| Too few always-on peers → poor reachability | Med | Nostr rendezvous; Docker/Pi node; bot mode gives nodes a reason to exist |
| Nostr relays drop, censor or rate-limit RIM events | Med | Multiple relays, PoW, later DHT/buddy fallback |
| Nostr relays correlate IPs and timing | Med | Ephemeral/derived keys; mailbox cleanup; optional Tor (NW-7); jittered fetches |
| Symmetric NAT / CGNAT blocks hole punching | Med | Chat: relay fallback. Files: direct only, so some pairs can't transfer — IPv6, UPnP/NAT-PMP/PCP, queue-and-retry, diagnostics hint |
| DHT spam / Sybil attacks on mailboxes | Med | PoW, record size limits, signed records |
| macOS and GNOME can't reserve screen space | Med | Documented fallback (DK-3) |
| macOS has no public API to read or control other apps' playback (MediaRemote is private and restricted since macOS 15.4) | Med | macOS: media-key play/pause/next (needs Accessibility permission) + AppleScript for Spotify and Music.app with track info; other players controls-only |
| Todo service APIs change or revoke third-party access | Low | todo.txt and CalDAV (open standards) first; proprietary backends P2 and isolated per module |
| Gadgets pull RIM away from being a messenger | Med | Plugins, off by default, not loaded unless enabled; first-party set tied to messaging/presence (GD-3, GD-7, GD-10); new plugins must pass the "real function" rule |
| Malicious or buggy plugin | Med | Separate process, declared capabilities enforced by host, signed first-party only in v1, no access to keys or history |
| Wayland layer-shell docking needs a custom Slint platform backend | Med | Beta item; X11/XWayland works meanwhile |
| Slint's smaller ecosystem (tray, updater, notifications come from separate crates) | Low | All exist and are maintained; wrapped behind one platform module |
| OS logo trademark objections (Apple, Microsoft) | Med | Original glyphs, legal review before 1.0, generic fallback glyphs ready |
| "RogueIM" / "RIM" name conflicts (e.g. BlackBerry's former "RIM") | Med | Trademark search before first public release; "RIM" used only as internal short form |
| OS field is self-reported | Low | Only to authorized contacts; labelled "reported by contact" on hover |
| All manager devices lost and no backup → account unrecoverable | High | Recovery key at setup; reminders until first backup exists; suggest a second manager device |
| Stolen device never reconnects or runs a modified client → wipe never executes | Med | Revocation still cuts it off; local data stays protected by its passphrase-derived DB key |
| Introductions spread a mistaken or malicious vouch | Low | Vouch shown with its source; never overrides a direct verification; revocable |
| Bots become a spam or abuse vector | Low | Bots are normal contacts needing authorization; command allowlist |
| Multi-device fan-out multiplies traffic | Low | Messages are small; files are never fanned out through mailboxes |
| Impersonation via look-alike nicknames | Med | Local petnames; add only by full key; safety-number verification |
| Trademark claims (ICQ) | Med | Original name, art and sounds; no ICQ assets |
| Crypto implementation bugs | High | Use vodozemac/libp2p primitives only, no custom crypto, published spec, audit |

---

## 14. Open questions

1. Mailbox retention: 14 days fixed, or sender-configurable?
2. Should "read" receipts exist at all, or only "delivered"?
3. Groups: Megolm now vs. MLS (Marmot over Nostr) later?
4. Which Nostr relays form the default set, and should RIM users be nudged to run their own?
