# Tasks

- [x] Screen sharing: a yellow border around the screen that is being shared, so the sharer always sees what the other side sees.
- [x] Voice calls: the audio level is too low. (Automatic microphone gain plus a call volume of 50–300 %.)
- [x] Settings for audio devices (microphone and speakers). (settings → alerts → calls)
- [x] Chat: the message input wraps and grows with the typed text (up to 8 lines); Enter sends.
- [x] Chat: paste an image (Ctrl+V) into a message. Shown inline when the chat is wide enough; in a small chat
      window it shows as an icon that opens the image in its own window when clicked.
      Decided: JPEG, at most 1600 px and ~600 KB, sent like a message (directly, or through the encrypted mailbox
      when the other side is not reachable); the PRD's "no inline images" rule (MS-9) is lifted for pasted pictures.
- [x] Chat text flickered now and then: the whole message list was rebuilt on every update (e.g. every presence
      message). Now only changed rows are replaced and new ones appended.

## Making RIM popular

### 1. Remove the hurdles first
- [ ] Decide on anonymity before any big launch (a launch draws attention to the GitHub account; Apple notarization would show a real name).
- [ ] macOS: Apple Developer Program + notarization in CI (no Gatekeeper warning).
- [ ] Windows: code-signing certificate for the installer and binaries (no SmartScreen warning).
- [x] One-click invites (done on GitHub Pages: `cyberhirsch.github.io/RogueIM/i/#…`; own domain later): link like `rogueim.net/i#…` with a small landing page ("install RIM", then add the contact automatically via a `rim://` link). The invite stays after the `#`, so the server never sees it.
- [~] Register the `rim://` URL handler: Windows and Linux done; macOS declares the scheme, but the app does not yet receive the link (needs an Apple Event handler) — the invite page's "copy code" covers it.
- [ ] Beta before launch: connections through two home routers must work reliably (hole punching, calls, screen sharing) — confirmed by real tests.

### 2. Reasons to use RIM alone (beat the network effect)
- [ ] Homelab / server notifications with `rim-cli` as a bot: a short guide and ready-made examples (backup finished, disk full, service down), aimed at r/selfhosted.
- [ ] "Notes to yourself across your own devices" as a highlighted feature (synced, encrypted, no cloud).

### 3. Tell the story where the audience is
- [ ] Positioning sentence: "The ICQ feeling, without servers, without a phone number, end-to-end encrypted."
- [ ] Landing page (GitHub Pages): 20-second video (docking bar, sounds, screen sharing), screenshots of every theme, an honest "what works / what does not yet" list.
- [ ] Draft "Show HN" post.
- [ ] Posts for Lobsters, Reddit (r/privacy, r/selfhosted, r/rust, r/linux, r/nostalgia, r/retrobattlestations), the Nostr community and the Fediverse.
- [ ] German press pitch: heise, Golem, t3n, Linux magazines, c't.
- [ ] Reach out to YouTube channels about retro tech and privacy.
- [ ] Trust: external security audit; reproducible builds.
- [ ] Contributors: `CONTRIBUTING.md`, "good first issue" labels, plugins as an easy entry point.

### Later
- [ ] Mobile companion app (another device on the same account) — without mobile RIM stays a niche.
