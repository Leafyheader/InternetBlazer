// The UI's copy of the engine's downloads, queues and settings, kept current by engine events.

import {
  api,
  onEngineEvent,
  onResync,
  type DownloadRecord,
  type Queue,
  type Status,
} from "./api";

export interface Item extends DownloadRecord {
  downloaded: number;
  /** Bytes per second; 0 when not downloading. */
  speed: number;
  /** Connections currently receiving data. */
  activeConnections: number;
  /** While downloading: nothing has arrived for a few seconds. */
  stalled: boolean;
  /** While stalled: why connections last failed. */
  problem: string | null;
}

export type Filter = "all" | "active" | "paused" | "completed" | "failed";

export const FILTERS: { id: Filter; label: string; matches: (s: Status) => boolean }[] = [
  { id: "all", label: "All downloads", matches: () => true },
  {
    id: "active",
    label: "Downloading",
    matches: (s) => s === "downloading" || s === "queued" || s === "retrying",
  },
  { id: "paused", label: "Paused", matches: (s) => s === "paused" },
  { id: "completed", label: "Completed", matches: (s) => s === "completed" },
  { id: "failed", label: "Failed", matches: (s) => s === "failed" },
];

/** What the list shows: downloads with some status, or the downloads of one queue. */
export type View = { filter: Filter } | { queue: number };

export function inView(view: View, item: Item): boolean {
  if ("queue" in view) return item.queue === view.queue;
  return FILTERS.find((f) => f.id === view.filter)!.matches(item.status);
}

function toItem(record: DownloadRecord, previous?: Item): Item {
  const downloaded =
    record.status === "completed" && record.total !== null
      ? record.total
      : record.segments.reduce((sum, s) => sum + s.downloaded, 0);
  const running = record.status === "downloading";
  return {
    ...record,
    downloaded,
    speed: running ? (previous?.speed ?? 0) : 0,
    activeConnections: running ? (previous?.activeConnections ?? 0) : 0,
    stalled: running && (previous?.stalled ?? false),
    problem: running ? (previous?.problem ?? null) : null,
  };
}

class Downloads {
  items = $state<Record<number, Item>>({});
  queues = $state<Queue[]>([]);
  /** The global limit in bytes per second. */
  speedLimit = $state<number | null>(null);

  /** Newest first. */
  sorted = $derived(Object.values(this.items).sort((a, b) => b.id - a.id));

  totalSpeed = $derived(this.sorted.reduce((sum, item) => sum + item.speed, 0));

  async load() {
    const [records, queues, settings] = await Promise.all([
      api.list(),
      api.queues(),
      api.settings(),
    ]);
    this.items = Object.fromEntries(records.map((r) => [r.id, toItem(r, this.items[r.id])]));
    this.queues = queues;
    this.speedLimit = settings.speedLimit;
  }

  async loadQueues() {
    this.queues = await api.queues();
  }

  async refresh(id: number) {
    try {
      const record = await api.get(id);
      if (this.items[id]) this.items[id] = toItem(record, this.items[id]);
    } catch {
      // Removed in the meantime; the `removed` event takes care of it.
    }
  }

  queueName(id: number): string {
    return this.queues.find((q) => q.id === id)?.name ?? "Main queue";
  }

  /** Subscribes to engine events and loads everything. Returns a function that unsubscribes. */
  async connect(onRemoved: (id: number) => void): Promise<() => void> {
    const unlisten = await Promise.all([
      onEngineEvent((event) => {
        switch (event.type) {
          case "added": {
            const { type: _, ...record } = event;
            this.items[record.id] = toItem(record);
            break;
          }
          case "progress": {
            const item = this.items[event.id];
            if (!item) break;
            item.downloaded = event.downloaded;
            item.total = event.total;
            item.speed = event.speed;
            item.activeConnections = event.connections;
            item.stalled = event.stalled;
            item.problem = event.problem;
            item.segments = event.segments;
            break;
          }
          case "statusChanged": {
            const item = this.items[event.id];
            if (!item) break;
            item.status = event.status;
            item.error = event.error;
            if (event.status !== "downloading") {
              item.speed = 0;
              item.activeConnections = 0;
              item.stalled = false;
              item.problem = null;
            }
            // Picks up what changed besides the status, such as the size after a restart.
            this.refresh(event.id);
            break;
          }
          case "updated": {
            const item = this.items[event.id];
            if (!item) break;
            if (item.status !== "downloading") {
              const { type: _, ...record } = event;
              this.items[event.id] = toItem(record, item);
              break;
            }
            // Only the settings are news; the stored progress of a running download lags.
            item.queue = event.queue;
            item.speedLimit = event.speedLimit;
            break;
          }
          case "removed":
            delete this.items[event.id];
            onRemoved(event.id);
            break;
          case "queuesChanged":
            this.loadQueues();
            break;
          case "settingsChanged":
            this.speedLimit = event.speedLimit;
            break;
        }
      }),
      onResync(() => this.load()),
    ]);
    await this.load();
    return () => unlisten.forEach((f) => f());
  }
}

export const downloads = new Downloads();

export function percent(item: Item): number | null {
  if (item.status === "completed") return 100;
  if (item.total === null) return null;
  if (item.total === 0) return 0;
  return Math.min(100, (item.downloaded * 100) / item.total);
}

/** Seconds left, or null when unknown. */
export function eta(item: Item): number | null {
  if (item.total === null || item.speed === 0) return null;
  return Math.max(0, item.total - item.downloaded) / item.speed;
}

export const STATUS_LABELS: Record<Status, string> = {
  queued: "Queued",
  downloading: "Downloading",
  retrying: "Retrying",
  paused: "Paused",
  completed: "Completed",
  failed: "Failed",
};

/** Seconds until the next automatic retry, or null when none is pending. */
export function retryIn(item: Item, now: number): number | null {
  if (item.status !== "retrying" || !item.retry) return null;
  return Math.max(0, item.retry.at - now);
}
