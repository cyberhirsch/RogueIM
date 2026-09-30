# RogueIM (RIM)

Serverless, end-to-end encrypted instant messenger for the desktop, in the spirit of ICQ.
Plain text, docked buddy list, no accounts on anyone's server.

Specs: [docs/PRD.md](docs/PRD.md) · [docs/TECH_STACK.md](docs/TECH_STACK.md) · [docs/protocol/PROTOCOL.md](docs/protocol/PROTOCOL.md)

> **Status: v0.1 alpha.** All P0 and P1 features of the PRD are implemented. The code has not been
> audited and releases are not code-signed. Do not rely on it for real secrets yet.
> The protocol may still change incompatibly before 1.0.

## Download

Prebuilt archives for Windows x64, Linux x64/arm64 and macOS (universal) are on the
[releases page](https://github.com/cyberhirsch/RogueIM/releases). Unpack the archive and start
`rogueim`. Keep the `rim-plugin-*` files next to it if you want the sidebar gadgets.
The `sounds/` folder holds RIM's five built-in sounds as WAV files, to reuse or edit. In settings → alerts,
each event can use any built-in sound or your own file (wav, mp3, ogg, flac).

macOS: open the `.dmg` and drag RogueIM to Applications. The app is not notarized (no Apple developer
account), so the first start is blocked. Open System Settings → Privacy & Security and click "Open Anyway"
(on macOS 14 and older, right-click the app and choose Open). The player gadget controls players other than
Music and Spotify through the media keys; that needs RogueIM under Privacy & Security → Accessibility.
macOS cannot reserve screen space, so maximised windows go under the bar; "auto-hide the bar" helps.

Linux runtime needs GTK 3, libayatana-appindicator3, libxdo and ALSA (usually preinstalled on desktops).

### Updates

From v0.1.4 on, RIM checks GitHub for new releases (settings → general; can be switched off). It installs
only releases whose files match a checksum list signed with the RogueIM release key, then restarts itself.
Automatic installation is off by default: you get a notice with an "install" button. Inside a
system folder like Program Files, RIM cannot replace itself; unpack it somewhere you own.

## Run from source

```
cargo run -p rogueim --release                       # profile "default"
cargo run -p rogueim --release -- --profile alice    # separate identity on the same machine
```

First start offers three tabs:

- **create**: a new account. You get a 24-word recovery key; write it down. It is the only way to
  restore a backup without the passphrase.
- **link**: add this computer to an existing account. Shows a `rimlink2:` code plus four check words;
  paste the code on a device you already use (settings → devices → link) and compare the words.
- **restore**: a new device from a `.rimb` backup file, using the passphrase or the recovery words.

There is no password reset: the state file is encrypted with the passphrase.

To add someone: **invite** → **copy** → send them the `rim2:…` code; they paste it under **+ add**.
You accept their authorization request, then you both see each other's status.
Invites are single-use by default, expire, and carry a small proof of work.

## What works in v0.1

| Area | |
|---|---|
| Identity | Ed25519 account key; per-device keys; fingerprints and safety numbers; no central ID |
| Contacts | Single-use or expiring invites, ICQ-style authorization, introductions ("send contact"), folders, visible/invisible lists, ignore |
| Presence | Online, Free for Chat, Away, N/A, Occupied, DND, Invisible; auto-away/N/A; OS icon with desktop/laptop/**phone** frame per device |
| Messages | Plain text only. Delivery states, typing, read receipts (opt-in), edit/delete, reply, urgent flag, auto-reply, disappearing messages, search, note to self |
| Offline delivery | Nostr mailbox (sealed, expiring, deleted after fetch), buddy relays, DHT slots |
| Groups | Invite, add/remove with key rotation, rename, leave; signed group state |
| Files | Direct connections only, encrypted, BLAKE3-verified, resumable |
| Multi-device | Link devices, sync contacts/settings/history/read state, rename, remote lock, revoke (the revoked device wipes its local data) |
| Backup | Encrypted `.rimb` files (manual or scheduled), restore via passphrase or recovery words, history import |
| Network | libp2p: QUIC + TCP/Noise, mDNS, Kademlia DHT, AutoNAT, relay v2, UPnP, optional helper mode, LAN-only mode. Behind two home routers: hole punching coordinated over Nostr and via public libp2p helper nodes, no port forwarding needed in most cases |
| Without a direct path | Messages over a live Nostr subscription (about a second), presence over short-lived Nostr signals |
| Desktop | Always-on-top bar docked to any screen edge on any monitor (Windows AppBar reserves the strip; X11 struts), auto-hide, frameless chat windows (pin, dock into the bar), drag & drop files onto chats, tray with your own OS icon, notifications, per-event sounds, rebindable global hotkeys, autostart (optionally hidden), Away on screen lock, idle lock, optional passphrase in the OS keychain |
| Themes | graphite (default), grey, green, amber; bundled Hack monospace font |
| Plugins | Separate processes, loaded only when enabled: pomodoro, todo (todo.txt or CalDAV), player controls (Windows SMTC, Linux MPRIS, macOS Music/Spotify) with optional "now playing" status |
| Headless | `rim-cli` as node (relay/DHT/mailbox helper), bot with a local JSON API, one-shot sender |

## Known limitations of the alpha

- Tested by hand on Windows 10 only. Linux and macOS builds compile in CI, but docking, idle detection
  and the player plugin have not been exercised there. Wayland docking works through XWayland only.
- Hole punching fails behind "symmetric" NAT (common on mobile networks and some CGNAT lines). Then chat and
  presence still work over Nostr, but files need a direct path: UPnP, a port forward, or your own node
  (see below) entered under settings → net → bootstrap.
- The CalDAV mode of the todo plugin has not been tested against a real server yet.
- "Remember on this computer" puts the passphrase in the OS keychain; anyone logged in as you can then open RIM. A remote lock removes it.
- Only the three first-party plugins are loaded. Third-party plugins and plugin signing come later.
- Changing helper/relay mode takes effect after a restart.
- Profiles from the pre-alpha prototype are archived as `state.prototype.rim`; contacts have to be added again.
- No installers yet: archives only.

## Run a node

A node is an always-on peer that helps your friends reach each other (relay, DHT, store-and-forward).

```
docker build -f deploy/Dockerfile -t rogueim-node .
docker run -d --name rim-node -p 4001:4001/tcp -p 4001:4001/udp -v rim-node:/data -e RIM_PASS='change me' rogueim-node
```

On Debian/Raspberry Pi OS, use the `rogueim-node-*.deb` package from the release. Put `RIM_PASS=…` into
`/etc/rogueim/node.env`, then run `systemctl enable --now rogueim-node`.

## Bots and scripts

```
rim-cli --dir ./bot --pass botpass --nick echo-bot --invite --accept-all --echo
rim-cli --bot --api mybot --allow alice,bob --dir ./bot --pass x --nick backupbot
rim-cli send --dir ./bot --pass x --to alice "backup finished"
some-command | rim-cli --dir ./bot --pass x --stdin-to alice
```

`--api NAME` opens a local socket / named pipe `rogueim-NAME` that speaks JSON lines
(`{"cmd":"send","to":"alice","text":"hi"}`). It never listens on a network port.

## Layout

```
crates/rim-core         engine: identity, Olm/Megolm, libp2p swarm, Nostr mailbox, encrypted store
crates/rim-cli          headless peer: node, bot, scripting
crates/rim-plugin-sdk   plugin protocol (JSON lines over a local socket)
app/                    Slint desktop app (ui/*.slint, src/dock.rs = docking)
plugins/                pomodoro, todo, player
deploy/                 Dockerfile, systemd unit, .deb scripts
docs/                   PRD, tech stack, protocol spec
```

## Tests

```
cargo test -p rim-core -p rim-plugin-pomodoro -p rim-plugin-player
cargo test -p rim-core -- --ignored     # live tests against public Nostr relays
```

The engine tests run pairs of full engines on loopback: invites, authorization, presence, chat, restarts,
multi-device linking and sync, remote lock, direct files, groups with removal, backup and restore as a
new device, and introductions. Property tests feed malformed input to every decoder.

License: GPL-3.0-or-later.
