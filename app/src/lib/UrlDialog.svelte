<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "./api";
  import type { Item } from "./downloads.svelte";
  import Icon from "./Icon.svelte";

  let { item, onclose }: { item: Item; onclose: () => void } = $props();

  // The initial value only; the dialog edits its own copy.
  // svelte-ignore state_referenced_locally
  let url = $state(item.url);
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());

  async function save(resume: boolean) {
    busy = true;
    error = "";
    try {
      await api.setUrl(item.id, url);
      if (resume) await api.start(item.id);
      dialog.close();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }

  function submit(e: SubmitEvent) {
    e.preventDefault();
    save(true);
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="url-title">
  <form onsubmit={submit}>
    <header>
      <h2 id="url-title">Change address</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>
    <p class="muted">
      Use a new link to the same file, for example when the old one expired. The download keeps
      what it has downloaded, so the new link must be for the same version of the file.
    </p>

    <label>
      <span>Address</span>
      <input type="url" bind:value={url} required spellcheck="false" />
    </label>

    {#if error}
      <p class="error" role="alert"><Icon name="alert" size={16} />{error}</p>
    {/if}

    <footer>
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="button" disabled={busy} onclick={() => save(false)}>Save</button>
      <button type="submit" class="primary" disabled={busy}>Save and resume</button>
    </footer>
  </form>
</dialog>
