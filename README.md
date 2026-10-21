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

Windows: run `RogueIM-windows-x64-setup.exe` (installs for your user, no admin rights; Windows SmartScreen may ask once because the installer is not code-signed). The `.zip` is the portable version.

macOS: open the `.dmg` and drag RogueIM to Applications. Everything, including `rim-cli`, is inside the app.
Without the "unidentified developer" warning, install from the Terminal instead (the download is checked
against the release's checksums):

```
curl -fsSL https://raw.githubusercontent.com/cyberhirsch/RogueIM/main/deploy/macos/install.sh | sh
``` The app is not notarized (no Apple developer
account), so the first start is blocked. Open System Settings → Privacy & Security and click "Open Anyway"
(on macOS 14 and older, right-click the app and choose Open). The player gadget controls players other than
Music and Spotify through the media keys; that needs RogueIM under Privacy & Security → Accessibility.
macOS cannot reserve screen space, so maximised windows go under the bar. Either turn on "auto-hide the bar", or
"make room: move other windows beside the bar" (settings → general, Mac only): about once a second it moves or
narrows windows that reach under the bar. That too needs RogueIM under Privacy & Security → Accessibility.

Linux runtime needs GTK 3, libayatana-appindicator3, libxdo and ALSA (usually preinstalled on desktops).

### Updates

From v0.1.4 on, RIM checks GitHub for new releases (settings → general; can be switched off). It installs
only releases whose files match a checksum list signed with the RogueIM release key, then restarts itself.
Automatic installation is on by default (from v0.1.10; earlier profiles are switched over once); untick "install
updates automatically" to get a notice with an "install" button instead. Inside a
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

There is no password reset: the state file is encrypted with the passphrase. If you forgot it, **forgot the
passphrase?** under unlock sets the profile aside (renamed to `default.old-<date>`, nothing is deleted) and offers the
three tabs again: link this computer from a device you use, or restore a backup with the recovery words.

To add someone: **invite** → **copy** → send them the `rim2:…` code; they paste it under **+ add**.
You accept their authorization request, then you both see each other's status.
Invites are single-use by default, expire, and carry a small proof of work.

## What works in v0.1

| Area | |
|---|---|
| Identity | Ed25519 account key; per-device keys; fingerprints and safety numbers; no central ID |
| Contacts | Single-use or expiring invites, ICQ-style authorization, introductions ("send contact"), folders, visible/invisible lists, ignore |
| Presence | Online, Free for Chat, Away, N/A, Occupied, DND, Invisible; auto-away/N/A; OS icon with desktop/laptop/**phone** frame per device |
| Messages | Plain text, plus pictures pasted with Ctrl+V. Delivery states, typing, read receipts (opt-in), edit/delete, reply, urgent flag, auto-reply, disappearing messages, search, note to self |
| Offline delivery | Nostr mailbox (sealed, expiring, deleted after fetch), buddy relays, DHT slots |
| Voice calls | 1:1, end-to-end encrypted, Opus with echo cancellation (speexdsp) and noise suppression (RNNoise), over the direct connection; screen sharing during a call (view only, choose the screen); no camera, no group calls |
| Groups | Invite, add/remove with key rotation, rename, leave; signed group state |
| Files | Direct connections only, encrypted, BLAKE3-verified, resumable; to anyone else through **send to someone** (below) |
| Transport honesty | Every buddy shows how its conversation travels right now: **A** direct, **B** end-to-end via relay or mailbox, **C** readable by a service in between (other networks, bots), **D** not encrypted (mail). Hover for the meaning |
| Multi-device | Link devices, sync contacts/settings/history/read state, rename, remote lock, revoke (the revoked device wipes its local data) |
| Backup | Encrypted `.rimb` files (manual or scheduled), restore via passphrase or recovery words, history import |
| Network | libp2p: QUIC + TCP/Noise, mDNS, Kademlia DHT, AutoNAT, relay v2, UPnP, optional helper mode, LAN-only mode. Behind two home routers: hole punching coordinated over Nostr and via public libp2p helper nodes, no port forwarding needed in most cases |
| Without a direct path | Messages over a live Nostr subscription (about a second), presence over short-lived Nostr signals |
| Desktop | Always-on-top bar docked to any screen edge on any monitor (Windows AppBar reserves the strip; X11 struts), auto-hide, frameless chat windows (pin, dock into the bar; or open every chat in the bar: settings → general), drag & drop files onto chats, tray with your own OS icon, notifications, per-event sounds, rebindable global hotkeys, autostart (optionally hidden), Away on screen lock, idle lock, optional passphrase in the OS keychain |
| Themes | graphite (default), grey, green, amber, phosphor, solar, dos, paper, or custom from three colours (background, text, accent); the theme follows you to your other devices; bundled Hack monospace font |
| Plugins | Separate processes, loaded only when enabled: pomodoro, todo (todo.txt or CalDAV), player controls (Windows SMTC, Linux MPRIS, macOS Music/Spotify) with optional "now playing" status, mail, WhatsApp, Telegram, Signal and your Android phone's messages (see below) |
| Headless | `rim-cli` as node (relay/DHT/mailbox helper), bot with a local JSON API, one-shot sender |

## Known limitations of the alpha

- Tested by hand mostly on Windows 10, macOS only briefly. Linux builds compile in CI, but docking, idle detection
  and the player plugin have not been exercised there. Wayland docking works through XWayland only.
- Hole punching fails behind "symmetric" NAT (common on mobile networks and some CGNAT lines). Then chat and
  presence still work over Nostr, but files need a direct path: UPnP, a port forward, or your own node
  (see below) entered under settings → net → bootstrap.
- The CalDAV mode of the todo plugin has not been tested against a real server yet.
- "Remember on this computer" puts the passphrase in the OS keychain; anyone logged in as you can then open RIM. A remote lock removes it.
- Only the first-party plugins are loaded. Third-party plugins and plugin signing come later.
- The mail plugin has been tested against a local test server (GreenMail) only, not yet against real mail providers.
- The WhatsApp plugin is tested up to the QR code; linking a real account has not been tested yet.
- The Telegram plugin is tested up to Telegram turning down a made-up API key; logging in with a real account has not
  been tested yet.
- The Signal plugin is tested up to Signal handing out the link code; linking a real phone has not been tested yet.
- The phone app is tested in the Android emulator with Google Messages (SMS in, reply out), not yet on a real phone.
- Changing helper/relay mode takes effect after a restart.
- Profiles from the pre-alpha prototype are archived as `state.prototype.rim`; contacts have to be added again.

## Send to someone

The row under **note to self**. Click it, type a name or a network, pick someone from any list (RIM contacts,
groups, mail, WhatsApp, Telegram, Signal, phone) and send a message, a file, or both. Dropping a file on the bar
(with no chat docked) starts here too. Each recipient shows its level (A–D).

- A RIM contact with a direct connection (A) gets the file directly, as always.
- Everyone else gets a link. The file is encrypted on your computer (AES-256-GCM, 1 MiB chunks), only the ciphertext
  goes to the **drop**, and the key is in the link after the `#`, which browsers never send to a server. The
  recipient opens the link and their browser decrypts it ([download page](site/f/), on GitHub Pages). Files of any
  size; very big ones stream to disk in Chromium-based browsers.
- **Whoever can read the link can open the file.** Sent over RIM (B) only the recipient can; sent by mail (D), anyone
  who can read that mail can too. The panel says which before you send.
- The drop deletes files after 1, 7 or 30 days (settings → net). It needs a token, kept in the OS keychain.

Run your own drop (e.g. on a Raspberry Pi behind a Cloudflare tunnel, no open port):

```
cd deploy/drop
cp .env.example .env     # TUNNEL_TOKEN, RIM_DROP_TOKEN (at least 24 characters), where the files go
docker compose up -d --build
```

Then enter its address and the token under settings → net → send to someone. `rim-drop` keeps nothing but
ciphertext and a byte count, enforces a total quota and a per-file limit, and sweeps expired files every ten minutes.

## Mail

Settings → plugins → **mail**: your email in the buddy list. Enter your address and password; the servers are filled
in for common providers (Gmail, iCloud and Yahoo need an app password; Outlook.com no longer lets other programs log in
with a password). The password is kept in the OS keychain. RogueIM stores no mail on disk.

- The people you wrote to (the last 500 sent mails) appear in a **mail** folder with an envelope icon. Paste an email
  address under **+ add** to add someone; right-click to take someone off the list (nothing is deleted at the provider).
- One person is one chat: their latest mails and yours, quotes and signatures cut, attachments named but not loaded.
  Opening the chat marks their mail read. What you write goes out as a reply in the same thread, with a copy in "Sent".
- New mail is checked every minute. Mail from someone on the list rings and pops up like a RogueIM message.
- Mail is not end-to-end encrypted: your provider and the recipient's can read it. Every mail chat says so, the input is
  tagged `[mail]`, and the first mail you send asks once.

## WhatsApp

Settings → plugins → **WhatsApp**: your WhatsApp chats in the buddy list, in RogueIM's own chat windows.

- **Read the warning first.** WhatsApp has no official way in for other programs. The plugin links RogueIM like
  WhatsApp Web, through the unofficial library [whatsmeow](https://github.com/tulir/whatsmeow). That breaks WhatsApp's
  terms: WhatsApp can block your number, for a while or for good. Nothing connects until you accept this in the plugin.
- Link by scanning the QR code in the sidebar (WhatsApp → Settings → Linked devices), or with an 8-digit code: type
  your number into the plugin and enter the code on your phone. "unlink" removes RogueIM from your phone again.
- Chats from the last 60 days appear in a **whatsapp** folder, groups included. Text and pictures show in the chat;
  voice messages, videos, files and locations become a line with a link (`[voice message 0:12] [play]`). You can send
  text, pictures (Ctrl+V or **+**) and files. Opening a chat marks it read on WhatsApp too.
- Level C: WhatsApp encrypts end to end, this computer decrypts. WhatsApp's servers see who you write to and when.
  The link, keys and a copy of your messages stay in the plugin's folder in your RogueIM profile.

## Telegram

Settings → plugins → **Telegram**: your Telegram chats in the buddy list, through Telegram's own client API
([gotd](https://github.com/gotd/td)). Telegram allows other programs, with a key of their own.

- Get the key once: log in at [my.telegram.org](https://my.telegram.org) → API development tools, create an app (any
  name), and enter its `api_id` and `api_hash` in the plugin. The key stays in the plugin's folder.
- Then scan the QR code in the sidebar: Telegram → Settings → Devices → Link Desktop Device. With two-step
  verification, the plugin asks for your cloud password. "sign out" ends the session on Telegram too.
- Chats from the last 60 days appear in a **telegram** folder: people, bots, groups, Saved Messages; channels are left
  out. Telegram keeps the history, so the plugin stores no messages; pictures you open are kept in its folder.
  Pictures, files, voice messages and locations work as with WhatsApp. Opening a chat marks it read on Telegram.
- Level D: Telegram's chats are not end-to-end encrypted; Telegram's servers can read them. Secret chats exist only
  in Telegram's phone apps and do not appear.

## Signal

Settings → plugins → **Signal**: your Signal chats in the buddy list. RogueIM becomes a linked device of your
Signal account, like Signal Desktop, through the unofficial library [presage](https://github.com/whisperfish/presage).

- Click **link** and scan the code: Signal → Settings → Linked devices → Link new device. "unlink" removes RogueIM
  from your account again.
- Signal does not hand older messages to a new device: chats fill from the moment of linking. Contacts come from your
  phone; add a chat under **+ add** by a name or number from them (groups by name).
- Text, pictures, files, voice messages and stickers; pictures (Ctrl+V or **+**) and files can be sent. Opening a chat
  marks it read on your other devices; no read receipts go to the sender.
- Level C: Signal encrypts end to end, this computer decrypts. Keys, contacts and messages stay in the plugin's
  folder. Signal does not support presage but tolerates such clients.
- The plugin is AGPL-3.0, like the libraries it uses; the rest of RogueIM is GPL-3.0-or-later.

## Phone (Android)

Settings → plugins → **Phone**, plus the **RogueIM Phone** app (`RogueIM-Phone.apk` with each release): the messages
your Android phone shows as notifications appear in RogueIM, and you can answer them. It touches no messenger's
protocol, so no account can be blocked for it, and it works for WhatsApp, Signal, SMS, Telegram and more alike.

- Pair once: scan the QR code from the plugin with the app, then allow the app to read notifications. Phone and
  computer talk directly in your own network, encrypted with the key from the QR code; there is no server.
- Each conversation becomes a contact in a **phone** folder (`Anna` · WhatsApp). Answers go through the notification's
  reply button; opening a chat marks it read on the phone where the app offers that.
- Limits: only what arrives as a notification (no history, no pictures), and only while the phone is on and in the
  same network. If you dismissed the notification on the phone, answer there.
- Windows asks once whether `rim-plugin-phone` may receive connections: allow it for private networks.
- RogueIM Phone is a helper, not RogueIM itself. The way to a full RogueIM on Android is in
  [docs/ANDROID_ROADMAP.md](docs/ANDROID_ROADMAP.md).

## Run a node

A node is an always-on peer that helps your friends reach each other (relay, DHT, store-and-forward).

```
docker build -f deploy/Dockerfile -t rogueim-node .
docker run -d --name rim-node -p 4001:4001/tcp -p 4001:4001/udp -v rim-node:/data -e RIM_PASS='change me' rogueim-node
```

On Debian/Raspberry Pi OS, use the `rogueim-node-*.deb` package from the release. Put `RIM_PASS=…` into
`/etc/rogueim/node.env`, then run `systemctl enable --now rogueim-node`.

## AI bot

`rim-llm-bot` is a RogueIM contact that answers with a language model (Google Gemini's free tier by default, or any
OpenAI-compatible API). It accepts everyone, warns each new contact that the conversation goes unencrypted to the
model provider, keeps per-user and daily limits, and mentions every few replies that ads can be booked. Writing
`ads` explains the offer; `ad: <text> / <contact>` stores an ad request in `ad-requests.jsonl` next to the bot's
profile (time, name, fingerprint, text) for the operator to review. Personality,
ad text and limits live in a `bot.json` (see `deploy/llm-bot/bot.example.json`); `deploy/llm-bot/Dockerfile` runs it,
for example on a Raspberry Pi.

### Bots in the app (settings → bots)

- **Rogue**, the free AI contact, is added on first start. Switching it off removes the contact; switching it on adds it again.
- **Local AI bots**: name, endpoint, model, API key. The bot appears under "bots" in the list and talks to the
  endpoint straight from your computer, not over the RogueIM network. Any OpenAI-compatible API works, local ones too
  (e.g. Ollama at `http://localhost:11434/v1/chat/completions`). The key is kept in the OS keychain; the chat stays in
  memory only.
- **Script API**: when on, programs on this computer can connect to the local socket / named pipe
  `rogueim-app-<profile>` and use the same JSON lines as `rim-cli --api` (below): read incoming messages, send as you,
  set your status.

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
crates/rim-drop         send to someone: file format, upload client, the drop server
site/                   GitHub Pages: invite page (i/), download page that decrypts in the browser (f/)
plugins/                pomodoro, todo, player, mail, phone, whatsapp and telegram (Go), signal (own workspace)
android/                RogueIM Phone, the Android app for the phone plugin
deploy/                 Dockerfiles (node, AI bot, drop), systemd unit, .deb scripts
docs/                   PRD, tech stack, protocol spec
```

## Tests

```
cargo test -p rim-core -p rim-plugin-pomodoro -p rim-plugin-player -p rim-plugin-mail
cargo test -p rogueim -p rim-drop --features rim-drop/server,rim-drop/client
node site/f/test.js                     # the browser decryptor against rim-drop's known answer
cargo test -p rim-core -- --ignored     # live tests against public Nostr relays
```

The engine tests run pairs of full engines on loopback: invites, authorization, presence, chat, restarts,
multi-device linking and sync, remote lock, direct files, groups with removal, backup and restore as a
new device, and introductions. Property tests feed malformed input to every decoder.

License: GPL-3.0-or-later.
