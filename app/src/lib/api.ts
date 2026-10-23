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
  /** Set for a video on a web page, which yt-dlp downloads. */
  media: MediaFormat | null;
  segments: Segment[];
  status: Status;
  error: string | null;
  retry: Retry | null;
  /** Running when the app last quit or crashed; the app resumes these as it starts. */
  interrupted: boolean;
  /** Unix seconds. */
  addedAt: number;
  completedAt: number | null;
  /** When it last started downloading. Null if never, or if added before this was kept. */
  lastTryAt: number | null;
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
  /** A web page rather than a file, which may have a video on it. */
  page: boolean;
}

/** What a media download fetches: the best video up to a height, or only the sound. */
export type MediaFormat = { type: "video"; maxHeight: number | null } | { type: "audio" };

/** The videos yt-dlp found on a page. */
export interface MediaInfo {
  title: string;
  site: string;
  /** Seconds. */
  duration: number | null;
  /** Best first, then the audio-only choice if there is one. */
  choices: MediaChoice[];
  /** Without ffmpeg, sites that keep picture and sound apart offer few qualities or none. */
  ffmpeg: boolean;
}

export interface MediaChoice {
  /** Such as "1080p60" or "Audio only". */
  label: string;
  format: MediaFormat;
  /** Likely extension, without the dot. */
  ext: string;
  /** An estimate. */
  size: number | null;
}

export interface Tool {
  path: string;
  version: string;
  /** Installed by the app, which can update it; otherwise found on PATH. */
  managed: boolean;
}

/** The programs video downloads use. */
export interface MediaTools {
  ytdlp: Tool | null;
  /** Runs YouTube's JavaScript for yt-dlp. Without it, YouTube offers fewer formats. */
  deno: Tool | null;
  ffmpeg: string | null;
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
  /** New downloads go to a folder for their file type. Wins over `rememberLastDir`. */
  sortByType: boolean;
  /** Folders picked for some file types; the others use a subfolder of `downloadDir`. */
  categoryDirs: Partial<Record<Category, string>>;
  /** Browser downloads wait in the add dialog instead of starting right away. */
  confirmBrowserDownloads: boolean;
  closeToTray: boolean;
  notify: boolean;
  /** Looks for a newer version each time the app starts. */
  checkUpdates: boolean;
}

export type Category = "video" | "music" | "compressed" | "documents" | "programs" | "other";

/** A file type that downloads are sorted by. */
export interface CategoryInfo {
  id: Category;
  /** Also the name of its subfolder. */
  name: string;
  /** Lower-case, without the dot. */
  extensions: string[];
}

export interface UpdateInfo {
  version: string;
  notes: string | null;
}

/** What the computer or app does once a queue finishes. */
export type FinishAction = "shutdown" | "sleep" | "hibernate" | "quit";

export const FINISH_LABELS: Record<FinishAction, string> = {
  shutdown: "Shut down",
  sleep: "Sleep",
  hibernate: "Hibernate",
  quit: "Quit InternetBlazer",
};

/** A queue finished; its action happens after `seconds` unless cancelled. */
export interface Countdown {
  queue: string;
  action: FinishAction;
  seconds: number;
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
  /** Download the video on the page with yt-dlp. */
  media?: MediaFormat | null;
}

export interface ImportRequest {
  urls: string[];
  /** One folder for every link, or null for the folder of each file's type. */
  dir: string | null;
  queue: number;
  start: boolean;
}

/** A link of a list that couldn't be added, and why. */
export interface ImportFailure {
  url: string;
  error: string;
}

/** A download handed over by the browser extension, waiting for the user to confirm it. */
export interface Incoming {
  url: string;
  fileName: string | null;
  headers: [string, string][];
  /** The quality picked on the page, for a video. */
  media: MediaFormat | null;
}

export const api = {
  list: () => invoke<DownloadRecord[]>("list_downloads"),
  get: (id: number) => invoke<DownloadRecord>("get_download", { id }),
  /** The folder the add dialog suggests for a file, going by its name. */
  defaultDir: (fileName: string, media: boolean) =>
    invoke<string | null>("default_dir", { fileName, media }),
  fileCategories: () => invoke<CategoryInfo[]>("file_categories"),
  /** The system's Downloads folder. */
  systemDownloadDir: () => invoke<string | null>("system_download_dir"),
  inspect: (url: string, headers: [string, string][]) =>
    invoke<LinkInfo>("inspect_link", { url, headers }),
  inspectMedia: (url: string, headers: [string, string][]) =>
    invoke<MediaInfo>("inspect_media", { url, headers }),
  mediaTools: () => invoke<MediaTools>("media_tools"),
  /** Installs or updates yt-dlp. */
  installYtdlp: () => invoke<Tool>("install_ytdlp"),
  installDeno: () => invoke<Tool>("install_deno"),
  freeSpace: (dir: string) => invoke<number | null>("free_space", { dir }),
  add: (request: AddRequest) => invoke<number>("add_download", { request }),
  /** Adds a list of links and returns the ones that couldn't be added. */
  importLinks: (request: ImportRequest) => invoke<ImportFailure[]>("import_links", { request }),
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
  /** The queues that do something when they finish. */
  finishActions: () => invoke<Record<number, FinishAction>>("finish_actions"),
  /** Happens once, the next time the queue finishes; `null` clears it. */
  setFinishAction: (queue: number, action: FinishAction | null) =>
    invoke<void>("set_finish_action", { queue, action }),
  cancelFinish: () => invoke<void>("cancel_finish"),
  finishNow: () => invoke<void>("finish_now"),
  settings: () => invoke<Settings>("get_settings"),
  setSettings: (settings: Settings) => invoke<void>("set_settings", { settings }),
  setSpeedLimit: (limit: number | null) => invoke<void>("set_speed_limit", { limit }),
  prefs: () => invoke<Prefs>("get_prefs"),
  setPrefs: (prefs: Prefs) => invoke<void>("set_prefs", { new: prefs }),
  launchAtLogin: () => invoke<boolean>("get_launch_at_login"),
  setLaunchAtLogin: (enabled: boolean) => invoke<void>("set_launch_at_login", { enabled }),
  openLogFolder: () => invoke<void>("open_log_folder"),
  openProjectPage: (page: "home" | "releases" | "issues" | "license") =>
    invoke<void>("open_project_page", { page }),
  /** Null when this is the newest version. */
  checkUpdate: () => invoke<UpdateInfo | null>("check_update"),
  /** Ends the app; the new version starts in its place. */
  installUpdate: () => invoke<void>("install_update"),
  takeBrowserDownloads: () => invoke<Incoming[]>("take_browser_downloads"),
  openFile: (id: number) => invoke<void>("open_file", { id }),
  /** The system's "Open with" dialog; Windows only. */
  openWith: (id: number) => invoke<void>("open_with", { id }),
  /** Moves or renames the file, or a paused download's partial data. */
  move: (id: number, dir: string, fileName: string) =>
    invoke<void>("move_download", { id, dir, fileName }),
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

export function onFinishActions(
  handler: (actions: Record<number, FinishAction>) => void,
): Promise<UnlistenFn> {
  return listen<Record<number, FinishAction>>("finish-actions", (e) => handler(e.payload));
}

/** A queue's finish action is counting down, or `null` once it's done or cancelled. */
export function onFinishCountdown(handler: (countdown: Countdown | null) => void): Promise<UnlistenFn> {
  return listen<Countdown | null>("finish-countdown", (e) => handler(e.payload));
}

/** "About InternetBlazer" in the tray menu. */
export function onShowAbout(handler: () => void): Promise<UnlistenFn> {
  return listen("show-about", handler);
}
