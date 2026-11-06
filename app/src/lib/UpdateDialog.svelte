<script lang="ts">
  import { onMount } from "svelte";
  import { api, type UpdateInfo } from "./api";

  let { update, onclose }: { update: UpdateInfo; onclose: () => void } = $props();

  let installing = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    installing = true;
    error = "";
    try {
      // Ends the app once installed; the new version starts in its place.
      await api.installUpdate();
    } catch (err) {
      installing = false;
      error = `Couldn't install the update: ${err}`;
    }
  }

  function cancel(e: Event) {
    // Closing mid-install would hide that the app is about to restart.
    if (installing) e.preventDefault();
  }
</script>

<dialog bind:this={dialog} onclose={onclose} oncancel={cancel} aria-labelledby="update-title">
  <form onsubmit={submit}>
    <h2 id="update-title">InternetBlazer {update.version} is available</h2>
    {#if update.notes}
      <p class="notes muted">{update.notes}</p>
    {/if}
    <p class="muted">
      {installing
        ? "Downloading the update. InternetBlazer restarts when it's installed, and downloads carry on where they were."
        : "The update downloads now and InternetBlazer restarts with it. Running downloads pause and carry on afterwards."}
    </p>
    {#if error}
      <p class="error" role="alert">{error}</p>
    {/if}
    <footer>
      <button type="button" onclick={() => dialog.close()} disabled={installing}>Later</button>
      <button type="submit" class="primary" disabled={installing}>
        {installing ? "Downloading…" : "Download and restart"}
      </button>
    </footer>
  </form>
</dialog>

<style>
  .notes {
    white-space: pre-wrap;
    max-height: 12rem;
    overflow-y: auto;
  }
</style>
