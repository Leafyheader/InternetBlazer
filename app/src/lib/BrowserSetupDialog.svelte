<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api, type Browser } from "./api";
  import { installMessage } from "./browsers";

  let { browsers, onclose }: { browsers: Browser[]; onclose: () => void } = $props();

  // Every browser it can go into, and isn't in yet, starts ticked.
  let picked = $state<Record<string, boolean>>(
    // The list the dialog opened with; it doesn't change while open.
    untrack(() => Object.fromEntries(browsers.map((b) => [b.id, b.canInstall && !b.connected]))),
  );
  let busy = $state(false);
  let results = $state<string[]>([]);
  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    const done: string[] = [];
    for (const b of browsers.filter((b) => picked[b.id] && b.canInstall)) {
      try {
        done.push(installMessage(b, await api.installExtension(b.id)));
      } catch (err) {
        done.push(`${b.name}: ${err}`);
      }
    }
    results = done;
    busy = false;
  }

  function close() {
    api.browserSetupShown().catch(() => {});
    onclose();
  }
</script>

<dialog bind:this={dialog} onclose={close} aria-labelledby="browser-setup-title">
  <form onsubmit={submit}>
    <h2 id="browser-setup-title">Download from your browsers</h2>
    {#if results.length}
      <ul class="results" aria-live="polite">
        {#each results as line, i (i)}<li>{line}</li>{/each}
      </ul>
      <p class="muted">You can change this later in Settings, under Browsers.</p>
      <footer>
        <button type="button" class="primary" onclick={() => dialog.close()}>Done</button>
      </footer>
    {:else}
      <p class="muted">
        The InternetBlazer extension hands your browser's downloads to the app. Pick the browsers to add
        it to; each asks you to confirm before turning it on.
      </p>
      {#each browsers as b (b.id)}
        <label class="check">
          <input type="checkbox" bind:checked={picked[b.id]} disabled={!b.canInstall || b.connected} />
          {b.name}
          {#if b.connected}
            <span class="muted">· already added</span>
          {:else if !b.canInstall}
            <span class="muted">· not in the {b.store} yet</span>
          {/if}
        </label>
      {/each}
      <footer>
        <button type="button" onclick={() => dialog.close()} disabled={busy}>Not now</button>
        <button type="submit" class="primary" disabled={busy || !browsers.some((b) => picked[b.id] && b.canInstall)}>
          {busy ? "Adding…" : "Add extension"}
        </button>
      </footer>
    {/if}
  </form>
</dialog>

<style>
  .results {
    margin: 0;
    padding-left: 1.2rem;
  }
</style>
