---
title: InternetBlazer privacy policy
---

# InternetBlazer privacy policy

_Last updated: 25 October 2026_

InternetBlazer is a download manager made of a desktop app and a browser extension, published by
Leafyhead Technologies. This policy covers both.

## The short version

InternetBlazer does not collect, sell or share your data. Nothing you download, and nothing about
the pages you visit, is sent to us or to anyone else. There are no accounts, ads, analytics or
tracking.

## What the browser extension handles, and where it goes

The extension passes information **only to the InternetBlazer app on your own computer**, through
your browser's native messaging. It never sends anything over the internet itself.

When you download a file, use "Download with InternetBlazer", or use the video button, the
extension gives the app:

- the address of the file or video, and of the page it is on;
- the file name and size the browser reported;
- the cookies, referrer and origin that the browser would send for that address, so the app can
  download it as you, from sites you are signed in to;
- the addresses of video streams the page loaded, so the app can offer them.

The extension does not read page content beyond finding `<video>` elements to place its button on,
and it ignores private / incognito windows entirely.

Your extension settings (on/off, the file types to take over, the video button) are kept in your
browser's extension storage on your computer.

## What the desktop app keeps

The app keeps your download list, settings and log files in its folder on your computer. It
connects only to:

- the sites you download from;
- GitHub, to check for app updates (you can turn this off in Settings) and to fetch yt-dlp,
  and deno when video downloads need them.

These requests carry no information about you beyond what any web request does.

## Children

InternetBlazer collects no personal data from anyone, including children.

## Changes

Changes to this policy are published at this address and in the project's source code at
<https://github.com/Leafyheader/InternetBlazer>.

## Contact

Questions: open an issue at <https://github.com/Leafyheader/InternetBlazer/issues>.
