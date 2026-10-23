<script lang="ts">
  import { clock } from "./clock.svelte";
  import { downloads, retryIn, STATUS_LABELS, type Item } from "./downloads.svelte";
  import { bytes, date, duration, quality } from "./format";

  let {
    item,
    onlimit,
    onaddress,
  }: { item: Item; onlimit: () => void; onaddress: () => void } = $props();

  // A running download is paused first; a finished one needs no new address.
  let canChangeAddress = $derived(["paused", "failed", "retrying"].includes(item.status));
  let wait = $derived(retryIn(item, clock.now));

  /** A speed this close to a limit counts as held back by it. */
  const AT_LIMIT = 0.8;

  const downloadsText = (n: number) => (n === 1 ? "1 other download" : `${n} other downloads`);

  /** What most likely sets the pace of a running download, most telling first. */
  let pace = $derived.by(() => {
    // No progress report yet, or it shows its own trouble.
    if (item.status !== "downloading" || item.planned === 0 || item.stalled || item.problem) return null;
    const others = downloads.sorted.filter((i) => i.status === "downloading" && i.id !== item.id).length;
    if (item.speedLimit !== null && item.speed >= item.speedLimit * AT_LIMIT) {
      return "held to this download's speed limit";
    }
    if (downloads.speedLimit !== null && downloads.totalSpeed >= downloads.speedLimit * AT_LIMIT) {
      return others ? `held to the overall speed limit, shared with ${downloadsText(others)}` : "held to the overall speed limit";
    }
    if (item.media) return "yt-dlp downloads it, at the pace the site allows";
    if (!item.resumable) return "the server sends this file over one connection only";
    if (item.total === null) return "the server didn't say the size, so it comes over one connection";
    if (item.planned === 1) return "set to use one connection";
    if (item.refused > 0) {
      return `the server allows only ${item.planned - item.refused} of ${item.planned} connections`;
    }
    if (item.idle > 0) return "what's left is too small to split among more connections";
    if (others) return `sharing your internet with ${downloadsText(others)}`;
    return `using all ${item.planned} connections; the server and your network set the pace`;
  });

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
      {:else if item.status === "downloading" && !item.media}
        · {item.activeConnections} connections
      {/if}
      {#if item.status === "retrying" && item.retry}
        · attempt {item.retry.attempt}, {wait ? `in ${duration(wait)}` : "starting"}
      {/if}
    </dd>
    {#if pace}
      <dt>Speed</dt>
      <dd>{bytes(item.speed)}/s · {pace}</dd>
    {/if}
    {#if item.media}
      <dt>Video</dt>
      <dd>{quality(item.media)} · downloaded by yt-dlp</dd>
    {/if}
    <dt>Resume</dt>
    <dd>
      {#if item.media}
        Supported · yt-dlp carries on where it stopped
      {:else if item.resumable}
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
