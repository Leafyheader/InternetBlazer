<script lang="ts">
  import type { Item } from "./downloads.svelte";
  import { fileIcon, ICON_SIZE } from "./fileIcons.svelte";
  import { extension, kind } from "./format";

  let { item }: { item: Item } = $props();

  let ext = $derived(extension(item.fileName));
  let src = $derived(fileIcon(item));
</script>

{#if src}
  <img class="file-icon" {src} width={ICON_SIZE} height={ICON_SIZE} alt="" />
{:else}
  <!-- Shown while the system icon loads too, so rows don't shift. -->
  <span class="badge {kind(ext)}">{ext.slice(0, 4) || "FILE"}</span>
{/if}
