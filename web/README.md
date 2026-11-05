# ZapFast in a browser

An experimental build of the whole app for `wasm32-unknown-unknown`. It runs on
this branch only, and is not ready for real accounts.

## Build and run

You need the pinned toolchain's `wasm32-unknown-unknown` target, `clang`, and
`wasm-bindgen-cli` at the version in `Cargo.lock`.

```sh
web/build.sh demo
python3 -m http.server --directory dist/web 8000
```

Then open <http://localhost:8000/?demo> for the sample chats.

## How it fits together

- `src/platform.rs` names what differs between the desktop and the browser:
  tasks, timers, the clock, and files. On the desktop these are plain
  re-exports, so nothing changes there.
- `src/web/` holds the browser's versions, the WebSocket transport, the fetch
  client, and the entry point.
- `web/shims/` holds stand-ins for crates that cannot build or run in a page.
  `src/lib.rs` names each one as the crate it replaces, for wasm builds only.

## What works

- The interface with the sample chats: drawing, clicks, navigation, typing.
- Without `?demo`, the socket to WhatsApp, the handshake, and the pairing QR code.
  Linking itself has not been tried with a phone.

## What does not work yet

- **Keeping anything.** Settings, the session, and the archive live in memory and
  are gone when the page closes, so a linked device stays listed on the phone
  with nothing behind it. Do not link an account you care about yet.
- **Colour emoji.** They draw in egui's monochrome font, and some have no glyph.
- **Video and voice.** The H.264 and Opus stand-ins always refuse. They need
  WebCodecs.
- **Downloads and avatars.** Several still use the blocking client, which always
  fails in a page, rather than fetch.
- **Background work.** The app shares the page's one thread, so decoding and a
  history sync hold up drawing.
- **Attachments, notifications, the clipboard for pictures, cameras, calls, and
  installing as an app.**

## What a browser cannot do

With no window open nothing runs, so no messages or notifications arrive: push
needs a server, and ZapFast has none. There is no tray, start at login, or
system-wide proxy setting, and no OS keyring.
