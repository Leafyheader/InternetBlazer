<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "./api";
  import type { Item } from "./downloads.svelte";

  let { items, onclose }: { items: Item[]; onclose: () => void } = $props();

  let deleteFiles = $state(false);
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    const results = await Promise.allSettled(items.map((item) => api.remove(item.id, deleteFiles)));
    busy = false;
    const failed = results.find((r) => r.status === "rejected");
    if (failed) error = String(failed.reason);
    else dialog.close();
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="remove-title">
  <form onsubmit={submit}>
    <h2 id="remove-title">
      {items.length === 1 ? `Remove “${items[0].fileName}”?` : `Remove ${items.length} downloads?`}
    </h2>
    <p class="muted">The downloads are removed from the list.</p>
    <label class="check">
      <input type="checkbox" bind:checked={deleteFiles} />
      Also delete the files from disk
    </label>
    {#if error}
      <p class="error" role="alert">{error}</p>
    {/if}
    <footer>
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="submit" class="danger" disabled={busy}>Remove</button>
    </footer>
  </form>
</dialog>
