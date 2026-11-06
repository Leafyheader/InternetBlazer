# Changelog

Each release's section is what the update dialog in the app and the GitHub release show. Write each
item on one line: the app shows them as plain text, so a wrapped line would break there too.

## 0.3.2

- Linux: .deb and .rpm packages, which update themselves like the Windows app.
- Settings lists your browsers: untick one to leave its downloads to the browser.
- Release notes now show in the update dialog.

## 0.3.0

- Video downloads work right after installing: yt-dlp, deno and ffmpeg come with the installer. Installs without them fetch them in the background.
- Updates are small (about 5 MB) and keep the video tools.
- A dialog offers each new version, checking at start, when the internet comes back and every 6 hours.
- Faster downloads: every connection is its own, writing to disk no longer holds them up, and the speed shown is steadier.
- Downloads start sooner: the file starts downloading while the download dialog is open, and connections skip redirects such as GitHub's.
- Browser extension: fixes the "Download must be in progress" error.
