# RogueIM Phone

The Android app for RogueIM's phone plugin: it forwards the messages your phone shows as notifications (WhatsApp,
Signal, SMS, Telegram, … you choose) to RogueIM on your computer, and sends your replies through the notification's
reply button. No server, no cloud: phone and computer talk directly, in your own network, encrypted
(see `plugins/phone/src/link.rs`).

Build: `./gradlew assembleRelease testDebugUnitTest` (JDK 17, Android SDK 36). The APK is in
`app/build/outputs/apk/release/`.

Test against the emulator: start an emulator with Google Messages, install the APK, allow notification access
(`adb shell cmd notification allow_listener io.github.cyberhirsch.rogueim.phone/io.github.cyberhirsch.rogueim.phone.Forwarder`),
run `RIM_PHONE_CODE_FILE=code.txt cargo test -p rim-plugin-phone --test protocol emulator_round_trip -- --ignored`,
paste the code from `code.txt` into the app, then `adb emu sms send 5551234 "hi"`.
