# Store listings

Text to paste into the store dashboards. Build the zips with `npm run pack-extension` in `app/`;
they land in `target/extension/`. After a store assigns the extension its ID, add it to
`CHROME_EXTENSION_IDS` in `crates/blazer-host/src/install.rs` (Edge and Chrome IDs differ).
Firefox keeps the ID `internetblazer@internetblazer.org` from the manifest.

## Common

- **Name:** InternetBlazer
- **Category:** Productivity (Edge) / Download management (Firefox)
- **Privacy policy:** https://leafyheader.github.io/InternetBlazer/privacy
- **Homepage / support:** https://github.com/Leafyheader/InternetBlazer
- **License (Firefox):** MIT or Apache-2.0

**Short description** (Edge: up to 250 characters; Firefox summary: up to 250):

> Sends your downloads to InternetBlazer, a free and open-source download manager that downloads
> faster with several connections, resumes broken downloads, and saves videos from web pages.

**Description:**

> InternetBlazer connects your browser to the InternetBlazer desktop app, a free and open-source
> download manager for Windows.
>
> • Downloads of the file types you choose go to the app instead of the browser
> • Right-click any link and pick "Download with InternetBlazer"
> • A "Download video" button on videos lists the available qualities
> • Downloads from sites you are signed in to work, because the app gets the browser's cookies
> • Private windows are always left alone
>
> The app downloads with several connections at once, resumes after a dropped connection or a
> restart, checks SHA-256 sums, and runs queues on a schedule.
>
> This extension needs the InternetBlazer app: https://github.com/Leafyheader/InternetBlazer/releases
>
> Nothing is collected or sent anywhere except to the app on your own computer.

## Permission justifications

Edge and Chrome ask for these; Firefox reviewers read them from the "Notes to reviewer" field.

- **Single purpose:** Hand downloads and videos from the browser over to the InternetBlazer
  download manager app installed on the same computer.
- **nativeMessaging:** The only way the extension talks to the InternetBlazer app; every download
  is passed to it this way.
- **downloads:** To notice downloads the user starts, cancel the browser's copy and give the
  address to the app, and to let the browser download it after all when the app is not running.
- **cookies:** So the app can download files from sites the user is signed in to, it is given
  the cookies the browser would send for that one address. They go only to the local app.
- **webRequest:** To see the video streams (HLS/DASH playlists, media files) a page loads, so the
  video button can offer them, and the Referer/Origin the browser sent, which some sites require.
  Requests are observed only, never blocked or changed.
- **contextMenus:** For the "Download with InternetBlazer" item on links.
- **storage:** To keep the extension's own settings (on/off, file types, video button).
- **Host permission `<all_urls>` and the content script:** Downloads and videos can be on any
  site. The content script only places the download button on `<video>` elements.
- **Remote code:** No. All code is in the package; nothing is fetched or evaluated at run time.

## Data use (Edge "Privacy" tab / Firefox data collection)

The extension does not collect or transmit user data. Addresses, cookies and file names are passed
only to the InternetBlazer app on the user's own computer, through native messaging, to perform
the download the user asked for. Nothing is sold, shared, or used for anything else.

## Notes for reviewers / test instructions

> This extension is the browser half of InternetBlazer, an open-source download manager
> (https://github.com/Leafyheader/InternetBlazer). On its own it can't download anything; it needs
> the desktop app:
>
> 1. Install the app from https://github.com/Leafyheader/InternetBlazer/releases/latest
>    (Windows, `InternetBlazer_x.y.z_x64-setup.exe`). Installing registers the native messaging host
>    `org.internetblazer.host`.
> 2. Install this extension, then open the toolbar popup: it shows "Connected to InternetBlazer".
> 3. Download a file, for example https://proof.ovh.net/files/10Mb.dat (add `dat` to the file types
>    in the popup) or any .zip; it appears in the app instead of the browser's download bar.
> 4. Right-click a link and choose "Download with InternetBlazer".
> 5. Open a page with a video (e.g. https://test-streams.mux.dev/ or any site with an MP4); hover
>    the video and use the "Download video" button.
>
> If the app is closed, downloads fall back to the browser. The source code is the same as the
> repository's `extension/` folder; the package is built by `app/scripts/pack-extension.mjs`, which
> only drops the `key` field and the other browser's background entry from the manifest. No code is
> minified or generated.

## Screenshots

Edge wants 1280×800 or 640×400 (up to 10); Firefox takes any size. Suggested:

1. A page's download handed to the app, with the app window in front.
2. The "Download video" menu open on a video, listing qualities.
3. The toolbar popup with its settings.
4. The right-click "Download with InternetBlazer" item.

Edge also takes a 300×300 logo (use `design/app-icon.png`, resized) and optional promo tiles
(440×280 small, 1400×560 large).
