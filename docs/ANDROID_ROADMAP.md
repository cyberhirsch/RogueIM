# RogueIM on Android — roadmap

From the helper app that exists today to a full RogueIM device on Android. Each stage ships on its own and is
useful without the next one. PRD references: §4 (mobile companion), PR-6/PR-8/PR-9 (device class), §12 (milestones).

## Where we are

**Stage 1 — RogueIM Phone (done, ships with every release as `RogueIM-Phone.apk`).**
A helper for the desktop: it mirrors the phone's messenger notifications (WhatsApp, Signal, Telegram, SMS …) to the
phone plugin and answers through the notifications' reply buttons. It has no RIM account, no contacts, no network of
its own; without a running RogueIM on a computer in the same network it does nothing.

Open item before anyone else installs it: a **release signing key** (GitHub secrets `RIM_APK_KEY_B64`,
`RIM_APK_PASS`). Until then the APK is signed with a debug key, and the first properly signed version can only be
installed after uninstalling the old one.

## Decisions

- **One app.** The full client grows out of the helper's Gradle project (`android/`); the notification mirror stays as
  a mode inside it. No second app to install.
- **The core is the desktop's core.** `rim-core` (identity, Olm/Megolm, libp2p, Nostr mailbox, encrypted store) is
  compiled for Android as a library. No second implementation of the protocol.
- **UI in Slint**, so the phone looks and behaves like RIM on the desktop (same components, themes, Hack font), with a
  phone layout. Kotlin only where Android demands it: the foreground service, notifications, the camera for QR codes,
  the notification listener. *To be confirmed by the spike in stage 2*; the fallback is a Kotlin UI on the same core.
- **No push server.** Messages wait in the Nostr mailbox; a foreground service keeps one Nostr subscription open so
  new messages ring within seconds. Google's push (FCM) is not used: it needs a server and Google Play Services.
- **The phone is another device of an existing account**, linked from a desktop (§7.1). Creating an account on the
  phone comes last, if at all: RIM is desktop first.

## Stages

### Stage 2 — Core on Android (spike, no UI)

- `rim-core` builds for `aarch64-linux-android` and `x86_64-linux-android` (cargo-ndk), loaded by the existing app.
- Replace what Android lacks: OS keychain → Android Keystore; profile folder → app storage; mDNS needs a multicast
  lock; device class `mobile` and OS `Android` in presence (PR-6, PR-8).
- A Slint window inside the app (GameActivity) showing a test screen, to settle the UI decision.
- **Done when:** the emulator, driven by a test like `emulator_round_trip`, links to a desktop account and exchanges a
  message both ways; APK size and idle RAM measured.

### Stage 3 — Companion MVP (chat + presence)

- Link this phone: scan the desktop's link code with the camera (QR returns, PRD §0), or paste it; compare the words.
- Unlock with the passphrase, optionally kept in the Android Keystore behind the phone's screen lock.
- Buddy list with OS/device icons and status, own status, 1:1 chats, note to self, unread counts.
- Foreground service ("RogueIM is connected") holding the Nostr subscription; a notification per new message, reply
  from the notification.
- The notification mirror of stage 1 moves into this app as a switch; the separate helper is retired.
- **Done when:** the Boss uses it daily as a second device for a week without missing a message.

### Stage 4 — Living in the background

- Fallback without the permanent notification: a periodic check (WorkManager, at best every 15 minutes), clearly
  labelled as delayed.
- Battery: one idle connection, reconnect with back-off, nothing while the phone has no network; a guide for
  manufacturers that kill background apps (exempt from battery optimisation).
- PR-9 behaviour: contacts see "may reply late" while the app is in the background; no typing indicators to it;
  files > 25 MB to a phone ask first.
- **Done when:** a night in Doze costs under 2 % battery and messages still arrive on waking.

### Stage 5 — The rest of the chat

- Groups, pictures (camera, gallery, share-to-RogueIM from other apps), files, expiring and urgent messages, search,
  history copied at linking.
- Invites: show your own as a QR code to scan from another phone; add contacts by scanning theirs.
- **Done when:** everything a contact can send from the desktop shows properly on the phone.

### Stage 6 — A device like any other

- Device management (manager role: link and remove devices from the phone), remote logout, backups.
- 1:1 voice calls, if the stage 2 measurements leave room (audio in the background needs its own service type).
- Network plugins (mail, Telegram, Signal …) stay on the desktop; on the phone the notification mirror covers them.
- **Done when:** a phone alone can do everything an ordinary user needs except creating the account.

### Stage 7 — Release

- Signed APK from GitHub releases with the same signed checksums as the desktop, and an in-app update notice.
- F-Droid with a reproducible build. Google Play only if its rules on foreground services allow RIM's design.
- **Done when:** someone who is not the Boss installs and links it from the README alone.

## Not planned

- **iOS.** iOS does not let an app keep a connection open in the background; messages would need a push server.
  Revisit only if RIM ever accepts an optional, self-hostable push relay.
- **Phone-first accounts** and phone-number identity (§3).

## Risks

- Native dependencies of `rim-core` (crypto, QUIC, SQLite) on the NDK; caught by stage 2.
- Manufacturers' battery killers; only mitigated, never solved, by stage 4.
- Slint on Android is younger than on the desktop (text input, keyboard insets, accessibility); stage 2 decides.
