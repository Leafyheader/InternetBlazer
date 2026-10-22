<script lang="ts">
  import { onMount } from "svelte";
  import Icon from "./Icon.svelte";

  let {
    title,
    note,
    limit,
    onsave,
    onclose,
  }: {
    title: string;
    note: string;
    /** Bytes per second, or null for no limit. */
    limit: number | null;
    onsave: (limit: number | null) => Promise<void>;
    onclose: () => void;
  } = $props();

  const UNITS = { KB: 1024, MB: 1024 * 1024 };
  type Unit = keyof typeof UNITS;

  // Initial values only; the dialog edits its own copy.
  // svelte-ignore state_referenced_locally
  const initial = (() => {
    if (limit === null) return { value: 1, unit: "MB" as Unit };
    const unit: Unit = limit >= UNITS.MB ? "MB" : "KB";
    return { value: Math.round((limit / UNITS[unit]) * 10) / 10, unit };
  })();

  // svelte-ignore state_referenced_locally
  let enabled = $state(limit !== null);
  let value = $state(initial.value);
  let unit = $state<Unit>(initial.unit);
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = "";
    try {
      await onsave(enabled ? Math.round(value * UNITS[unit]) : null);
      dialog.close();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="limit-title">
  <form onsubmit={submit}>
    <header>
      <h2 id="limit-title">{title}</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>
    <p class="muted">{note}</p>

    <label class="check">
      <input type="checkbox" bind:checked={enabled} />
      Limit the speed
    </label>
    <div class="inline">
      <label>
        <span>Speed</span>
        <input type="number" bind:value min="0.1" step="any" required class="narrow" disabled={!enabled} />
      </label>
      <label>
        <span>Unit</span>
        <select bind:value={unit} disabled={!enabled}>
          <option value="KB">KB/s</option>
          <option value="MB">MB/s</option>
        </select>
      </label>
    </div>

    {#if error}
      <p class="error" role="alert"><Icon name="alert" size={16} />{error}</p>
    {/if}

    <footer>
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="submit" class="primary" disabled={busy}>Save</button>
    </footer>
  </form>
</dialog>
