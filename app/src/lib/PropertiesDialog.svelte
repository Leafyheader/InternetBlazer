<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "./api";
  import Details from "./Details.svelte";
  import type { Item } from "./downloads.svelte";
  import FileIcon from "./FileIcon.svelte";
  import { parseSha256 } from "./format";
  import Icon from "./Icon.svelte";

  let {
    item,
    onclose,
    onlimit,
    onaddress,
  }: { item: Item; onclose: () => void; onlimit: () => void; onaddress: () => void } = $props();

  let dialog: HTMLDialogElement;

  // svelte-ignore state_referenced_locally
  let sha256 = $state(item.checksum?.expected ?? "");
  let parsed = $derived(parseSha256(sha256));
  let empty = $derived(sha256.trim() === "");
  let changed = $derived((empty ? null : parsed) !== (item.checksum?.expected ?? null));
  let shaError = $state("");

  async function saveSha256() {
    shaError = "";
    try {
      await api.setSha256(item.id, empty ? null : sha256);
      if (parsed) sha256 = parsed;
    } catch (err) {
      shaError = String(err);
    }
  }

  onMount(() => dialog.showModal());
</script>

<dialog bind:this={dialog} class="properties" onclose={onclose} aria-labelledby="properties-title">
  <!-- Not submitted: the links in the details are buttons too. -->
  <form onsubmit={(e) => e.preventDefault()}>
    <header>
      <span class="title">
        <FileIcon {item} />
        <h2 id="properties-title" class="selectable">{item.fileName}</h2>
      </span>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>
    <Details {item} {onlimit} {onaddress} />
    <label class="sha256">
      <span>SHA-256 the file should have</span>
      <div class="inline">
        <input
          bind:value={sha256}
          placeholder="Paste the hash the site gives"
          spellcheck="false"
          aria-invalid={!empty && !parsed}
          onkeydown={(e) => e.key === "Enter" && changed && (empty || parsed) && saveSha256()}
        />
        <button type="button" onclick={saveSha256} disabled={!changed || (!empty && !parsed)}>
          {empty ? "Remove" : item.status === "completed" ? "Check" : "Save"}
        </button>
      </div>
      {#if !empty && !parsed}
        <span class="link-info warn">A SHA-256 is 64 characters of 0-9 and a-f.</span>
      {:else if shaError}
        <span class="link-info error-text">{shaError}</span>
      {/if}
    </label>
    <footer>
      <button type="button" class="primary" onclick={() => dialog.close()}>Close</button>
    </footer>
  </form>
</dialog>
