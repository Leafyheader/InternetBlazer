// Shows whether the app can be reached, the last error, and the settings.

const $ = (id) => document.getElementById(id);
const ALL_SITES = { origins: ["<all_urls>"] };

async function showStatus() {
  const status = $("status");
  let text;
  try {
    const reply = await chrome.runtime.sendNativeMessage(HOST, { type: "ping" });
    status.className = "status ok";
    text = reply.appRunning
      ? "Connected to InternetBlazer"
      : "Ready. InternetBlazer opens when a download arrives.";
  } catch {
    status.className = "status bad";
    text = "InternetBlazer is not set up. Open the app once, then reopen this popup.";
  }
  $("status-text").textContent = text;
}

async function showError() {
  const { lastError } = await chrome.storage.local.get("lastError");
  $("error").hidden = !lastError;
  if (lastError) $("error-text").textContent = lastError.message;
}

async function showAccess() {
  // Firefox lets users withhold site access, which reading cookies needs.
  $("access").hidden = await chrome.permissions.contains(ALL_SITES);
}

async function showSettings() {
  const settings = await loadSettings();
  $("enabled").checked = settings.enabled;
  $("file-types").value = settings.fileTypes;
}

$("dismiss").addEventListener("click", async () => {
  await chrome.storage.local.remove("lastError");
  await chrome.action.setBadgeText({ text: "" });
  $("error").hidden = true;
});

$("grant").addEventListener("click", async () => {
  if (await chrome.permissions.request(ALL_SITES)) $("access").hidden = true;
});

$("enabled").addEventListener("change", (e) => {
  chrome.storage.local.set({ enabled: e.target.checked });
});

// Saved while typing: the popup closes, without a change event, when it loses focus.
$("file-types").addEventListener("input", (e) => {
  chrome.storage.local.set({ fileTypes: [...parseFileTypes(e.target.value)].join(" ") });
});

showStatus();
showError();
showAccess();
showSettings();
