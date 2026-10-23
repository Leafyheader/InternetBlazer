// The details panel under the download list: whether it shows, and the height the user dragged it to.

const KEY = "detailsPanel";
const MIN = 80;

class DetailsPanel {
  shown = $state(true);
  /** In px; null is the panel's own height, up to a share of the window. */
  height = $state<number | null>(null);

  constructor() {
    try {
      const saved = JSON.parse(localStorage.getItem(KEY) ?? "{}");
      if (typeof saved.shown === "boolean") this.shown = saved.shown;
      if (typeof saved.height === "number") this.height = Math.max(MIN, saved.height);
    } catch {
      // Storage can be unavailable or hold something else; the defaults do.
    }
  }

  toggle() {
    this.shown = !this.shown;
    this.save();
  }

  /** Follows the pointer until the button is let go; dragging up makes the panel taller. */
  startResize(e: PointerEvent, panel: HTMLElement) {
    if (e.button !== 0) return;
    e.preventDefault();
    const grip = e.currentTarget as HTMLElement;
    grip.setPointerCapture(e.pointerId);
    const startY = e.clientY;
    const start = panel.getBoundingClientRect().height;
    // Leave the toolbar and a few rows of the list in view.
    const max = Math.max(MIN, (panel.parentElement?.clientHeight ?? 600) - 160);
    const move = (ev: PointerEvent) => {
      this.height = Math.round(Math.min(max, Math.max(MIN, start + startY - ev.clientY)));
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

  reset() {
    this.height = null;
    this.save();
  }

  private save() {
    try {
      localStorage.setItem(KEY, JSON.stringify({ shown: this.shown, height: this.height }));
    } catch {
      // Not saved; the choice still holds until the app closes.
    }
  }
}

export const detailsPanel = new DetailsPanel();
