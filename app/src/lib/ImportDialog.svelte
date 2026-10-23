<script module lang="ts">
  /** The http and https links in some text, each once, found as `blazer get -i` finds them. */
  export function findLinks(text: string): string[] {
    const found = text.match(/https?:\/\/[^\s"'<>]+/gi) ?? [];
    const links = found.map((link) => link.replace(/[.,;)\]]+$/, "")).filter((link) => link.length > 8);
    return [...new Set(links)];
  }
</script>

<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { api, type ImportFailure, MAIN_QUEUE } from "./api";
  import { downloads } from "./downloads.svelte";
  import Icon from "./Icon.svelte";

  let {
    onclose,
    queue: initialQueue = MAIN_QUEUE,
    text: initialText = "",
  }: {
    onclose: () => void;
    queue?: number;
    /** Text pasted into the window, to look for links in. */
    text?: string;
  } = $props();

  /** Bigger files are not lists of links. */
  const MAX_FILE = 10 * 1024 * 1024;

  // svelte-ignore state_referenced_locally
  let text = $state(initialText);
  // svelte-ignore state_referenced_locally
  let queue = $state(initialQueue);
  let start = $state(true);
  /** Each file goes to the folder for its type, rather than all to `dir`. */
  let byType = $state(true);
  let dir = $state("");
  let busy = $state(false);
  let error = $state("");
  /** Links the server turned away, once the others are added. */
  let failures = $state<ImportFailure[]>([]);
  let added = $state(0);
  /** Boxes the user ticked or cleared; the others are ticked unless the link is in the list. */
  let picked = $state<Record<string, boolean>>({});
  let dialog: HTMLDialogElement;
  let fileInput = $state<HTMLInputElement>();

  let links = $derived(findLinks(text));
  let known = $derived(new Set(Object.values(downloads.items).map((i) => i.url)));
  const isPicked = (link: string) => picked[link] ?? !known.has(link);
  let chosen = $derived(links.filter(isPicked));

  onMount(() => {
    dialog.showModal();
    api.prefs().then(async (p) => {
      byType = p.sortByType;
      const one = p.sortByType ? (p.downloadDir ?? (await api.systemDownloadDir())) : await api.defaultDir("", false);
      if (!dir) dir = one ?? "";
    });
  });

  /** The last part of a link's path, which is often the file's name, or else its site. */
  function nameOf(link: string): string {
    try {
      const { pathname, host } = new URL(link);
      return decodeURIComponent(pathname.slice(pathname.lastIndexOf("/") + 1)) || host;
    } catch {
      return link;
    }
  }

  function toggleAll() {
    const on = chosen.length < links.length;
    picked = Object.fromEntries(links.map((link) => [link, on]));
  }

  async function readFile(e: Event & { currentTarget: HTMLInputElement }) {
    const input = e.currentTarget;
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    if (file.size > MAX_FILE) {
      error = `${file.name} is too big to be a list of links.`;
      return;
    }
    error = "";
    text = await file.text();
  }

  async function browse() {
    const folder = await pickFolder({ directory: true, defaultPath: dir || undefined });
    if (typeof folder === "string") {
      dir = folder;
      byType = false;
    }
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = "";
    try {
      const urls = chosen;
      const failed = await api.importLinks({ urls, dir: byType ? null : dir.trim(), queue, start });
      if (failed.length === 0) return onclose();
      added = urls.length - failed.length;
      failures = failed;
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }

  /** Back to the list, holding only the links that failed. */
  function retry() {
    text = failures.map((f) => f.url).join("\n");
    picked = Object.fromEntries(failures.map((f) => [f.url, true]));
    failures = [];
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="import-title" class="import">
  {#if failures.length > 0}
    <form method="dialog">
      <header>
        <h2 id="import-title">Some links weren't added</h2>
      </header>
      <p>
        Added {added} {added === 1 ? "link" : "links"}. These {failures.length} couldn't be added:
      </p>
      <ul class="link-list">
        {#each failures as f (f.url)}
          <li>
            <span class="link-text">
              <span class="link-name">{f.url}</span>
              <span class="link-url warn-text">{f.error}</span>
            </span>
          </li>
        {/each}
      </ul>
      <footer>
        <button type="button" onclick={retry}>Try these again</button>
        <!-- svelte-ignore a11y_autofocus -->
        <button type="submit" class="primary" autofocus>Close</button>
      </footer>
    </form>
  {:else}
    <form onsubmit={submit}>
      <header>
        <h2 id="import-title">Import links</h2>
        <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
          <Icon name="close" />
        </button>
      </header>

      <label>
        <span>Paste links, or any text with links in it</span>
        <!-- svelte-ignore a11y_autofocus -->
        <textarea
          bind:value={text}
          rows="4"
          placeholder={"https://example.com/one.zip\nhttps://example.com/two.zip"}
          spellcheck="false"
          autofocus={!initialText}
        ></textarea>
      </label>
      <div class="inline">
        <button type="button" onclick={() => fileInput?.click()}><Icon name="document" />Open a text file…</button>
        <input type="file" accept=".txt,text/plain" hidden bind:this={fileInput} onchange={readFile} />
      </div>

      {#if links.length > 0}
        <div class="links">
          <label class="check">
            <input
              type="checkbox"
              checked={chosen.length === links.length}
              indeterminate={chosen.length > 0 && chosen.length < links.length}
              onchange={toggleAll}
            />
            {chosen.length} of {links.length} {links.length === 1 ? "link" : "links"} selected
          </label>
          <ul class="link-list">
            {#each links as link (link)}
              <li>
                <label class="check">
                  <input
                    type="checkbox"
                    checked={isPicked(link)}
                    onchange={(e) => (picked[link] = e.currentTarget.checked)}
                  />
                  <span class="link-text" title={link}>
                    <span class="link-name">{nameOf(link)}</span>
                    <span class="link-url">{link}</span>
                  </span>
                  {#if known.has(link)}<span class="tag">In your list</span>{/if}
                </label>
              </li>
            {/each}
          </ul>
        </div>
      {:else if text.trim()}
        <p class="muted">No links found. Links start with http:// or https://.</p>
      {/if}

      <fieldset>
        <legend>Save to</legend>
        <label class="check">
          <input type="radio" bind:group={byType} value={true} />
          The folder for each file's type
        </label>
        <div class="inline type-folder">
          <label class="check">
            <input type="radio" bind:group={byType} value={false} />
            One folder
          </label>
          <input bind:value={dir} oninput={() => (byType = false)} required={!byType} spellcheck="false" aria-label="Folder" />
          <button type="button" onclick={browse}><Icon name="folder" />Browse</button>
        </div>
      </fieldset>

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
        <button type="submit" class="primary" disabled={busy || chosen.length === 0}>
          {#if busy}
            Adding {chosen.length} {chosen.length === 1 ? "link" : "links"}…
          {:else}
            {start ? "Download" : "Add"} {chosen.length || ""} {chosen.length === 1 ? "link" : "links"}
          {/if}
        </button>
      </footer>
    </form>
  {/if}
</dialog>
