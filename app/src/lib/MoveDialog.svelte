<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { api } from "./api";
  import type { Item } from "./downloads.svelte";
  import Icon from "./Icon.svelte";

  let { item, onclose }: { item: Item; onclose: () => void } = $props();

  // The initial values only; the dialog edits its own copy.
  // svelte-ignore state_referenced_locally
  let dir = $state(item.dir);
  // svelte-ignore state_referenced_locally
  let fileName = $state(item.fileName);
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;
  let nameInput: HTMLInputElement;

  onMount(() => {
    dialog.showModal();
    // Selects the name without its extension, ready to type over.
    const dot = fileName.lastIndexOf(".");
    nameInput.focus();
    nameInput.setSelectionRange(0, dot > 0 ? dot : fileName.length);
  });

  async function browse() {
    const picked = await pickFolder({ directory: true, defaultPath: dir || undefined });
    if (typeof picked === "string") dir = picked;
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = "";
    try {
      await api.move(item.id, dir.trim(), fileName.trim());
      dialog.close();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="move-title">
  <form onsubmit={submit}>
    <header>
      <h2 id="move-title">Move or rename</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>
    {#if item.status !== "completed"}
      <p class="muted">
        What's downloaded so far moves along, and the download carries on in the new place.
      </p>
    {/if}

    <label>
      <span>Folder</span>
      <div class="inline">
        <input bind:value={dir} required spellcheck="false" />
        <button type="button" onclick={browse}><Icon name="folder" />Browse</button>
      </div>
    </label>
    <label>
      <span>File name</span>
      <input bind:this={nameInput} bind:value={fileName} required spellcheck="false" />
    </label>

    {#if error}
      <p class="error" role="alert"><Icon name="alert" size={16} />{error}</p>
    {/if}

    <footer>
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="submit" class="primary" disabled={busy}>{busy ? "Moving…" : "Move"}</button>
    </footer>
  </form>
</dialog>
