// Shared by the background script and the popup.

const HOST = "org.internetblazer.host";

const DEFAULT_SETTINGS = {
  /** Whether matching browser downloads go to the app. */
  enabled: true,
  /** Whether videos get a "Download video" button. */
  videoButton: true,
  /** File name extensions to take over, separated by spaces. */
  fileTypes:
    "zip rar 7z gz tgz bz2 xz lz lzma zst tar cab iso img bin cue " +
    "vhd vhdx vmdk qcow2 ova wim esd " +
    "exe msi msix msu dmg pkg deb rpm apk appimage jar " +
    "mp4 mkv avi mov wmv webm m4v flv ts mp3 m4a flac wav ogg opus aac pdf epub",
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
