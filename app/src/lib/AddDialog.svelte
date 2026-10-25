<script module lang="ts">
  /** Set once yt-dlp is known to be there, so later dialogs skip asking. */
  let ytdlpReady = false;
</script>

<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { api, type Incoming, type LinkInfo, MAIN_QUEUE, type MediaInfo } from "./api";
  import { downloads } from "./downloads.svelte";
  import { bytes, parseSha256 } from "./format";
  import Icon from "./Icon.svelte";

  let {
    onclose,
    queue: initialQueue = MAIN_QUEUE,
    url: initialUrl = "",
    prefill,
  }: {
    onclose: () => void;
    queue?: number;
    /** A link pasted into the window. */
    url?: string;
    /** A download from the browser. */
    prefill?: Incoming;
  } = $props();

  // svelte-ignore state_referenced_locally
  let url = $state(prefill?.url ?? initialUrl);
  let dir = $state("");
  /** Set once the user picks a folder, which then no longer follows the file's type. */
  let touchedDir = false;
  // A video's name follows the quality picked here, which may change its extension.
  // svelte-ignore state_referenced_locally
  let fileName = $state((prefill?.media ? null : prefill?.fileName) ?? "");
  let connections = $state(8);
  let touchedConnections = false;
  // svelte-ignore state_referenced_locally
  let queue = $state(initialQueue);
  let start = $state(true);
  /** Text holding the SHA-256 to check the finished file against. */
  let sha256 = $state("");
  let badSha256 = $derived(sha256.trim() !== "" && parseSha256(sha256) === null);
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;

  /** What the link points to, checked while the user fills in the rest. */
  let info = $state<LinkInfo | null>(null);
  let checking = $state(false);
  let checkError = $state("");
  /** Videos found on the page the link leads to. */
  let media = $state<MediaInfo | null>(null);
  let mediaError = $state("");
  /** Index into `media.choices`. */
  let choice = $state(0);
  /** What the check is doing, when it takes a while. */
  let stage = $state("");
  let chosen = $derived(media?.choices[choice] ?? null);
  /** A title is no path, though it may hold slashes, as in "AC/DC". */
  // The browser names a video after its page, which for a stream beats yt-dlp's "index".
  let videoStem = $derived(
    prefill?.media && prefill.fileName && url.trim() === prefill.url
      ? prefill.fileName.replace(/\.[^.]*$/, "")
      : (media?.title.replace(/[\\/]/g, "_") ?? ""),
  );
  let videoName = $derived(media && chosen ? `${videoStem}.${chosen.ext}` : null);
  let size = $derived(media ? (chosen?.size ?? null) : (info?.total ?? null));
  /** The best guess so far at the file's name, which decides its folder. */
  let likelyName = $derived(
    fileName.trim() || videoName || info?.fileName || prefill?.fileName || nameInUrl(url) || "",
  );
  /** Bytes free where the file goes, or null when unknown. */
  let free = $state<number | null>(null);
  let tooBig = $derived(size !== null && free !== null && size > free);

  /** Finds the videos on a page, getting yt-dlp first if it isn't there yet. A playlist is read
   * without yt-dlp, which is only fetched if the engine can't read it after all. */
  async function findMedia(
    target: string,
    headers: [string, string][],
    playlist: boolean,
    report: (s: string) => void,
  ) {
    if (playlist && !ytdlpReady) {
      report("Reading the video's playlist…");
      try {
        return await api.inspectMedia(target, headers);
      } catch (err) {
        if (!String(err).includes("yt-dlp, which is not installed")) throw err;
      }
    }
    if (!ytdlpReady) {
      if (!(await api.mediaTools()).ytdlp) {
        report("Getting yt-dlp, which downloads videos…");
        await api.installYtdlp();
      }
      ytdlpReady = true;
    }
    report("Looking for videos on the page…");
    return api.inspectMedia(target, headers);
  }

  // Checks the link once typing pauses. An answer for an address that has changed since is
  // dropped. Only `url` and `prefill` may be read here: reading state this effect sets, such
  // as `checking`, would rerun it whenever a check finishes, and so check forever.
  $effect(() => {
    const target = url.trim();
    const headers = prefill?.headers ?? [];
    const valid = /^https?:\/\/\S+$/i.test(target);
    info = null;
    checkError = "";
    media = null;
    mediaError = "";
    stage = "";
    checking = valid;
    if (!valid) return;
    let stale = false;
    const timer = setTimeout(async () => {
      try {
        let page: boolean;
        try {
          const result = await api.inspect(target, headers);
          if (stale) return;
          info = result;
          page = result.page;
        } catch (err) {
          if (stale) return;
          checkError = String(err);
          // Sites that turn away plain requests, as with a bot check, may still let yt-dlp in.
          page = true;
        }
        // A video's HLS or DASH playlist is a small file of its own, but it stands for the video.
        const playlist = /\.(m3u8|mpd)([?#]|$)/i.test(target) || (prefill?.media && target === prefill.url);
        if (!page && !playlist) return;
        const found = await findMedia(target, headers, !!playlist, (s) => !stale && (stage = s));
        if (stale) return;
        if (found.choices.length > 0) {
          media = found;
          // The quality picked on the page, if the link is still that page.
          const picked = target === prefill?.url ? JSON.stringify(prefill.media) : null;
          choice = Math.max(0, found.choices.findIndex((c) => JSON.stringify(c.format) === picked));
          checkError = "";
        } else {
          mediaError = found.ffmpeg
            ? "yt-dlp found the video but no way to download it."
            : "This video comes as separate picture and sound, which need ffmpeg to be joined. Install ffmpeg (on Windows: winget install ffmpeg), then check the link again.";
        }
      } catch (err) {
        if (!stale) mediaError = String(err);
      } finally {
        if (!stale) {
          checking = false;
          stage = "";
        }
      }
    }, 400);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  });

  // The folder for the file's type, which may change as more is known about the file.
  $effect(() => {
    const name = likelyName;
    const video = media !== null || !!prefill?.media;
    let stale = false;
    api.defaultDir(name, video).then((d) => {
      if (!stale && !touchedDir) dir = d ?? "";
    });
    return () => {
      stale = true;
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

  onMount(() => {
    dialog.showModal();
    api.settings().then((s) => {
      if (!touchedConnections && s.connections !== null) connections = s.connections;
    });
  });

  async function browse() {
    const picked = await pickFolder({ directory: true, defaultPath: dir || undefined });
    if (typeof picked === "string") {
      dir = picked;
      touchedDir = true;
    }
  }

  /** The last part of a link's path, which is often the file's name. */
  function nameInUrl(link: string): string {
    try {
      const { pathname } = new URL(link.trim());
      return decodeURIComponent(pathname.slice(pathname.lastIndexOf("/") + 1));
    } catch {
      return "";
    }
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = "";
    try {
      await api.add({
        url: url.trim(),
        dir,
        // Named here for a video, so adding it doesn't ask yt-dlp about the page again.
        fileName: fileName.trim() || videoName,
        connections: Math.min(32, Math.max(1, Math.round(connections))),
        queue,
        start,
        headers: prefill?.headers ?? [],
        media: chosen?.format ?? null,
        sha256: parseSha256(sha256),
      });
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
          <span class="muted">{stage || "Checking link…"}</span>
        {:else if media}
          <span>{media.site ? `${media.site} video` : "Video"}: {media.title}</span>
        {:else if mediaError}
          <span class="warn">
            {mediaError.startsWith("Unsupported URL")
              ? "This is a web page, not a file, and yt-dlp found no video on it."
              : mediaError}
          </span>
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

    {#if media}
      <label>
        <span>Quality</span>
        <select bind:value={choice}>
          {#each media.choices as c, i (i)}
            <option value={i}>
              {c.label} · {c.ext.toUpperCase()}{c.size === null ? "" : ` · about ${bytes(c.size)}`}
            </option>
          {/each}
        </select>
        {#if !media.ffmpeg}
          <span class="link-info warn">
            ffmpeg isn't installed, so only qualities that come in one file are offered. Install
            ffmpeg (on Windows: winget install ffmpeg) for the rest.
          </span>
        {/if}
      </label>
    {/if}

    <label>
      <span>Save to</span>
      <div class="inline">
        <input bind:value={dir} oninput={() => (touchedDir = true)} required spellcheck="false" />
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
        <input bind:value={fileName} placeholder={videoName ?? info?.fileName ?? "Use the name from the server"} spellcheck="false" />
      </label>
      <label>
        <span>Connections</span>
        <input
          type="number"
          bind:value={connections}
          oninput={() => (touchedConnections = true)}
          min="1"
          max="32"
          class="narrow"
        />
      </label>
    </div>

    <label>
      <span>SHA-256 <span class="muted">(optional)</span></span>
      <input
        bind:value={sha256}
        placeholder="Paste the hash the site gives, to check the file once it's downloaded"
        spellcheck="false"
        aria-invalid={badSha256}
      />
      {#if badSha256}
        <span class="link-info warn">A SHA-256 is 64 characters of 0-9 and a-f.</span>
      {/if}
    </label>

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
      <button type="submit" class="primary" disabled={busy || badSha256}>
        {busy ? "Checking link…" : start ? "Download" : "Add"}
      </button>
    </footer>
  </form>
</dialog>
