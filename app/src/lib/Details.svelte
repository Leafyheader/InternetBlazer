<script lang="ts">
  import { downloads, STATUS_LABELS, type Item } from "./downloads.svelte";
  import { bytes, date } from "./format";

  let { item, onlimit }: { item: Item; onlimit: () => void } = $props();

  let path = $derived.by(() => {
    const sep = item.dir.includes("\\") ? "\\" : "/";
    return item.dir.replace(/[\\/]$/, "") + sep + item.fileName;
  });
</script>

<section class="details" aria-label="Download details">
  <div class="segments" class:unknown={item.total === null} title="Segments">
    {#if item.status === "completed"}
      <div class="segment" style="left: 0; width: 100%"><div class="fill" style="width: 100%"></div></div>
    {:else if item.total}
      {#each item.segments as s (s.start)}
        <div
          class="segment"
          style="left: {(s.start / item.total) * 100}%; width: {((s.end - s.start) / item.total) * 100}%"
        >
          <div class="fill" style="width: {(s.downloaded / Math.max(1, s.end - s.start)) * 100}%"></div>
        </div>
      {/each}
    {/if}
  </div>

  <dl>
    <dt>Address</dt>
    <dd class="selectable">{item.url}</dd>
    <dt>Saved to</dt>
    <dd class="selectable">{path}</dd>
    <dt>Size</dt>
    <dd>
      {item.total === null ? "Unknown" : bytes(item.total)}
      {#if item.status !== "completed"}· {bytes(item.downloaded)} done{/if}
    </dd>
    <dt>Status</dt>
    <dd>
      {STATUS_LABELS[item.status]}
      {#if item.status === "downloading"}· {item.activeConnections} connections{/if}
    </dd>
    <dt>Resume</dt>
    <dd>
      {#if item.resumable}
        Supported · pausing keeps what's downloaded
      {:else}
        <span class="warn">Not supported</span> · pausing or a lost connection starts over
      {/if}
    </dd>
    <dt>Queue</dt>
    <dd>{downloads.queueName(item.queue)}</dd>
    <dt>Speed limit</dt>
    <dd>
      {item.speedLimit === null ? "None" : `${bytes(item.speedLimit)}/s`}
      · <button class="link" onclick={onlimit}>Change</button>
    </dd>
    <dt>Added</dt>
    <dd>{date(item.addedAt)}{#if item.completedAt} · finished {date(item.completedAt)}{/if}</dd>
    {#if item.error}
      <dt>Error</dt>
      <dd class="error-text selectable">{item.error}</dd>
    {/if}
  </dl>
</section>
