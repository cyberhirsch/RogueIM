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
