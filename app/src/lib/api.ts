// Typed wrappers around the Tauri commands and events in `src-tauri/src/main.rs`. The types
// mirror the serde output of `blazer-core`.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Status = "queued" | "downloading" | "paused" | "completed" | "failed";

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
  segments: Segment[];
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

export interface Settings {
  /** Bytes per second across all downloads. */
  speedLimit: number | null;
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
  defaultDir: () => invoke<string | null>("default_dir"),
  add: (request: AddRequest) => invoke<number>("add_download", { request }),
  start: (id: number) => invoke<void>("start_download", { id }),
  pause: (id: number) => invoke<void>("pause_download", { id }),
  restart: (id: number) => invoke<void>("restart_download", { id }),
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
  setSpeedLimit: (limit: number | null) => invoke<void>("set_speed_limit", { limit }),
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
