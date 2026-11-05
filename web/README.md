# ZapFast in a browser

An experimental build of the whole app for `wasm32-unknown-unknown`. It runs on
this branch only, and is not ready for real accounts.

## Build and run

You need the pinned toolchain's `wasm32-unknown-unknown` target, `clang`, and
`wasm-bindgen-cli` at the version in `Cargo.lock`.

```sh
web/build.sh demo
python3 -m http.server --bind 127.0.0.1 --directory dist/web 8000
```

Then open <http://localhost:8000/?demo> for the sample chats, or
<http://localhost:8000/> for a real account. The page needs a secure context, which
`localhost` counts as: elsewhere it has to be served over HTTPS.

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
- Without `?demo`, a real account: linking by QR code, syncing chats, sending and
  receiving messages, and staying linked across a reload.
- Storage that survives a reload, kept to one tab at a time. The archive and the
  device store are encrypted. Each has a random key, wrapped by a master key the
  browser holds as non-extractable.

## What storage costs

- A commit reaches IndexedDB just after it returns, so a tab killed in between
  loses it. A message comes back with the next history sync, and a Signal session
  re-establishes. The durable alternative needs a dedicated worker.
- Each database is held in memory while the page is open, so a large archive takes
  that much memory and takes time to load.
- The browser may clear its storage when space runs low, unless it grants
  persistence. Clearing it unlinks the device.
- The keys protect copied files, not a compromised page: code running in the page
  can ask the browser to use them. Whoever serves the page can change that code.
- Attachments stay in memory only, and are downloaded again after a reload.

## What does not work yet

- **Colour emoji in plain labels.** Messages and chat names have them, drawn by the
  browser. Menus and tooltips keep the fonts' monochrome glyphs.
- **Video and voice.** The H.264 and Opus stand-ins always refuse. They need
  WebCodecs.
- **GIF search and Signal sticker packs.** They still use the blocking client,
  which always fails in a page, rather than fetch. Signal's servers also refuse
  requests from a page.
- **Background work.** The app shares the page's one thread, so decoding and a
  history sync hold up drawing.
- **Exporting a chat, and setting aside an unreadable archive.** Both need a second
  way into a database, which this storage does not offer.
- **Attachments, notifications, the clipboard for pictures, cameras, calls, and
  installing as an app.**

## What a browser cannot do

With no window open nothing runs, so no messages or notifications arrive: push
needs a server, and ZapFast has none. There is no tray, start at login, or
system-wide proxy setting, and no OS keyring.
