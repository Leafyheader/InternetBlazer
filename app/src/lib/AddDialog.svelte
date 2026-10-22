<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { api, type Incoming, type LinkInfo, MAIN_QUEUE } from "./api";
  import { downloads } from "./downloads.svelte";
  import { bytes } from "./format";
  import Icon from "./Icon.svelte";

  let {
    onclose,
    queue: initialQueue = MAIN_QUEUE,
    prefill,
  }: {
    onclose: () => void;
    queue?: number;
    /** A download from the browser. */
    prefill?: Incoming;
  } = $props();

  const LAST_DIR_KEY = "lastDir";

  // svelte-ignore state_referenced_locally
  let url = $state(prefill?.url ?? "");
  let dir = $state(readLastDir());
  // svelte-ignore state_referenced_locally
  let fileName = $state(prefill?.fileName ?? "");
  let connections = $state(8);
  // svelte-ignore state_referenced_locally
  let queue = $state(initialQueue);
  let start = $state(true);
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;

  /** What the link points to, checked while the user fills in the rest. */
  let info = $state<LinkInfo | null>(null);
  let checking = $state(false);
  let checkError = $state("");
  /** Bytes free where the file goes, or null when unknown. */
  let free = $state<number | null>(null);
  let tooBig = $derived(info?.total != null && free !== null && info.total > free);

  // Checks the link once typing pauses. An answer for an address that has changed since is
  // dropped. Only `url` and `prefill` may be read here: reading state this effect sets, such
  // as `checking`, would rerun it whenever a check finishes, and so check forever.
  $effect(() => {
    const target = url.trim();
    const headers = prefill?.headers ?? [];
    const valid = /^https?:\/\/\S+$/i.test(target);
    info = null;
    checkError = "";
    checking = valid;
    if (!valid) return;
    let stale = false;
    const timer = setTimeout(async () => {
      try {
        const result = await api.inspect(target, headers);
        if (!stale) info = result;
      } catch (err) {
        if (!stale) checkError = String(err);
      } finally {
        if (!stale) checking = false;
      }
    }, 400);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  });

  $effect(() => {
    const target = dir.trim();
    free = null;
    if (!target) return;
    let stale = false;
    const timer = setTimeout(async () => {
      const result = await api.freeSpace(target).catch(() => null);
      if (!stale) free = result;
    }, 300);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  });

  function readLastDir(): string {
    try {
      return localStorage.getItem(LAST_DIR_KEY) ?? "";
    } catch {
      return "";
    }
  }

  onMount(() => {
    dialog.showModal();
    if (!dir) api.defaultDir().then((d) => (dir ||= d ?? ""));
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
      await api.add({
        url: url.trim(),
        dir,
        fileName: fileName.trim() || null,
        connections: Math.min(32, Math.max(1, Math.round(connections))),
        queue,
        start,
        headers: prefill?.headers ?? [],
      });
      try {
        localStorage.setItem(LAST_DIR_KEY, dir);
      } catch {}
      onclose();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="add-title">
  <form onsubmit={submit}>
    <header>
      <h2 id="add-title">{prefill ? "Download from browser" : "Add download"}</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>

    <label>
      <span>Address</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        type="url"
        bind:value={url}
        placeholder="https://example.com/file.zip"
        required
        autofocus
        spellcheck="false"
      />
      <span class="link-info" aria-live="polite">
        {#if checking}
          <span class="muted">Checking link…</span>
        {:else if info}
          <span>{info.total === null ? "Size unknown" : bytes(info.total)}</span>
          {#if info.resumable}
            <span class="muted">· can be paused and resumed</span>
          {:else}
            <span class="warn">· can't resume: pausing starts over</span>
          {/if}
        {:else if checkError}
          <span class="warn">{checkError}</span>
        {/if}
      </span>
    </label>

    <label>
      <span>Save to</span>
      <div class="inline">
        <input bind:value={dir} required spellcheck="false" />
        <button type="button" onclick={browse}><Icon name="folder" />Browse</button>
      </div>
      {#if free !== null}
        <span class="link-info" class:warn={tooBig}>
          {tooBig ? `Not enough space: ${bytes(free)} free` : `${bytes(free)} free`}
        </span>
      {/if}
    </label>

    <div class="inline">
      <label class="grow">
        <span>File name</span>
        <input bind:value={fileName} placeholder={info?.fileName ?? "Use the name from the server"} spellcheck="false" />
      </label>
      <label>
        <span>Connections</span>
        <input type="number" bind:value={connections} min="1" max="32" class="narrow" />
      </label>
    </div>

    <div class="inline">
      <label class="grow">
        <span>Queue</span>
        <select bind:value={queue}>
          {#each downloads.queues as q (q.id)}
            <option value={q.id}>{q.name}</option>
          {/each}
        </select>
      </label>
      <label class="check grow">
        <input type="checkbox" bind:checked={start} />
        Start when the queue has room
      </label>
    </div>

    {#if error}
      <p class="error" role="alert"><Icon name="alert" size={16} />{error}</p>
    {/if}

    <footer>
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="submit" class="primary" disabled={busy}>
        {busy ? "Checking link…" : start ? "Download" : "Add"}
      </button>
    </footer>
  </form>
</dialog>
