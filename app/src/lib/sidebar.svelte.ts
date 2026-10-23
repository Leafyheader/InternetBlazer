// The sidebar: whether it's narrowed to icons, and which of its sections are folded away.

const KEY = "sidebar";

export type Section = "filters" | "types" | "queues";

class Sidebar {
  /** Only icons, with the names in tooltips. */
  narrow = $state(false);
  folded = $state<Section[]>([]);

  constructor() {
    try {
      const saved = JSON.parse(localStorage.getItem(KEY) ?? "{}");
      if (typeof saved.narrow === "boolean") this.narrow = saved.narrow;
      if (Array.isArray(saved.folded)) this.folded = saved.folded.filter((s: unknown) => typeof s === "string");
    } catch {
      // Storage can be unavailable or hold something else; the defaults do.
    }
  }

  toggleNarrow() {
    this.narrow = !this.narrow;
    this.save();
  }

  /** Narrowed, every section shows: its header, which folds it, isn't there. */
  isOpen(section: Section) {
    return this.narrow || !this.folded.includes(section);
  }

  toggle(section: Section) {
    this.folded = this.folded.includes(section)
      ? this.folded.filter((s) => s !== section)
      : [...this.folded, section];
    this.save();
  }

  private save() {
    try {
      localStorage.setItem(KEY, JSON.stringify({ narrow: this.narrow, folded: this.folded }));
    } catch {
      // Not saved; the choice still holds until the app closes.
    }
  }
}

export const sidebar = new Sidebar();
