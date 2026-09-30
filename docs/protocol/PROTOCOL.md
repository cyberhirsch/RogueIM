# RogueIM protocol — version 2 (RogueIM v0.1)

Status: **draft, alpha**. Normative for RogueIM v0.1. Not yet audited.
Source of truth for field names: `crates/rim-core/src/proto.rs`.

All JSON uses the field names below; unknown fields are ignored; binary values are standard base64 unless noted.

---

## 1. Keys and identities

| Key | Type | Where | Purpose |
|---|---|---|---|
| **Account key** | Ed25519 (libp2p keypair) | manager devices only | signs device lists and remote wipe commands |
| **Device key** | Ed25519 (libp2p keypair) | every device | libp2p identity (PeerId), signs introductions and group states |
| **Olm account** | Curve25519 identity + one-time + fallback keys (vodozemac) | every device | 1:1 end-to-end encryption |
| **Megolm session** | vodozemac `GroupSession` | per group per device | group messages |
| **Seeds** | 2 × 32 random bytes (`inbox`, `rdv`) | per device | locate its Nostr mailbox and address record |

* **Account id** = hex(SHA-256(account public key, libp2p protobuf encoding)). Used as contact id.
* **Fingerprint** (display) = first 40 hex chars of the account id, groups of 4.
* **Safety number** = for each of the two account public keys (sorted as base64 strings): SHA-512 iterated 513 times over the key bytes; take bytes `[5i..5i+5]` for i in 0..6, big-endian, mod 100000, 5 digits each. 12 groups of 5 digits; identical on both sides.

### 1.1 Device list

```json
{ "list": { "account_pk": "<b64>", "version": 7,
            "devices": [ { "peer_id": "12D3…", "curve": "<b64>", "fallback": "<b64>",
                           "name": "laptop", "os": "WIN", "class": "Laptop",
                           "added": 1759140000, "manager": false } ] },
  "sig": "<b64>" }
```

`sig` = account key signature over `"rim-devices-v2\0" || JSON(list)`. A list is accepted if the signature verifies, every `curve`/`fallback` is a valid Curve25519 key and every `peer_id` parses. A newer `version` replaces the stored one. A device appearing in a newer, valid list is **a new device, not a key change**.

### 1.2 Card

`{ "nick": "…", "devices": <signed device list> }` — what peers exchange about an account.

---

## 2. Invites and authorization

### 2.1 Invite link

`rim2:` + base64url(no padding)(JSON):

```json
{ "v": 2, "card": <card>, "device": "<peer id of the issuing device>",
  "otk": "<b64 one-time key or empty>", "token": "<32 hex>",
  "seeds": { "inbox": "<64 hex>", "rdv": "<64 hex>" },
  "addrs": ["/ip4/…/tcp/…"], "pow_bits": 12, "expires": 1759999999 }
```

* Single-use invites reserve an Olm one-time key (`otk`); multi-use invites leave it empty and the fallback key is used.
* The issuer stores `(token, uses_left, expires, revoked)` locally.

### 2.2 Authorization request

The requester opens an Olm session to the invite's device (with `otk`, else the device's fallback key) and sends `Body::AuthRequest`:

```json
{ "AuthRequest": { "token": "…", "card": <requester card>, "seeds": <requester device seeds>,
                   "text": "Please add me", "pow": 4711, "intro": null } }
```

Accepted only if **either** the token is valid (not revoked, not expired, uses left) **and** `SHA-256("rim-pow-v2" || token || account_pk || u64le(pow))` has ≥ 12 leading zero bits, **or** `intro` is a valid introduction (§7) by one of the recipient's authorized contacts that presents the *recipient* to the *requester*.

Accepting sends `Body::AuthAccept { card, seeds }`; denying sends `Body::AuthDeny`. Until accepted, nothing but the ratchet state is kept. Presence and profile are only ever sent to authorized contacts.

---

## 3. Transport

* libp2p: QUIC-v1 and TCP+Noise+Yamux; mDNS on the LAN; Kademlia (`/rim/kad/1.0.0`); identify (`/rim/id/2`); ping; AutoNAT; circuit relay v2 (client; server on nodes/helpers); DCUtR; UPnP.
* Messages: request/response `/rim/msg/2`, JSON codec. Request = **WireReq**, response `{ "ok": bool }`.
* Files: request/response `/rim/file/1` (§8), **direct connections only**.

### 3.1 WireReq

```json
{ "sender_curve": "<b64 Olm identity key of the sending device>",
  "msg": { "type": 0|1, "body": "<b64>" } }
```

`msg` is a vodozemac `OlmMessage` (0 = pre-key, 1 = normal). The receiver looks the sender up by `sender_curve` among its own devices, contacts' devices, pending requesters and group members; tries each stored session (up to 4 per device, most recent first); a pre-key message that none decrypts opens a new inbound session.

### 3.2 Envelope

The Olm plaintext is:

```json
{ "account": "<b64 account pk>", "device": "<peer id>", "body": <Body> }
```

`device` must equal the device that owns `sender_curve` in the sender's device list, and (for direct delivery) the connection's PeerId. `account` must match the owner.

### 3.3 Bodies

`Body` is an externally tagged enum. Messaging: `Text{id,ts,text,reply_to,urgent,ttl}`, `Edit{id,text}`, `Delete{id}`, `Typing(bool)`, `Receipt{ids}`, `Read{up_to}`, `Presence`, `Profile`, `Introduce`. Account: `AuthRequest`, `AuthAccept`, `AuthDeny`, `Seeds`, `DeviceList`. Files: `FileOffer`, `FileAccept`, `FileDecline`, `FileCancel`. Groups: `GroupInvite`, `GroupUpdate`, `GroupKey`, `GroupMsg`, `GroupLeave`, `GroupSyncReq`, `GroupSync`. Own devices only: `SelfCopy`, `SelfNote`, `SyncContacts`, `SyncSettings`, `SyncRead`, `LinkGrant`, `History`, `Remote`, `RemoteAck`, `PluginState`. Store-and-forward: `Hold`, `Fetch`.

### 3.4 Delivery

Reliable bodies are sealed **per recipient device** and kept in an outbox until that device answers `ok: true` or sends a `Receipt`. If a device is unreachable for 20 s, the WireReq is stored in the network (§4). A message is *delivered* when any device of the contact took it. Presence and typing are only sent over live connections, so offline devices never make a ratchet run ahead.

Every message a device sends to a contact is also sent to the account's other devices as `SelfCopy`, so all devices hold the whole conversation.

### 3.5 Presence

`Presence { status, away_msg, os, device_class, on_battery, background, helper, bot, addrs, now_playing }`, sent when a connection opens, on change, and every 10 s over live connections; expires after 35 s. Visibility lists: *visible* contacts see `Online` while the user is Invisible; *invisible* contacts always see `Offline`.

---

## 4. Store and forward

### 4.1 Nostr mailbox (primary)

For recipient device seed `S = inbox`:

* day tag `y = hex(SHA-256("rim-mbox-tag" || S || i64le(day)))[..32]`, `day = unix_seconds div 86400`
* one-time signing key `k = secp256k1(SHA-256("rim-mbox-key" || S || nonce))` with a fresh 16-byte hex `nonce`
* content = base64(`nonce24 || XChaCha20-Poly1305(SHA-256("rim-mbox-enc" || S), JSON(WireReq))`)
* event: kind **4333**, tags `["y", tag]`, `["n", nonce]`, `["expiration", now + 14 d]`, signed by `k`.

The recipient queries kind 4333 with `#y` over the last 15 days, accepts only events whose author equals the key derived from `(S, n)`, decrypts, processes, then publishes a NIP-09 deletion signed by the same one-time key. Relays learn neither sender nor recipient identity, and events of different days are unlinkable without `S`.

### 4.2 Buddy relays

A sender may ask up to 3 online authorized contacts whose presence has `helper: true` to `Hold { device, wire, expires }`. On connecting to a helper, a device sends `Fetch`; the helper forwards held WireReqs raw (they are encrypted to the recipient).

### 4.3 DHT mailbox

The same WireReq, sealed with `SHA-256("rim-dht-enc" || S)`, is put under Kademlia key `SHA-256("rim-dht-mbox" || S || i64le(day) || u32le(slot))`, `slot ∈ 0..8`. Recipients poll today's and yesterday's slots every 5 minutes when connected to peers.

### 4.4 Address rendezvous

Each device publishes a kind **30078** (NIP-78) replaceable event authored by `secp256k1(SHA-256("rim-rdv-key" || R))` (`R = rdv` seed), `d = hex(SHA-256("rim-rdv-d" || R))[..32]`, content = sealed (as §4.1 with key `SHA-256("rim-rdv-enc" || R)`) JSON list of multiaddrs; republished every 10 minutes or when addresses change. Contacts and own devices look it up for devices they cannot reach.

---

## 5. Multiple devices

* **Link code** `rimlink2:` + base64url(JSON `{v, peer_id, curve, fallback, name, os, class, seeds, addrs, nonce}`), shown by the new device. Both sides display **six BIP-39 words** = SHA-256 over `peer_id|curve|fallback|nonce`, two bytes per word mod 2048. The user checks they match.
* A manager adds the device to the device list (version + 1), signs it, and sends `LinkGrant { nick, devices, account_key?, contacts[ContactSync], own_seeds, settings, groups }` (account key only if the new device is made a manager), then `History` chunks (≤ 200 lines), group keys, and the new device list to all contacts and own devices.
* `ContactSync` and `SettingsSync` are last-writer-wins by `updated` timestamp.
  `ContactSync` also carries the optional per-contact flags `notify_online`, `auto_accept` and `urgent_allowed`; `SettingsSync` the optional `nick` and `send_typing`. Missing fields leave the receiver's value unchanged.
* `PluginState{plugin, state, updated}` carries plugin state between own devices. The plugin id `_layout` is reserved for the gadget layout (`{plugins, collapsed, updated}`, newest wins).
* **Revocation**: a manager publishes a list without the device. **Remote lock**: `Remote{target, Lock}` from any own device. **Remote wipe**: `Remote{target, Wipe, ts, sig}` with `sig` = account key over `"rim-remote-v2\0" || "target|Wipe|ts"`; the device destroys its key material and state file. Before acting, the target sends `RemoteAck{action, ts}` to its own devices, so the sender can show the command as done.

---

## 6. Groups

* `GroupState { id, name, version, members[{account, nick, devices, seeds, addrs, admin}], by, by_device, sig }`, signed by an admin's **device key** over `"rim-group-v2\0" || JSON(id, name, version, members, by, by_device)`; accepted if the signing device is in that admin's device list and `version` is newer.
* Each member device has its own Megolm `GroupSession`; its key is sent 1:1 as `GroupKey { group, key: "s:<b64 session key>" }` (or `"x:<b64 exported key>"` when sharing history with a newly linked device).
* Messages: `GroupMsg { group, session, ciphertext }` fanned out 1:1 (Olm) to every member device, so offline members get them through their mailboxes.
* Removing a member publishes a new state and every remaining member rotates its session. Members that are not each other's contacts reach one another through the devices, seeds and addresses in the state.
* `GroupSyncReq { group, since }` / `GroupSync { lines }` on reconnect to any member (catch-up).
* Traffic for a group or session not yet known is buffered for 10 minutes and replayed.

---

## 7. Introductions

`Introduction { card, seeds, introducer, introducer_device, introducer_nick, to, verified, ts, sig, addrs }` — "introducer presents `card` to `to`". `sig` = introducer's device key over `"rim-intro-v2\0" || JSON(card, to, introducer, device, verified, ts)`; valid if that device is in the introducer's device list. `addrs` are unsigned hints.

---

## 8. Files

`FileOffer { id, name, size, chunk: 65536, hash: hex(BLAKE3(file)), key: b64(32 random bytes), ts }` travels Olm-encrypted. The receiver pulls chunks over `/rim/file/1`: `ChunkReq { file_id, index }` → `ChunkResp { file_id, index, data: b64(XChaCha20-Poly1305(key, nonce = BLAKE3(id|index)[..24], chunk)), error }`. The sender refuses relayed connections and anyone but the recipient's devices. The receiver writes `name.part`, resumes by missing indices, verifies BLAKE3 before renaming.

---

## 9. Local storage and backups

* **State file** `state.rim` = `"RIM1" || salt16 || nonce24 || XChaCha20-Poly1305(Argon2id(passphrase, salt), JSON(state))`, written atomically.
* **Backup** `.rimbackup` = `"RIMB2" || u32le(len) || JSON header { v, salt, wrapped_pass, wrapped_recovery } || nonce24 || XChaCha20-Poly1305(data key, JSON(data))`; the random data key is wrapped with Argon2id(passphrase, salt) and with SHA-256("rim-recovery-wrap-v2" || recovery entropy). The recovery key is 32 random bytes shown as 24 BIP-39 words.
* Ratchet state is never restored from a backup; a restore becomes a new device of the account (requires a backup from a manager device).

---

## 10. Plugins (local only)

JSON lines over a local socket named in `RIM_PLUGIN_SOCKET`; see `crates/rim-plugin-sdk`. Capabilities: `presence.set`, `presence.now_playing`, `settings.sync`, `chat.context_menu`, `network:<name>`. Not part of the network protocol.

---

## 11. Known limitations (v0.1 alpha)

* No independent audit yet.
* Nostr relays and DHT peers see IP addresses and timing; use Tor (not in v0.1) against that.
* Device lists embedded in group states can lag for non-admin members; contacts always get fresh lists.
* Olm fallback keys are reused for first contact with a device (as in Matrix), weakening forward secrecy of the first message until the recipient replies.

## 12. Live delivery, signals and hole punching (v0.1.3)

* **Live subscription.** Each device keeps a Nostr subscription (fixed ids `rim-mail`, `rim-signal`) open for its
  own mailbox tags of yesterday, today and tomorrow. New mail arrives within about a second; the full mailbox fetch
  runs every 120 s as a backstop. The subscription is renewed when the day changes.
* **Signals** are kind **20333** (ephemeral: relays forward them to live subscribers and store nothing).
  Tag `y` = first 16 bytes of `H("rim-sig-tag" || inbox seed || day)`, content sealed with
  `H("rim-sig-enc" || inbox seed)`, signed by a throwaway Nostr key. The sealed content is a `Signal
  {from, to, ts, kind, sig}` where `sig` is the sender device's Ed25519 signature over
  `"rim-signal-v1\0" || json([from, to, ts, kind])`. Receivers accept signals only for themselves, within ±300 s,
  from their own devices or authorized contacts.
  * `Presence(Presence)`: sent every 60 s to each authorized device without a direct connection, with the same
    visibility rules as direct presence. Valid for 150 s. A direct connection always takes precedence.
  * `Punch{addrs, reply}`: public QUIC addresses of the sender as seen from outside. The receiver answers with its
    own (`reply = true`). Both then dial each other three times, 1.5 s apart: the device with the smaller peer id
    dials normally, the other dials "as listener" (QUIC hole punching: it sends packets to open its router and
    accepts the incoming connection). At most one punch request per device every 90 s.
* **Public helpers.** Unless disabled (`public_helpers`), devices connect to the public IPFS/libp2p bootstrap nodes.
  Their identify reply tells a device its outside QUIC address; if they offer circuit relay v2, a device reserves up
  to two circuits, so libp2p's own DCUtR hole punching also works. Helpers see IP addresses, never content.

## 13. Voice calls (1:1)

* Signalling is a `Body::Call(CallSignal)` inside the normal encrypted channel, sent only to connected devices:
  `Invite{call, key}` (random 32-byte call key, base64) rings every connected device of the contact;
  `Accept{call}` from the answering device; `Decline`, `Busy`, `Hangup`; and `Answered{call}` from the answering
  device to its own sibling devices so they stop ringing. Unanswered calls end after 45 s.
* Audio: Opus, 48 kHz mono, 20 ms frames, ~28 kbit/s with in-band FEC. Each frame travels as a
  `VoiceReq{call, data}` on `/rim/voice/1` (request-response, the response is empty), where
  `data = base64(nonce24 || XChaCha20-Poly1305(call key, seq_u32_le || opus))`. Frames from anyone but the answering
  device, or for another call, are dropped. The receiver orders by `seq` and conceals up to three lost frames.
* Calls need a connection to the device; the UI shows whether it is direct or relayed. A call ends 15 s after the
  peer's connection is lost.

