// System file icons, fetched once per file type and kept as object URLs.

import { SvelteMap } from "svelte/reactivity";
import { api } from "./api";
import type { Item } from "./downloads.svelte";
import { extension } from "./format";

/** Rendered size in CSS pixels. */
export const ICON_SIZE = 24;

/** Types whose finished files carry their own icon, so each one is looked up separately. */
const OWN_ICON = new Set(["EXE", "LNK", "ICO", "APP"]);

/** An object URL, or `null` when the system has no icon for it. */
const icons = new SvelteMap<string, string | null>();

function key(item: Item): string {
  const ext = extension(item.fileName);
  return item.status === "completed" && OWN_ICON.has(ext) ? `file:${item.id}` : `ext:${ext}`;
}

/** The icon for `item`, `undefined` while it loads, or `null` if there is none. */
export function fileIcon(item: Item): string | null | undefined {
  const k = key(item);
  if (!icons.has(k)) {
    // Only writes after the request, so this can run inside a derived value.
    void load(k, item.id);
    return undefined;
  }
  return icons.get(k);
}

const loading = new Set<string>();

async function load(k: string, id: number) {
  if (icons.has(k) || loading.has(k)) return;
  loading.add(k);
  try {
    const size = Math.round(ICON_SIZE * Math.max(1, window.devicePixelRatio));
    const png = await api.fileIcon(id, size);
    icons.set(k, URL.createObjectURL(new Blob([png], { type: "image/png" })));
  } catch {
    icons.set(k, null);
  } finally {
    loading.delete(k);
  }
}
