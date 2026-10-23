// The download list's columns, whose widths the user sets by dragging the header's edges.

export type ColumnId = "name" | "size" | "progress" | "speed" | "eta" | "lastTry";

export const COLUMNS: { id: ColumnId; label: string; num?: boolean; width: number; min: number }[] = [
  { id: "name", label: "Name", width: 320, min: 120 },
  { id: "size", label: "Size", width: 80, min: 50 },
  { id: "progress", label: "Progress", width: 130, min: 80 },
  { id: "speed", label: "Speed", num: true, width: 90, min: 60 },
  { id: "eta", label: "Time left", num: true, width: 80, min: 60 },
  { id: "lastTry", label: "Last try", width: 130, min: 80 },
];

const KEY = "columnWidths";

function defaults(): Record<ColumnId, number> {
  return Object.fromEntries(COLUMNS.map((c) => [c.id, c.width])) as Record<ColumnId, number>;
}

/** Widths saved in an earlier session, with the defaults for any missing or damaged. */
function load(): Record<ColumnId, number> {
  const widths = defaults();
  try {
    const saved = JSON.parse(localStorage.getItem(KEY) ?? "{}");
    for (const c of COLUMNS) {
      if (typeof saved[c.id] === "number") widths[c.id] = Math.max(c.min, saved[c.id]);
    }
  } catch {
    // Storage can be unavailable or hold something else; the defaults do.
  }
  return widths;
}

class Columns {
  widths = $state(load());

  /** For `grid-template-columns`. */
  template = $derived(COLUMNS.map((c) => `${this.widths[c.id]}px`).join(" "));

  /** Follows the pointer until the button is let go, then saves the widths. */
  startResize(e: PointerEvent, id: ColumnId) {
    if (e.button !== 0) return;
    e.preventDefault();
    const grip = e.currentTarget as HTMLElement;
    grip.setPointerCapture(e.pointerId);
    const startX = e.clientX;
    const start = this.widths[id];
    const min = COLUMNS.find((c) => c.id === id)!.min;
    const move = (ev: PointerEvent) => {
      this.widths[id] = Math.max(min, Math.round(start + ev.clientX - startX));
    };
    const done = () => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", done);
      grip.removeEventListener("pointercancel", done);
      this.save();
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", done);
    grip.addEventListener("pointercancel", done);
  }

  reset(id: ColumnId) {
    this.widths[id] = COLUMNS.find((c) => c.id === id)!.width;
    this.save();
  }

  private save() {
    try {
      localStorage.setItem(KEY, JSON.stringify(this.widths));
    } catch {
      // Not saved; the widths still hold until the app closes.
    }
  }
}

export const columns = new Columns();
