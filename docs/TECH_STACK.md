# RogueIM — Tech Stack

RogueIM (short: RIM). Serverless, end-to-end encrypted, desktop-first chat in the spirit of ICQ.

"Serverless" here means **no RIM-operated server is required for any feature**. Peers find each other, relay for each other and hold each other's offline mail. Public **Nostr relays** (independent, third-party, interchangeable) carry offline mail and rendezvous (disabled in LAN-only mode); they only ever see opaque ciphertext under unlinkable keys. See §3.

---

## 1. Summary

| Layer | Choice | Why |
|---|---|---|
| App shell + UI | **Slint** (Rust API, `.slint` markup, winit backend, Skia or FemtoVG renderer) | Native, no browser engine: ~20–40 MB RAM (estimate, verify in M0), small binary, no HTML/XSS attack surface, pixel-exact retro look, cheap multi-window for ICQ-style separate chats. Plain-text chat removes the one area where a webview would have won |
| Core language | **Rust** (stable, 2021+ edition) | Memory safety for crypto/network code; one language for core, net, store |
| P2P networking | **rust-libp2p** | Kademlia DHT, QUIC, NAT traversal (AutoNAT, Circuit Relay v2, DCUtR hole punching), mDNS, gossipsub — all serverless primitives in one audited-in-production stack |
| Store-and-forward / rendezvous | **Nostr** via `nostr-sdk` (rust-nostr) | Thousands of existing independent relays; fixes offline delivery and bootstrap without RIM infrastructure |
| Transport security | **Noise XX / TLS 1.3 over QUIC** (libp2p built-in) | Authenticated, encrypted peer links bound to PeerId |
| Message E2EE (1:1) | **vodozemac** (Olm: X3DH-style handshake + Double Ratchet) | Rust, audited, forward secrecy + post-compromise security |
| Message E2EE (groups) | **vodozemac Megolm** (v1) → evaluate **OpenMLS** (v2) | Megolm is simple without a central orderer; MLS needs commit ordering that is hard serverless. Evaluate the Marmot protocol (MLS over Nostr) as the v2 path — maturity not yet verified |
| Identity keys | **Ed25519** (ed25519-dalek) | Account key (fingerprint) signs per-device keys; device key = libp2p PeerId (§4) |
| Wire format | **Protocol Buffers** via `prost` | Versioned, compact, schema-evolvable |
| Local storage | **SQLite + SQLCipher** via `rusqlite` (`bundled-sqlcipher`) | Encrypted at rest, single file, zero admin |
| Own-device state sync | **Automerge** (`automerge`) | CRDT: contact list and settings merge across devices without a primary or server |
| Key derivation | **Argon2id** (`argon2`) | Passphrase → DB key |
| Secret storage | OS keychain via `keyring` (DPAPI / Keychain / Secret Service) | Optional "remember me" without plaintext keys on disk |
| Memory hygiene | `zeroize`, `secrecy` | Wipe key material on drop |
| Native windowing extras | `windows` (Win32 AppBar), `x11rb`, `smithay-client-toolkit` (layer-shell), `objc2` | Edge docking per platform (§5.1); raw handles via Slint's winit accessor |
| OS / device detection | `os_info`, `wmi` (Windows chassis), `starship-battery` | OS icon + desktop/laptop/mobile frame (§5.3) |
| Async runtime | **tokio** on worker threads; UI updates posted with `slint::invoke_from_event_loop` | libp2p is tokio-native; Slint owns the main thread |
| Logging | `tracing` + `tracing-subscriber` | Structured; **message content and keys never logged** |
| Updates | `cargo-packager` updater (signed, from GitHub Releases) | Only non-P2P network call; user-toggleable |
| Notifications / tray | `notify-rust`, `tray-icon` | Tray shows own OS icon in own status, blinks on unread |
| Global hotkeys | `global-hotkey` | Show/hide buddy list, reply to last, note to self, set status |
| Audio | `rodio` | Event sounds (original assets — see §7) |

---

## 2. Repository layout (Cargo workspace)

```
rim/
├─ crates/
│  ├─ rim-proto/     # .proto schemas, prost codegen, versioning
│  ├─ rim-crypto/    # identity, vodozemac sessions, fingerprints, safety numbers
│  ├─ rim-net/       # libp2p swarm, behaviours, DHT mailbox, relay, presence
│  ├─ rim-nostr/     # Nostr mailbox + rendezvous (feature-gated, off in LAN-only builds)
│  ├─ rim-store/     # SQLCipher schema, migrations, history, contact list
│  ├─ rim-core/      # orchestration: account, contacts, messaging state machine
│  ├─ rim-plugin-host/ # plugin discovery, process launch, IPC broker, capability checks
│  ├─ rim-plugin-sdk/  # crate plugins link against: IPC protocol, typed API
│  └─ rim-cli/       # headless client, node mode, bot mode + local JSON API
├─ plugins/
│  ├─ pomodoro/      # separate executables + .slint UI + manifest
│  ├─ todo/
│  └─ player/
├─ app/
│  ├─ src/           # Rust: window management, dock/, tray, bridge to rim-core
│  └─ ui/            # .slint files (buddy list, chat window, dialogs, themes)
├─ assets/sounds/
├─ assets/fonts/     # bundled monospace / pixel-style fonts
├─ deploy/          # Dockerfile, arm64 .deb packaging for rim-cli --node
└─ docs/
   └─ protocol/      # versioned protocol specification (SEC-9)
```

`rim-core` has no UI dependency, so the same engine runs as the desktop app, a CLI, a bot and an always-on relay/bootstrap node (e.g. on a Raspberry Pi).

---

## 3. Networking design

| Concern | Mechanism |
|---|---|
| Transports | QUIC (primary), TCP + Noise + Yamux (fallback) |
| LAN discovery | mDNS |
| WAN discovery | Kademlia DHT on a RIM-specific protocol ID (`/rim/kad/1.0.0`) |
| Bootstrap | (a) **Nostr rendezvous** (below), (b) peers remembered from previous sessions, (c) invite links/QR codes carrying multiaddrs, (d) any `rim-cli --node` instance volunteers, (e) editable community bootstrap list as last resort |
| Nostr rendezvous | Each account holds a *rendezvous key* shared with authorized contacts at auth time. Current multiaddrs are published as an encrypted replaceable event under a pubkey derived from it; contacts subscribe and dial. Rotated when a contact is removed |
| NAT traversal | AutoNAT → DCUtR hole punching → Circuit Relay v2 fallback via reachable peers |
| Presence | Signed, short-TTL presence records pushed directly to online contacts; not published to the DHT (avoids broadcast of online status) |
| Direct messages | libp2p request-response stream per contact, E2EE payload inside |
| Offline delivery | **MVP: Nostr mailbox only. Beta: all three layers**, written in parallel, deduplicated by message ID on receipt:<br>1. **Nostr mailbox** (primary): Double-Ratchet ciphertext wrapped in a Nostr event, signed by a fresh one-time key, tagged with a rotating mailbox pubkey derived from `HKDF(shared_secret, epoch)`, NIP-40 expiration 14 days, NIP-13 PoW where relays require it. Posted to 3–5 relays from the recipient's relay list (exchanged at auth). Recipient subscribes to its current and previous epoch keys. After fetching, it sends a NIP-09 deletion request signed by the event's one-time key (the key is derived, so the recipient can reproduce it) — mailbox cleanup.<br>2. **DHT mailbox**: same envelope in Kademlia under `H(shared_secret ‖ epoch)`.<br>3. **Buddy relays**: replication to up to N mutual online contacts. |
| Nostr key hygiene | The Ed25519 account key is **never** used on Nostr. All Nostr keys (secp256k1) are ephemeral or derived per contact-pair and epoch, so relays cannot link events to an identity or to each other across epochs |
| Nostr crypto | Nostr's own NIP-04/NIP-44 DM encryption is **not** relied upon (no forward secrecy). Payload is always RIM's ratchet ciphertext |
| Groups | gossipsub topic per group, Megolm-encrypted payloads |
| File transfer | **Direct connection only** — never via relays, Nostr or mailboxes. Dedicated QUIC stream to the contact's PeerId (Noise/TLS-authenticated, so no man-in-the-middle). Payload additionally E2EE: random per-file key, XChaCha20-Poly1305 in chunked STREAM construction (64 KiB chunks), key + BLAKE3 hash delivered inside the Double Ratchet channel. Resumable by chunk index |
| Direct-connection reach | IPv6 first; UPnP-IGD / NAT-PMP / PCP port mapping (`libp2p-upnp`); DCUtR hole punching; LAN via mDNS. The relay is used only to coordinate the hole punch, never to carry file data |
| Anti-abuse | Contact requests must carry a valid invite token or introduction; plus per-peer rate limits, proof-of-work stamp, peer scoring in gossipsub |
| Optional anonymity (later) | Tor via `arti-client` as an alternate transport |

---

## 4. Identity & crypto

- **Account = Ed25519 account key.** Fingerprint and safety numbers derive from it. It signs device keys; it is never a network identity itself.
- **Device = own Ed25519 device key** → the device's libp2p PeerId and Olm identity. Every install is a device, even a single-device account, so multi-device needs no protocol change.
- **Contacts are added only by full key** (invite link / QR / pasted ID). No short numeric or lookup handle exists.
- **Invite tokens**: each invite link carries the full key plus a random token with a use count and expiry, stored locally by the issuer. A contact request without a valid, unspent token (or an introduction) is dropped before the user sees it. Tokens are listed and revocable.
- **Introductions**: a contact card signed by the introducer's account key, stating the introducee's account key and the introducer's verification level for it. The recipient stores who vouched; a vouch never overrides a direct verification or a key-change warning.
- **Names are local petnames.** A peer's self-chosen nickname is shown as a suggestion when adding; the name in the buddy list is always the one the user assigned, so look-alike nicknames cannot impersonate.
- **Safety numbers** (Signal-style 60-digit / QR) for out-of-band verification; verified contacts marked in UI; key change blocks sending until acknowledged.
### 4.1 Multi-device (one account on desktop, laptop, …)

- **Device list**: a versioned, account-key-signed list of device keys (+ OS, device class, name). Sent to authorized contacts and to own devices; newest version wins. A contact's new device signed by their account key is **not** a key change — no warning. An unsigned or foreign-signed device is rejected.
- **Linking**: new device shows a QR / code → existing device scans it → direct connection → both show a 6-word short authentication string to compare → existing device signs the new device key and hands over account data.
- **Account-key custody**: the account key sits only on devices marked **manager** (default: the first device; user can promote others). Managers can link and revoke. Plain devices hold only their device key, so a stolen laptop cannot add devices or impersonate the account after revocation.
- **Revocation**: manager publishes a new device list without the device; contacts drop its sessions; rendezvous and mailbox keys rotate.
- **Remote logout** — signed command to one of the account's own devices, delivered directly if online, otherwise via that device's Nostr/DHT mailbox so it executes on next connect:
  - **Lock**: device drops its unlocked DB key and requires the passphrase. Stays linked. Any own device may send it.
  - **Log out & wipe**: revocation (above) **plus** a wipe command. The device destroys its key material (device key, DB key, keychain entry) = crypto-erase of the SQLCipher DB, then deletes the files. Manager-only, signed by the account key.
  - Device replies with a signed acknowledgement; device manager shows *pending* / *done*.
  - Wipe relies on the device running an honest RIM client. Revocation does not: it takes effect at contacts regardless, so a stolen or offline device can neither receive new messages nor send as the account.
- **Message fan-out**: each message is encrypted per device (Olm session per device pair) to all of the recipient's devices **and** the sender's other devices, so every device has the full conversation. Offline devices receive via per-device Nostr/DHT mailboxes.
- **State sync between own devices**: contact list, petnames, verification state, groups, settings, read markers = **Automerge** CRDT (`automerge` crate), synced over direct connection between own devices, or as encrypted self-addressed mailbox messages when they are never online together. Conflict-free, no "primary" needed.
- **History sync to a newly linked device**: bulk transfer over the direct connection (same path and encryption as file transfer). Optional; the user picks "all", "last N days" or "none".

### 4.2 Backup & restore

- **One file, everything**: `*.rimbackup` contains the account key, device list, contacts incl. petnames and verification state, groups + Megolm keys (old group history stays readable), settings, full message history, and optionally received files.
- **Encryption**: Argon2id (tuned to ~1 s) → XChaCha20-Poly1305 in chunked STREAM construction. Unlock by passphrase or by a generated 24-word **recovery key** (user chooses; recovery key recommended).
- **Scheduled backups**: optional, to any folder (local disk, NAS, a synced cloud folder — the file is opaque there), keeping the last N.
- **Ratchet state is deliberately not restored.** Restoring old Double Ratchet state would reuse keys. A restore creates a new device (signed by the restored account key) and opens fresh sessions; contacts see a new device, not a key change.
- **Restore modes**: full restore onto a new machine (becomes a manager), or import history only into an existing device.
- No cloud service, no RIM server involvement.

### 4.3 Note to self, urgent, bots

- **Note to self**: a conversation whose recipient is the own account; fan-out goes to own devices only. Files in it use the direct device-to-device path.
- **Urgent flag**: a signed field in the message envelope. The receiving client enforces the per-contact opt-out and the rate limit (3/h), so a sender cannot force it.
- **Bot mode** (`rim-cli --bot`): a separate account on a headless machine. Interfaces: `rim-cli send`, stdin, and a local JSON API on a Unix socket / Windows named pipe (no TCP listener, so nothing is exposed to the network). Commands accepted only from an allowlist of authorized contacts. Presence carries a `bot` flag → bot badge in the buddy list.

---

## 5. UI stack details

- **Slint** `.slint` components compiled at build time (`slint-build`), Rust callbacks for actions.
- **Plain text only.** No rich text, no markdown rendering, no emoji picker, no stickers, no inline images, no link previews. Text is displayed exactly as typed. Unicode passes through unchanged; bundled fonts are monochrome, so a pasted emoji renders as a plain glyph, never as colour art. Classic text emoticons (`:-)`) stay text.
- Look: nerdy and retro — bundled monospace / pixel-style fonts, bevelled 2000s window chrome drawn in Slint, hard edges. Themes via Slint global style tokens: **Classic** (ICQ-era, not copied), **Terminal** (green or amber on black), **Plain light**.
- Windows: `buddy-list` (main, narrow, dockable), one `chat-<contact>` window per conversation (or tabbed mode), `note-to-self`, `settings`, `devices`, `invites`, `file-transfer`.
- Chat log: virtualised `ListView` of plain-text rows; selection and copy via read-only `TextInput` per message; search highlights by row.
- Bridge: UI and `rim-core` talk over channels; core events are marshalled onto the UI thread with `invoke_from_event_loop`.
- Accessibility: Slint's AccessKit integration (screen readers); full keyboard navigation.

### 5.1 Edge docking (buddy list)

Slint has no docking API; implemented per platform in `app/src/dock/`, using the native window handle from Slint's winit backend, behind one trait (`dock(edge)`, `undock()`, `set_autohide(bool)`).

| Platform | Mechanism | Reserves screen space? |
|---|---|---|
| Windows 10/11 **(MVP)** | Win32 **AppBar** (`SHAppBarMessage`: `ABM_NEW`, `ABM_SETPOS`, `ABN_POSCHANGED`) via `windows` crate; auto-hide via `ABM_SETAUTOHIDEBAREX` | Yes — maximised windows shrink, like ICQ |
| Linux X11 **(MVP)** | `_NET_WM_WINDOW_TYPE_DOCK` + `_NET_WM_STRUT_PARTIAL` via `x11rb` | Yes |
| Linux Wayland (KDE, Sway, Hyprland, other wlroots) (Beta) | `wlr-layer-shell` with exclusive zone via `smithay-client-toolkit`. winit cannot create layer-shell surfaces, so the buddy list needs a custom Slint platform backend (`slint::platform`) on Wayland — largest docking work item | Yes |
| Linux Wayland (GNOME) (Beta) | No layer-shell. Fallback: edge-snapped, always-on-top, auto-hide on mouse-leave | No |
| macOS (Beta) | No public API to reserve screen space. `NSWindow` at a floating level, snapped to edge, `canJoinAllSpaces`, auto-hide with edge-hover reveal (`objc2` / `cocoa`) | No |

Multi-monitor: dock to any monitor edge; position and monitor persisted, re-applied on display change (DPI/hot-plug events).

### 5.2 Sidebar gadgets

Every gadget is a **plugin**. Not enabled = not loaded: no process, no code, no network.

**Plugin model: out-of-process.**
- Each plugin is its own executable, started by `rim-plugin-host` only when enabled, stopped when disabled. It runs in a separate OS process, so it cannot read RIM's memory — keys, sessions and history stay unreachable even if the plugin is buggy or malicious. A crashing plugin cannot take RIM down.
- Why not in-process: Rust has no stable ABI for `.dll`/`.so` plugins, and any native code loaded into RIM's process could read key material. WASM (wasmtime) would sandbox well, but player control and keychain access need native OS APIs, which a WASM guest cannot call without the host re-implementing them.
- **IPC**: length-prefixed protobuf over a Unix socket / Windows named pipe created per plugin, local only.
- **UI**: plugin ships a `.slint` file; RIM renders it with `slint-interpreter` in the sidebar and binds its properties/callbacks to IPC messages. Slint markup is declarative, so the plugin UI cannot execute arbitrary code inside RIM. The interpreter itself is only initialised when the first plugin loads.
- **Manifest** (`plugin.toml`): id, version, author, signature, and **capabilities** requested — e.g. `presence.set`, `presence.now_playing`, `chat.context_menu` (receives the text of a message only when the user clicks "Add as task"), `settings.sync`, `network:<host>`. The host enforces them: undeclared calls are refused. Capabilities shown to the user on enable.
- **Signing**: v1 loads only plugins signed with the RogueIM release key (first-party: pomodoro, todo, player). Third-party plugins: later, with user-confirmed signatures and the same capability prompt.
- **Credentials**: plugins store service tokens through a host-mediated keychain call scoped to the plugin id; they can't read other plugins' secrets.

| Gadget | Implementation |
|---|---|
| Pomodoro | Local state machine; drives presence via core callback (GD-3). State synced to own devices as part of the Automerge settings doc |
| Todo — todo.txt | Plain file, watched with `notify`; parsed/written per the todo.txt format |
| Todo — CalDAV | VTODO over CalDAV (`reqwest` + `icalendar` crate, or a CalDAV client crate chosen in M2); Basic/Digest auth, credentials in keychain |
| Todo — Todoist / MS To Do / Google Tasks (P2) | REST via `reqwest`; OAuth 2 PKCE with loopback redirect (`oauth2` crate); tokens in keychain |
| Player — Windows | System Media Transport Controls: `GlobalSystemMediaTransportControlsSessionManager` via `windows` crate — reads metadata and controls any registered player |
| Player — Linux | MPRIS over D-Bus via `zbus` (or `mpris` crate) — any MPRIS player |
| Player — macOS | No public API for other apps' now-playing info. Media keys posted via `CGEvent` (Accessibility permission) for play/pause/next; AppleScript (`osascript`) for Spotify and Music.app to get track info |
| Now playing as status | Player gadget → optional `now_playing` field in the signed presence record, authorized contacts only |

### 5.3 OS identity icon

- Local OS detected with `os_info` (Windows / macOS / Linux + distro from `/etc/os-release`; iOS / Android on mobile builds).
- **Device class** `desktop` | `laptop` | `mobile`:
  - `mobile`: set at compile time by the mobile companion build.
  - `laptop` vs `desktop`: chassis type — Windows `Win32_SystemEnclosure.ChassisTypes` (WMI), Linux `/sys/class/dmi/id/chassis_type`, macOS model identifier (`MacBook*`). Fallback: battery present (`starship-battery`). Chassis first, because desktops on a UPS can report a battery.
  - Laptops also report `on_battery` (bool), re-sent on change.
- Sent as `os`, `device_class`, `on_battery` in the signed presence record, **only to authorized contacts**.
- Rendered as the contact's status icon in place of ICQ's flower: OS glyph inside a **frame that shows device class** — monitor outline (desktop), laptop outline (laptop), phone outline (mobile) — tinted by status colour, with a small status-shape badge (so status never depends on colour alone).
- Own tray / dock icon = own OS glyph in own status.
- Icons: original monochrome glyphs drawn for RIM, not vendor logo files (see §7).

---

## 6. Quality & tooling

| Area | Tool |
|---|---|
| Rust tests | `cargo nextest`, `proptest` for protocol/state machines |
| Network simulation | Multi-node integration tests with in-memory libp2p transport; `testground`-style NAT scenarios in Docker |
| Crypto | Known-answer tests, `cargo fuzz` on all decoders |
| UI tests | Slint testing backend (`i-slint-backend-testing`) for component tests; screenshot tests per theme |
| Lint | `clippy -D warnings`, `rustfmt`, `cargo deny` (licenses/advisories), `cargo audit`, `slint-lsp` format check |
| CI | GitHub Actions; `cargo-packager` builds Win (MSI/NSIS), macOS (.dmg), Linux (AppImage/.deb) |
| Release | Reproducible builds goal; Authenticode (Win), notarization (macOS), AppImage + .deb + Flatpak (Linux); updater signatures |
| Node distribution | Multi-arch Docker image (amd64/arm64) and arm64 .deb for `rim-cli --node`, built in CI |

---

## 7. Licensing & legal

- License: **GPL-3.0-or-later** (forks must publish source; AGPL's network clause adds nothing without servers). All deps checked by `cargo deny`. Note: GPL excludes the Mac App Store; distribution is direct download. vodozemac, libp2p and rust-nostr are Apache-2.0/MIT; Slint is used under its GPLv3 option — all compatible.
- OS glyphs are nominative use of trademarks. Apple's and Microsoft's guidelines restrict use of their logos in third-party apps — have the glyph set reviewed before release. Tux is free to use.
- Name: **RogueIM**; "RIM" is the internal short form. Run a trademark search before the first public release (BlackBerry's former name was Research In Motion / RIM).
- **Do not** use ICQ trademarks, the flower logo, or the "Uh-oh!" sample. Commission/produce original sounds with the same function.
- Crypto export: open-source publication exemption (EAR 742.15(b)) — file the notification before first public release.

---

## 8. Targets

| Platform | Minimum |
|---|---|
| Windows | 10 |
| macOS | 12 (Intel + Apple Silicon) |
| Linux | x86_64/aarch64, X11 or Wayland |
| Headless node | Linux x86_64/aarch64 (Raspberry Pi 4/5) |