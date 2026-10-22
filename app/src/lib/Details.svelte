<script lang="ts">
  import { clock } from "./clock.svelte";
  import { downloads, retryIn, STATUS_LABELS, type Item } from "./downloads.svelte";
  import { bytes, date, duration } from "./format";

  let {
    item,
    onlimit,
    onaddress,
  }: { item: Item; onlimit: () => void; onaddress: () => void } = $props();

  // A running download is paused first; a finished one needs no new address.
  let canChangeAddress = $derived(["paused", "failed", "retrying"].includes(item.status));
  let wait = $derived(retryIn(item, clock.now));

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
    <dd>
      <span class="selectable">{item.url}</span>
      {#if canChangeAddress}· <button class="link" onclick={onaddress}>Change</button>{/if}
    </dd>
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
      {#if item.status === "downloading" && item.problem}
        · <span class="warn">reconnecting</span>
      {:else if item.status === "downloading" && item.stalled}
        · <span class="warn">no data arriving</span>
      {:else if item.status === "downloading"}
        · {item.activeConnections} connections
      {/if}
      {#if item.status === "retrying" && item.retry}
        · attempt {item.retry.attempt}, {wait ? `in ${duration(wait)}` : "starting"}
      {/if}
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
    {#if item.status === "downloading" && item.problem}
      <dt>Connection</dt>
      <dd class="error-text selectable">{item.problem}; trying again</dd>
    {/if}
    {#if item.error}
      <dt>{item.status === "retrying" ? "Last error" : "Error"}</dt>
      <dd class="error-text selectable">{item.error}</dd>
    {/if}
  </dl>
</section>
