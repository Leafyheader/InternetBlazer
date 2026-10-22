// Hands browser downloads and context menu links over to the InternetBlazer app, through the
// native messaging host `blazer-host`. When the app cannot take a download, the browser
// downloads it after all. Also answers the button that video.js puts on videos, and notes the
// video streams each tab fetches for it.

// A service worker in Chrome; Firefox loads common.js first from the manifest.
if (typeof importScripts === "function") importScripts("common.js");

/** URLs the browser should download itself, because the app could not take them. */
const leaveToBrowser = new Set();

const isWeb = (url) => /^https?:/i.test(url);
const baseName = (path) => path.split(/[\/]/).pop() ?? "";

function nameFromUrl(url) {
  try {
    return decodeURIComponent(new URL(url).pathname.split("/").pop() ?? "");
  } catch {
    return "";
  }
}

function fileExtension(name) {
  return /\.([a-z0-9]{1,10})$/i.exec(name)?.[1].toLowerCase() ?? "";
}

async function shouldTake(item, name) {
  const url = item.finalUrl || item.url;
  // Private windows stay private: nothing from them goes into the app's list.
  if (item.incognito || !isWeb(url)) return false;
  if (leaveToBrowser.delete(item.url)) return false;
  const settings = await loadSettings();
  return (
    settings.enabled &&
    parseFileTypes(settings.fileTypes).has(fileExtension(name || nameFromUrl(url)))
  );
}

/** The `Cookie` header the browser would send, so the app is signed in like the browser. */
async function cookieHeader(url, storeId) {
  try {
    const cookies = await chrome.cookies.getAll(storeId ? { url, storeId } : { url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ") || null;
  } catch {
    // No access to the site, or Firefox's first-party isolation is on.
    return null;
  }
}

async function reportError(message) {
  await chrome.storage.local.set({ lastError: { message, at: Date.now() } });
  await chrome.action.setBadgeBackgroundColor({ color: "#d93838" });
  await chrome.action.setBadgeText({ text: "!" });
}

/** Sends a request about `url` to the app, signed in like the browser. Never throws. */
async function askApp(type, { url, fileName, referrer, storeId, media, sent, stream }) {
  const message = {
    type,
    url,
    stream: Boolean(stream),
    fileName: fileName || null,
    referrer: referrer || null,
    cookies: await cookieHeader(url, storeId),
    userAgent: navigator.userAgent,
    media: media ?? null,
    // What the browser itself sent for a stream, which its server may check.
    ...sent,
  };
  try {
    return (await chrome.runtime.sendNativeMessage(HOST, message)) ?? { ok: false };
  } catch (e) {
    return { ok: false, error: e?.message ?? String(e) };
  }
}

/** Returns whether the app took the download. */
async function sendToApp(download) {
  const reply = await askApp("download", download);
  if (!reply.ok) await reportError(reply.error ?? "InternetBlazer did not reply.");
  return Boolean(reply.ok);
}

/** Sends a download the browser already stopped to the app, or restarts it in the browser. */
async function handOver(item, name) {
  chrome.downloads.erase({ id: item.id }).catch(() => {});
  const sent = await sendToApp({
    url: item.finalUrl || item.url,
    fileName: name,
    referrer: item.referrer,
    storeId: item.cookieStoreId,
  });
  if (!sent) {
    leaveToBrowser.add(item.url);
    chrome.downloads.download({ url: item.url }).catch((e) => reportError(String(e)));
  }
}

const stop = (item) => chrome.downloads.cancel(item.id).catch(() => {});

if (chrome.downloads.onDeterminingFilename) {
  // Chrome: decide before the browser picks a file or shows its save dialog.
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    const name = baseName(item.filename);
    shouldTake(item, name).then(async (take) => {
      if (take) await stop(item);
      try {
        suggest();
      } catch {
        // Already cancelled.
      }
      if (take) handOver(item, name);
    });
    return true;
  });
} else {
  // Firefox: the file name is known when the download is created.
  chrome.downloads.onCreated.addListener(async (item) => {
    const name = baseName(item.filename);
    if (await shouldTake(item, name)) {
      await stop(item);
      handOver(item, name);
    }
  });
}

function createMenu() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "download",
      title: "Download with InternetBlazer",
      contexts: ["link", "image", "video", "audio"],
    });
  });
}

chrome.runtime.onInstalled.addListener(createMenu);
chrome.runtime.onStartup.addListener(createMenu);

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.linkUrl || info.srcUrl;
  if (!url || !isWeb(url)) {
    reportError("Only http and https links can be downloaded.");
    return;
  }
  sendToApp({ url, referrer: info.pageUrl, storeId: tab?.cookieStoreId });
});

// The button on videos.

chrome.runtime.onMessage.addListener((message, sender, reply) => {
  const tab = sender.tab;
  if (!tab || tab.incognito) return false;
  const type = message?.type;
  if (type === "streams") {
    streamsOf(tab.id)
      .catch(() => [])
      .then((streams) => {
        for (const s of streams) titled(s.found, tab);
        reply({ ok: true, streams, title: tab.title ?? "" });
      });
    return true;
  }
  if (!["inspect", "prefetch", "download"].includes(type) || !isWeb(message.url)) return false;
  (async () => {
    const stream = (await streamsOf(tab.id)).find((s) => s.url === message.url);
    const answer = await askApp(type, {
      url: message.url,
      referrer: sender.url ?? tab.url,
      storeId: tab.cookieStoreId,
      fileName: message.fileName,
      media: message.media,
      sent: stream?.sent,
      stream: stream && stream.kind !== "file",
    });
    if (stream) titled(answer, tab);
    if (type !== "download") return answer;
    return { ok: Boolean(answer.ok), error: answer.ok ? null : (answer.error ?? "InternetBlazer did not reply.") };
  })().then(reply, (e) => reply({ ok: false, error: String(e) }));
  return true;
});

/** A stream has no title of its own (its file is "index" or "master"), so it gets the page's,
 * as the tab shows it. */
function titled(answer, tab) {
  if (answer?.media && tab.title) answer.media.title = tab.title;
}

// Video streams fetched by each tab, for pages yt-dlp doesn't know. Kept in session storage,
// since the service worker may stop between the video loading and the user asking.

const MAX_STREAMS = 12;
const streamKey = (tabId) => `streams:${tabId}`;

async function streamsOf(tabId) {
  const key = streamKey(tabId);
  return (await chrome.storage.session.get(key))[key] ?? [];
}

/** Files smaller than this are sounds and clips that pages play, such as YouTube's beeps. */
const MIN_FILE_SIZE = 1024 * 1024;

function header(details, name) {
  return details.responseHeaders?.find((h) => h.name.toLowerCase() === name)?.value ?? "";
}

/** The size of the whole file, from a ranged answer's Content-Range or the Content-Length. */
function fileSize(details) {
  const total = /\/(\d+)\s*$/.exec(header(details, "content-range"))?.[1];
  const size = Number(total ?? header(details, "content-length"));
  return Number.isFinite(size) && size > 0 ? size : null;
}

/** "hls", "dash" or "file" for a video the tab fetched, or null for anything else. */
function streamKind(details) {
  const type = header(details, "content-type").toLowerCase();
  const path = (() => {
    try {
      return new URL(details.url).pathname.toLowerCase();
    } catch {
      return "";
    }
  })();
  if (type.includes("mpegurl") || path.endsWith(".m3u8")) return "hls";
  if (type.includes("dash+xml") || path.endsWith(".mpd")) return "dash";
  // Files a <video> plays directly. Players that feed the video in pieces fetch them from
  // scripts, and HLS pieces are MPEG-TS.
  if (
    details.type === "media" &&
    /^(video|audio)\//.test(type) &&
    !type.includes("mp2t") &&
    (fileSize(details) ?? MIN_FILE_SIZE) >= MIN_FILE_SIZE
  ) {
    return "file";
  }
  return null;
}

// Headers about this one request or connection, which the app sets itself. The rest are sent
// as the browser sent them, as IDM does, since some video servers check them all.
const UNSENT_HEADERS = new Set([
  "host", "connection", "keep-alive", "content-length", "transfer-encoding", "te", "upgrade",
  "range", "if-range", "if-match", "if-none-match", "if-modified-since", "accept-encoding",
  "proxy-authorization", "proxy-connection",
]);

// The headers of requests in flight, until their answer shows whether they fetch a stream.
// Chrome shows Referer, Origin and Cookie only with "extraHeaders"; Firefox always does.
const sentHeaders = new Map();
const SEND_OPTIONS = chrome.webRequest.OnSendHeadersOptions?.EXTRA_HEADERS
  ? ["requestHeaders", "extraHeaders"]
  : ["requestHeaders"];
const STREAM_REQUESTS = { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other"] };

chrome.webRequest.onSendHeaders.addListener(
  (details) => {
    if (details.tabId < 0) return;
    const sent = { extraHeaders: [] };
    for (const { name, value } of details.requestHeaders ?? []) {
      if (!value) continue;
      const lower = name.toLowerCase();
      const key = { referer: "referrer", origin: "origin", cookie: "cookies", "user-agent": "userAgent" }[lower];
      if (key) sent[key] = value;
      else if (!UNSENT_HEADERS.has(lower)) sent.extraHeaders.push([name, value]);
    }
    sentHeaders.set(details.requestId, sent);
  },
  STREAM_REQUESTS,
  SEND_OPTIONS,
);

for (const event of [chrome.webRequest.onCompleted, chrome.webRequest.onErrorOccurred]) {
  event.addListener((details) => sentHeaders.delete(details.requestId), STREAM_REQUESTS);
}

// Stream changes are queued, so two arriving together don't overwrite each other.
let streamWrites = Promise.resolve();

function noteStream(tabId, stream) {
  let isNew = false;
  updateStreams(tabId, (list) => {
    const old = list.find((s) => s.url === stream.url);
    isNew = !old;
    // Newest first, keeping what was already read from it.
    return [{ ...stream, found: old?.found }, ...list.filter((s) => s !== old)];
  }).then(() => {
    if (isNew && stream.kind !== "file") readStream(tabId, stream);
  });
}

/** Has the app read the playlist's qualities right away, as IDM does, so they are ready when
 * the button is clicked. Only when the app is running: playing a video never starts it. */
async function readStream(tabId, stream) {
  const answer = await askApp("prefetch", {
    url: stream.url,
    referrer: stream.sent?.referrer,
    sent: stream.sent,
    stream: true,
  });
  if (answer.appRunning === false) return;
  await updateStreams(tabId, (list) =>
    list.map((s) => (s.url === stream.url ? { ...s, found: answer } : s)),
  );
}

function updateStreams(tabId, change) {
  streamWrites = streamWrites
    .then(async () => {
      const list = change(await streamsOf(tabId));
      await chrome.storage.session.set({ [streamKey(tabId)]: list.slice(0, MAX_STREAMS) });
    })
    .catch(() => {});
  return streamWrites;
}

function forgetStreams(tabId) {
  streamWrites = streamWrites.then(() => chrome.storage.session.remove(streamKey(tabId))).catch(() => {});
}

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (details.tabId < 0 || !isWeb(details.url)) return;
    if (details.type === "main_frame") {
      forgetStreams(details.tabId);
      return;
    }
    const sent = sentHeaders.get(details.requestId);
    sentHeaders.delete(details.requestId);
    if (details.statusCode < 200 || details.statusCode >= 300) return;
    const kind = streamKind(details);
    if (kind) noteStream(details.tabId, { url: details.url, kind, sent, size: fileSize(details) });
  },
  { urls: ["<all_urls>"], types: ["main_frame", "media", "xmlhttprequest", "other"] },
  ["responseHeaders"],
);

chrome.tabs.onRemoved.addListener(forgetStreams);
