# InternetBlazer

A fast, open-source, cross-platform download manager written in Rust: an IDM alternative with a modern UI.

> **Status:** Phase 5. The download engine, a CLI, the desktop app (with queues, a scheduler and speed limits), a browser extension and video downloads with yt-dlp work. Packaging comes next.

## Features (engine)

- **Segmented downloads.** Parallel HTTP range requests, with dynamic re-splitting: when a connection finishes, it takes half of the largest remaining segment, so no connection sits idle near the end.
- **Crash-safe resume.** Progress is checkpointed every second: data is flushed to disk first, then the progress is recorded, so the record never claims bytes that are not on disk. A crash or power loss loses at most the last second of progress. (This relies on the disk really writing data when asked to, as disks normally do.) When the app opens again, it checks the partial file: if its size no longer matches, the download starts over instead of resuming into the wrong data. A download that finished just before a crash is recognized as complete.
- **Safe finish.** A download is written to a `.part` file and renamed when complete, never over an existing file. Its size is checked before it counts as complete.
- **Checked responses.** Every partial response must say which bytes it holds and match the expected file size, or the download stops instead of writing misplaced data. A server whose ranges hold more data than they claim, as some CDNs do, is downloaded in one piece instead.
- **Automatic retries.** After a temporary failure, such as a lost connection or a server error, a download waits and tries again by itself, for about an hour with growing pauses, and shows when the next try is. A short drop is bridged by the connections themselves; the list then shows "Reconnecting…" and why, rather than a frozen progress bar. Errors that retrying cannot fix, such as a missing file, fail right away with a message that says what to do.
- **New address, same progress.** When a link expires, give the download a fresh one. It keeps its progress if the new link serves the same file.
- **Disk space check.** A download that cannot fit on the disk fails before it starts, saying how much space it needs.
- **Change detection.** `If-Range` and ETag/Last-Modified checks stop a resumed download from mixing two versions of a file.
- **Polite to servers.** On HTTP 429/503 it sheds connections and honors `Retry-After`, instead of hammering the server until the IP gets blocked. Retries are spread out slightly at random, so connections that failed together don't all come back at once.
- **Fallbacks.** Single-stream download for servers without range support or without a known size.
- **Safe file names.** Server-supplied names are sanitized, and a unique `name (1).ext` is picked when the name is already taken.
- **File type detection.** A file sent without an extension gets one from its first bytes ("magic bytes"), such as `.mp4`, `.flv`, `.zip` or `.pdf`, so it opens with the right program.
- **Queues.** Started downloads wait in a queue that runs a set number at a time, oldest first. You can add your own queues and move downloads between them.
- **Scheduler.** A queue can start and stop at set times on chosen weekdays (local time), for example overnight. A stop time at or before the start time means the next day. Schedules run only while the desktop app is open.
- **Folders by file type.** Each download goes to the folder for its type, such as `Downloads\Video` or `Downloads\Compressed`, picked from the file's name. The download dialog shows the folder, which you can still change, and the sidebar lists downloads by type.
- **Speed limits.** One limit for all downloads together, plus an optional limit per download. Both can be changed while downloads run.
- **Videos from web pages.** For a link to a page with a video, such as on YouTube, [yt-dlp](https://github.com/yt-dlp/yt-dlp) finds the video and the qualities it comes in, and downloads it. These downloads queue, pause, resume and retry like any other. yt-dlp is installed on first use from its official release, after checking the release's published SHA-256, and can be updated from Settings; a copy on `PATH` also works. On Windows it is yt-dlp's folder build, which starts in under a second instead of three. For YouTube, yt-dlp also needs [deno](https://deno.com) to get every quality, which Settings can install the same way. An HLS (`.m3u8`) or DASH (`.mpd`) playlist link is read directly, which lists its qualities in a moment. Sites like YouTube keep picture and sound apart, and joining them needs [ffmpeg](https://ffmpeg.org) on `PATH` (on Windows: `winget install ffmpeg`). Without it, only the sound or qualities that come in one file are offered.

## Layout

| Path | What |
|---|---|
| `crates/blazer-core` | Download engine library: no UI, async (tokio + reqwest), state in redb |
| `crates/blazer-cli` | `blazer` command-line tool built on the engine |
| `crates/blazer-host` | `blazer-host`, which connects the browser extension to the app |
| `app/` | Desktop app: Svelte 5 frontend in `app/src`, Tauri 2 shell in `app/src-tauri` |
| `extension/` | Browser extension for Chrome, Edge, Brave and Firefox (Manifest V3, no build step) |

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
npm run package                            # Windows installer, with blazer-host, in target/release/bundle/nsis
```

`npm run package` also signs the update files, so it needs the update key: set `TAURI_SIGNING_PRIVATE_KEY` to the key or its path, and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (empty for the project's key). The installer installs for the current user, without asking for admin rights. Uninstalling also unregisters blazer-host from the browsers.

The app and the CLI share one download list, stored in your local app data folder. Only one of them can have it open at a time. Starting the app while it runs brings its window to the front. Downloads that were running or queued when the app last quit or crashed start again when it opens. The CLI never starts them on its own.

Right-click a download for Open, Open with (Windows), Show in folder, Resume or Pause, Download again, Move / rename (which also moves a paused download's partial data), Add to queue, Remove and Properties. Shift+F10 or the menu key opens it for the selection. The browser's own menu, with Reload and Inspect, is gone; text fields keep cut, copy and paste. Release builds also ignore browser shortcuts such as F5 and Ctrl+R.

The sidebar lists the queues: select one to start, stop or configure it. "Move to queue" in the toolbar moves the selected downloads. The speed limit for all downloads is in the status bar, and a single download's limit is in its details.

Closing the window keeps the app running in the tray, so downloads carry on. Click the tray icon to bring the window back, or right-click it to pause everything or quit. Its tooltip shows how many downloads are running and how fast.

Settings, at the bottom of the sidebar:

- **Downloads:** the default folder; whether new downloads are sorted into a folder per file type (on by default: `Video`, `Music`, `Compressed`, `Documents`, `Programs` and `Other` inside the default folder, each of which can be moved anywhere), or else whether to start in the folder used last; connections per download; how many automatic retries a download gets. The connections and retries settings are shared with the CLI.
- **Video downloads:** the yt-dlp version, with a button to update it; deno, which yt-dlp needs for every YouTube quality, with a button to install it; and whether ffmpeg was found.
- **Browser:** whether files from the extension wait in the download dialog or start right away in the folder for their type.
- **Desktop:** starting at sign-in (hidden in the tray), keeping the app in the tray when the window is closed, and notifications when downloads finish or fail while the window is in the background.
- **Updates:** whether to check for a newer version each time the app starts. A newer version found at start shows a notice with an install button, and a dot next to About.

About, below Settings (also in the tray menu), shows the version, checks for updates and installs them, and links to the website, the release notes and the issue tracker. "Copy version info" copies what a bug report needs. Updates come from the GitHub releases and are only installed when signed with the project's update key. Running downloads pause for the update and carry on in the new version.

The app keeps a log for each day, for a week, in the `logs` folder next to the download list. "Open log folder" in Settings shows it. Set `RUST_LOG=debug` for more detail.

Shortcuts: Ctrl+N adds a download, Delete removes the selection, Enter opens a finished file or resumes a paused one, Ctrl+A selects all.

## Browser extension

The extension hands downloads to the app: browser downloads whose file type is on its list (set in the extension's popup), and any link through "Download with InternetBlazer" in the right-click menu. The app shows each one in the add dialog, filled in, and starts itself if it is not running. The browser's cookies, referrer and user agent go along, so downloads that need you to be signed in work. Downloads in private windows stay in the browser.

Videos get a "Download video" button in their top right corner while the pointer is over them or when they start playing. It lists the video's qualities; pick one to send the video to the app, named after the page. When the player loads an HLS or DASH playlist, the app reads its qualities right away (while it runs), so they are ready before the button is clicked; the download keeps the headers the browser sent for it, which some video servers check. On pages without one, such as YouTube, the button asks yt-dlp, which a video that keeps playing gets in the background. Videos protected with DRM can't be downloaded. The button can be turned off in the popup.

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
blazer get <PAGE-URL> -m best|audio|720     # the video on a page, with yt-dlp
blazer tools [--update]                     # show yt-dlp and ffmpeg; install or update yt-dlp
blazer list
blazer set-url <ID> <URL>                   # a fresh link for the same file; keeps progress
blazer limit [RATE|off]                     # e.g. 500K or 2M; shared with the app
blazer resume <ID>
blazer restart <ID>
blazer remove <ID> [--delete-files]
```

Set `RUST_LOG=blazer_core=debug` to see retries and connection changes.

## Releasing

1. Raise `version` in the root `Cargo.toml` (the app takes its version from there) and in `app/package.json`.
2. Commit, then push a tag with that version: `git tag v0.2.0 && git push origin v0.2.0`.
3. The Release workflow builds the installer and drafts a GitHub release with it and `latest.json`. Publish the draft, and installed copies offer the update.

The workflow needs the repository secret `TAURI_SIGNING_PRIVATE_KEY`, holding the contents of the update key. The project's key has no password, so `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` stays unset. Losing the key means installed copies can no longer be updated, so keep a backup. The CI workflow runs formatting, clippy and the tests on Windows, Linux and macOS for every push.

## License

© 2026 Leafyhead Technologies. Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
