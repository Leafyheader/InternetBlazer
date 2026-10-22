// Hands browser downloads and context menu links over to the InternetBlazer app, through the
// native messaging host `blazer-host`. When the app cannot take a download, the browser
// downloads it after all.

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

/** Returns whether the app took the download. */
async function sendToApp({ url, fileName, referrer, storeId }) {
  const message = {
    type: "download",
    url,
    fileName: fileName || null,
    referrer: referrer || null,
    cookies: await cookieHeader(url, storeId),
    userAgent: navigator.userAgent,
  };
  let reply;
  try {
    reply = await chrome.runtime.sendNativeMessage(HOST, message);
  } catch (e) {
    reply = { ok: false, error: e?.message ?? String(e) };
  }
  if (!reply?.ok) await reportError(reply?.error ?? "InternetBlazer did not reply.");
  return Boolean(reply?.ok);
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
