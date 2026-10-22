// Typed wrappers around the Tauri commands and events in `src-tauri/src/main.rs`. The types
// mirror the serde output of `blazer-core`.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Status = "queued" | "downloading" | "retrying" | "paused" | "completed" | "failed";

/** An automatic retry after a temporary failure. */
export interface Retry {
  /** Counts from 1. */
  attempt: number;
  /** Unix seconds. */
  at: number;
}

export interface Segment {
  start: number;
  end: number;
  downloaded: number;
}

export interface DownloadRecord {
  id: number;
  url: string;
  headers: [string, string][];
  dir: string;
  fileName: string;
  total: number | null;
  resumable: boolean;
  etag: string | null;
  lastModified: string | null;
  connections: number | null;
  queue: number;
  /** Bytes per second. */
  speedLimit: number | null;
  segments: Segment[];
  status: Status;
  error: string | null;
  retry: Retry | null;
  /** Running when the app last quit or crashed; the app resumes these as it starts. */
  interrupted: boolean;
  /** Unix seconds. */
  addedAt: number;
  completedAt: number | null;
}

export interface Progress {
  id: number;
  downloaded: number;
  total: number | null;
  /** Bytes per second. */
  speed: number;
  connections: number;
  /** Nothing has arrived for a few seconds, and not because of a speed limit. */
  stalled: boolean;
  /** While stalled: why connections last failed, such as the network being down. */
  problem: string | null;
  segments: Segment[];
}

/** What a link points to, found out before downloading it. */
export interface LinkInfo {
  fileName: string | null;
  total: number | null;
  resumable: boolean;
}

export interface Schedule {
  enabled: boolean;
  /** Minutes after midnight, local time. */
  start: number | null;
  /** Minutes after midnight; at or before `start` means the next day. */
  stop: number | null;
  /** Bit 0 is Monday, bit 6 is Sunday. */
  days: number;
}

export interface Queue {
  id: number;
  name: string;
  maxActive: number;
  schedule: Schedule;
}

export const MAIN_QUEUE = 0;

/** The engine's settings, shared with the CLI. */
export interface Settings {
  /** Bytes per second across all downloads. */
  speedLimit: number | null;
  /** For downloads that don't set their own. `get_settings` always fills it in. */
  connections: number | null;
  /** Times a download that failed for a temporary reason is tried again. Always filled in by `get_settings`. */
  autoRetries: number | null;
}

/** Settings of the desktop app itself. */
export interface Prefs {
  /** Null uses the system's Downloads folder. */
  downloadDir: string | null;
  /** New downloads go to the folder used last instead of `downloadDir`. */
  rememberLastDir: boolean;
  /** Browser downloads wait in the add dialog instead of starting right away. */
  confirmBrowserDownloads: boolean;
  closeToTray: boolean;
  notify: boolean;
}

export type EngineEvent =
  | ({ type: "added" } & DownloadRecord)
  | ({ type: "progress" } & Progress)
  | { type: "statusChanged"; id: number; status: Status; error: string | null }
  | ({ type: "updated" } & DownloadRecord)
  | { type: "removed"; id: number }
  | { type: "queuesChanged" }
  | ({ type: "settingsChanged" } & Settings);

export interface AddRequest {
  url: string;
  dir: string;
  fileName: string | null;
  connections: number | null;
  queue: number;
  start: boolean;
  /** Passed on from the browser, such as its cookies. */
  headers?: [string, string][];
}

/** A download handed over by the browser extension, waiting for the user to confirm it. */
export interface Incoming {
  url: string;
  fileName: string | null;
  headers: [string, string][];
}

export const api = {
  list: () => invoke<DownloadRecord[]>("list_downloads"),
  get: (id: number) => invoke<DownloadRecord>("get_download", { id }),
  /** The folder the add dialog starts in. */
  defaultDir: () => invoke<string | null>("default_dir"),
  /** The system's Downloads folder. */
  systemDownloadDir: () => invoke<string | null>("system_download_dir"),
  inspect: (url: string, headers: [string, string][]) =>
    invoke<LinkInfo>("inspect_link", { url, headers }),
  freeSpace: (dir: string) => invoke<number | null>("free_space", { dir }),
  add: (request: AddRequest) => invoke<number>("add_download", { request }),
  start: (id: number) => invoke<void>("start_download", { id }),
  pause: (id: number) => invoke<void>("pause_download", { id }),
  restart: (id: number) => invoke<void>("restart_download", { id }),
  setUrl: (id: number, url: string) => invoke<void>("set_download_url", { id, url }),
  remove: (id: number, deleteFiles: boolean) =>
    invoke<void>("remove_download", { id, deleteFiles }),
  setQueue: (id: number, queue: number) => invoke<void>("set_download_queue", { id, queue }),
  setDownloadSpeedLimit: (id: number, limit: number | null) =>
    invoke<void>("set_download_speed_limit", { id, limit }),
  queues: () => invoke<Queue[]>("list_queues"),
  addQueue: (queue: Queue) => invoke<Queue>("add_queue", { queue }),
  updateQueue: (queue: Queue) => invoke<void>("update_queue", { queue }),
  removeQueue: (id: number) => invoke<void>("remove_queue", { id }),
  startQueue: (id: number) => invoke<void>("start_queue", { id }),
  stopQueue: (id: number) => invoke<void>("stop_queue", { id }),
  settings: () => invoke<Settings>("get_settings"),
  setSettings: (settings: Settings) => invoke<void>("set_settings", { settings }),
  setSpeedLimit: (limit: number | null) => invoke<void>("set_speed_limit", { limit }),
  prefs: () => invoke<Prefs>("get_prefs"),
  setPrefs: (prefs: Prefs) => invoke<void>("set_prefs", { new: prefs }),
  launchAtLogin: () => invoke<boolean>("get_launch_at_login"),
  setLaunchAtLogin: (enabled: boolean) => invoke<void>("set_launch_at_login", { enabled }),
  openLogFolder: () => invoke<void>("open_log_folder"),
  takeBrowserDownloads: () => invoke<Incoming[]>("take_browser_downloads"),
  openFile: (id: number) => invoke<void>("open_file", { id }),
  showInFolder: (id: number) => invoke<void>("show_in_folder", { id }),
  /** PNG bytes. */
  fileIcon: (id: number, size: number) => invoke<ArrayBuffer>("file_icon", { id, size }),
};

export function onEngineEvent(handler: (event: EngineEvent) => void): Promise<UnlistenFn> {
  return listen<EngineEvent>("download-event", (e) => handler(e.payload));
}

/** Fired when the UI missed events and should reload the whole list. */
export function onResync(handler: () => void): Promise<UnlistenFn> {
  return listen("downloads-resync", handler);
}

/** Fired when the browser handed over downloads; take them with `api.takeBrowserDownloads`. */
export function onBrowserDownload(handler: () => void): Promise<UnlistenFn> {
  return listen("browser-download", handler);
}
