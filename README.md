# InternetBlazer

A fast, open-source, cross-platform download manager written in Rust: an IDM alternative with a modern UI.

> **Status:** Phase 4. The download engine, a CLI, the desktop app (with queues, a scheduler and speed limits) and a browser extension work. Video downloads with yt-dlp come next.

## Features (engine)

- **Segmented downloads.** Parallel HTTP range requests, with dynamic re-splitting: when a connection finishes, it takes half of the largest remaining segment, so no connection sits idle near the end.
- **Crash-safe resume.** Progress is checkpointed every second: data is flushed to disk first, then the progress is recorded. A crash or power loss can only lose the last second of progress, never corrupt the file.
- **Change detection.** `If-Range` and ETag/Last-Modified checks stop a resumed download from mixing two versions of a file.
- **Polite to servers.** On HTTP 429/503 it sheds connections and honors `Retry-After`, instead of hammering the server until the IP gets blocked.
- **Fallbacks.** Single-stream download for servers without range support or without a known size.
- **Safe file names.** Server-supplied names are sanitized, and a unique `name (1).ext` is picked when the name is already taken.
- **Queues.** Started downloads wait in a queue that runs a set number at a time, oldest first. You can add your own queues and move downloads between them.
- **Scheduler.** A queue can start and stop at set times on chosen weekdays (local time), for example overnight. A stop time at or before the start time means the next day. Schedules run only while the desktop app is open.
- **Speed limits.** One limit for all downloads together, plus an optional limit per download. Both can be changed while downloads run.

## Layout

| Path | What |
|---|---|
| `crates/blazer-core` | Download engine library: no UI, async (tokio + reqwest), state in redb |
| `crates/blazer-cli` | `blazer` command-line tool built on the engine |
| `crates/blazer-host` | `blazer-host`, which connects the browser extension to the app |
| `app/` | Desktop app: Svelte 5 frontend in `app/src`, Tauri 2 shell in `app/src-tauri` |
| `extension/` | Browser extension for Chrome, Edge, Brave and Firefox (Manifest V3, no build step) |

Planned: yt-dlp video support.

## Build

Requires Rust 1.95 or newer, and Node.js 22 or newer for the desktop app.

```sh
cargo build --release
cargo test
```

## Desktop app

```sh
cd app
npm install
npm run tauri dev                          # run with hot reload
npm run tauri build -- --no-bundle         # release executable in target/release
npm run check                              # type-check the frontend
```

The app and the CLI share one download list, stored in your local app data folder. Only one of them can have it open at a time. Starting the app while it runs brings its window to the front.

The sidebar lists the queues: select one to start, stop or configure it. "Move to queue" in the toolbar moves the selected downloads. The speed limit for all downloads is in the status bar, and a single download's limit is in its details.

Shortcuts: Ctrl+N adds a download, Delete removes the selection, Enter opens a finished file or resumes a paused one, Ctrl+A selects all.

## Browser extension

The extension hands downloads to the app: browser downloads whose file type is on its list (set in the extension's popup), and any link through "Download with InternetBlazer" in the right-click menu. The app shows each one in the add dialog, filled in, and starts itself if it is not running. The browser's cookies, referrer and user agent go along, so downloads that need you to be signed in work. Downloads in private windows stay in the browser.

The browser reaches the app through `blazer-host`, a native messaging host that must sit next to the app executable:

```sh
cargo build --release -p blazer-host       # next to the app in target/release
```

The app registers the `blazer-host` beside it with the installed browsers each time it starts. You can also do it yourself with `blazer-host install`, and undo it with `blazer-host uninstall`. Registration is per user and needs no admin rights.

To load the extension:

- **Chrome, Edge, Brave:** open `chrome://extensions`, turn on Developer mode, click "Load unpacked" and pick the `extension` folder. Its ID must be `bdghohjegkeehdacpafalkbieochhlee`, which the `key` in its manifest fixes.
- **Firefox:** open `about:debugging#/runtime/this-firefox`, click "Load Temporary Add-on" and pick `extension/manifest.json`. In the popup, click "Allow access" so the extension can read cookies.

If a download cannot reach the app, the browser downloads it after all and the toolbar icon shows a red `!`. Open the popup to see why.

## CLI

```sh
blazer get <URL> [-d DIR] [-o NAME] [-c CONNECTIONS] [-l LIMIT]   # Ctrl+C pauses
blazer list
blazer limit [RATE|off]                     # e.g. 500K or 2M; shared with the app
blazer resume <ID>
blazer restart <ID>
blazer remove <ID> [--delete-files]
```

Set `RUST_LOG=blazer_core=debug` to see retries and connection changes.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
