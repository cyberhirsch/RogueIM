# RogueIM (RIM)

Serverless, end-to-end encrypted instant messenger for the desktop, in the spirit of ICQ.
Plain text, docked buddy list, no accounts on anyone's server.

Specs: [docs/PRD.md](docs/PRD.md) · [docs/TECH_STACK.md](docs/TECH_STACK.md)

> **Status: prototype.** Works between machines that can reach each other directly
> (same LAN, or open ports). Not audited — do not rely on it for real secrets yet.

## Run

```
cargo run -p rogueim --release                       # profile "default"
cargo run -p rogueim --release -- --profile alice    # separate identity on the same machine
```

First start creates an account (nickname + passphrase). Afterwards the passphrase unlocks it.
There is no password reset: the state file is encrypted with it.

To add someone: **invite** → **copy** → send them the `rim1:…` code; they paste it under **+ add**.
You accept their authorization request, then you both see each other's status.

### A test partner without a second machine

```
cargo run -p rim-cli -- --dir ./bot --pass botpass --nick echo-bot --invite --accept-all --echo
```

Prints a single-use invite, accepts every authorization request and echoes messages back.

## What works

| Area | Prototype |
|---|---|
| Identity | Ed25519 account key signs a separate device key (PeerId) and Olm identity; fingerprints |
| Invites | `rim1:` codes, **single-use** (token + reserved Olm one-time key); reuse is rejected |
| Authorization | ICQ-style request with message → accept / deny |
| Encryption | Olm (Double Ratchet, vodozemac) per contact; state at rest: Argon2id → XChaCha20-Poly1305 |
| Network | libp2p over QUIC + TCP/Noise, mDNS LAN discovery, direct dialing via addresses in invites/presence |
| Presence | Online, Free for Chat, Away, N/A, Occupied, DND, Invisible; OS + desktop/laptop icon; battery flag |
| Messages | Plain text, delivery states (queued `[q]` → delivered), local outbox retried until the contact is reachable |
| History | Stored encrypted, restored on unlock |
| UI | Slint; buddy list **always docked** (Windows AppBar: reserves the screen strip, maximised windows stay out of it) and always on top; left/right edge; one window per chat; themes: grey (default), graphite, green, amber |

## Not yet

Offline delivery when you are *both* not online at the same time (Nostr mailbox), NAT traversal /
relays, multi-device, backups, groups, file transfer, sounds, plugins, docking on Linux/macOS.
See the milestones in the PRD.

## Layout

```
crates/rim-core   engine: identity, Olm sessions, libp2p swarm, encrypted store
crates/rim-cli    headless peer (test partner, seed of bot mode)
app/              Slint desktop app (ui/*.slint, src/dock.rs = AppBar docking)
docs/             PRD and tech stack
```

## Tests

```
cargo test -p rim-core
```

Two engines on one machine: invite → authorize → presence → encrypted chat both ways →
delivery receipts → wrong passphrase rejected → restart with session intact; plus single-use invites.

License: GPL-3.0-or-later.
