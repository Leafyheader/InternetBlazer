// Shared by the background script and the popup.

const HOST = "org.internetblazer.host";

const DEFAULT_SETTINGS = {
  /** Whether matching browser downloads go to the app. */
  enabled: true,
  /** File name extensions to take over, separated by spaces. */
  fileTypes:
    "zip rar 7z gz bz2 xz zst tar iso img " +
    "exe msi msix dmg pkg deb rpm apk appimage " +
    "mp4 mkv avi mov wmv webm m4v mp3 m4a flac wav ogg opus aac pdf epub",
};

function loadSettings() {
  return chrome.storage.local.get(DEFAULT_SETTINGS);
}

/** Lowercase extensions without dots, from text like "zip, .rar 7z". */
function parseFileTypes(text) {
  return new Set(
    text
      .toLowerCase()
      .split(/[\s,;.]+/)
      .filter(Boolean),
  );
}
