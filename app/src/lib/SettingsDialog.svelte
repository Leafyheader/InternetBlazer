<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { api, type Category, type MediaTools, type Prefs, type Settings } from "./api";
  import { downloads } from "./downloads.svelte";
  import Icon from "./Icon.svelte";

  let { onclose }: { onclose: () => void } = $props();

  // Loaded when the dialog opens; the form stays empty until then.
  let prefs = $state<Prefs | null>(null);
  let settings = $state<Settings | null>(null);
  let launchAtLogin = $state(false);
  let initialLaunch = false;
  /** Where downloads go when no folder is set. */
  let systemDir = $state("");
  let busy = $state(false);
  let error = $state("");
  let dialog: HTMLDialogElement;
  /** Null while yt-dlp is asked for its version, which takes a moment. */
  let tools = $state<MediaTools | null>(null);
  let updating = $state(false);
  let updatingDeno = $state(false);
  let denoMessage = $state("");
  let toolsMessage = $state("");

  onMount(() => {
    dialog.showModal();
    Promise.all([api.prefs(), api.settings(), api.launchAtLogin().catch(() => false)])
      .then(([p, s, launch]) => {
        prefs = p;
        settings = s;
        launchAtLogin = initialLaunch = launch;
        return api.systemDownloadDir().then((d) => (systemDir = d ?? ""));
      })
      .catch((err) => (error = String(err)));
    api.mediaTools().then((t) => (tools = t));
  });

  async function updateYtdlp() {
    updating = true;
    toolsMessage = "";
    try {
      const before = tools?.ytdlp?.managed ? tools.ytdlp.version : null;
      const installed = await api.installYtdlp();
      toolsMessage =
        before === installed.version ? "Already the latest version." : `Installed yt-dlp ${installed.version}.`;
      tools = await api.mediaTools();
    } catch (err) {
      toolsMessage = String(err);
    } finally {
      updating = false;
    }
  }

  async function updateDeno() {
    updatingDeno = true;
    denoMessage = "";
    try {
      const before = tools?.deno?.managed ? tools.deno.version : null;
      const installed = await api.installDeno();
      denoMessage =
        before === installed.version ? "Already the latest version." : `Installed deno ${installed.version}.`;
      tools = await api.mediaTools();
    } catch (err) {
      denoMessage = String(err);
    } finally {
      updatingDeno = false;
    }
  }

  async function browse() {
    if (!prefs) return;
    const picked = await pickFolder({
      directory: true,
      defaultPath: prefs.downloadDir || systemDir || undefined,
    });
    if (typeof picked === "string") prefs.downloadDir = picked;
  }

  async function browseType(category: Category) {
    if (!prefs) return;
    const picked = await pickFolder({
      directory: true,
      defaultPath: prefs.categoryDirs[category] || typeDir(category) || undefined,
    });
    if (typeof picked === "string") prefs.categoryDirs[category] = picked;
  }

  /** The subfolder a file type uses when the user hasn't picked a folder for it. */
  function typeDir(category: Category): string {
    const base = prefs?.downloadDir?.trim() || systemDir;
    const name = downloads.categories.find((c) => c.id === category)?.name ?? "";
    if (!base) return "";
    const sep = base.includes("\\") ? "\\" : "/";
    return base.endsWith(sep) ? base + name : base + sep + name;
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!prefs || !settings) return;
    busy = true;
    error = "";
    try {
      const downloadDir = prefs.downloadDir?.trim() || null;
      // A cleared folder goes back to the subfolder.
      const categoryDirs = Object.fromEntries(
        Object.entries(prefs.categoryDirs)
          .map(([k, v]) => [k, v?.trim()])
          .filter(([, v]) => v),
      );
      await api.setPrefs({ ...prefs, downloadDir, categoryDirs });
      await api.setSettings({
        ...settings,
        connections: Math.min(32, Math.max(1, Math.round(settings.connections ?? 8))),
        autoRetries: Math.min(100, Math.max(0, Math.round(settings.autoRetries ?? 10))),
      });
      if (launchAtLogin !== initialLaunch) await api.setLaunchAtLogin(launchAtLogin);
      dialog.close();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }

  const openLogs = () => api.openLogFolder().catch((err) => (error = String(err)));
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="settings-title">
  <form onsubmit={submit}>
    <header>
      <h2 id="settings-title">Settings</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>

    {#if prefs && settings}
      <fieldset>
        <legend>Downloads</legend>
        <label>
          <span>Save downloads to</span>
          <div class="inline">
            <input bind:value={prefs.downloadDir} placeholder={systemDir} spellcheck="false" />
            <button type="button" onclick={browse}><Icon name="folder" />Browse</button>
          </div>
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={prefs.sortByType} />
          Sort downloads into folders by file type
        </label>
        {#if prefs.sortByType}
          {#each downloads.categories as c (c.id)}
            <div class="inline type-folder">
              <span class="type-name">{c.name}</span>
              <input
                bind:value={prefs.categoryDirs[c.id]}
                placeholder={typeDir(c.id)}
                aria-label="Folder for {c.name}"
                spellcheck="false"
              />
              <button type="button" class="icon" onclick={() => browseType(c.id)} aria-label="Browse for the {c.name} folder" title="Browse">
                <Icon name="folder" />
              </button>
            </div>
          {/each}
          <p class="hint muted">
            Each type goes to a subfolder of the folder above unless you pick another. The download
            dialog picks the folder from the file's name, and you can still change it there.
          </p>
        {:else}
          <label class="check">
            <input type="checkbox" bind:checked={prefs.rememberLastDir} />
            Start in the folder I used last
          </label>
        {/if}
        <div class="inline">
          <label class="grow">
            <span>Connections per download</span>
            <input type="number" bind:value={settings.connections} min="1" max="32" required class="narrow" />
          </label>
          <label class="grow">
            <span>Automatic retries</span>
            <input type="number" bind:value={settings.autoRetries} min="0" max="100" required class="narrow" />
          </label>
        </div>
        <p class="hint muted">
          A download that loses its connection is tried again this many times, waiting longer each time,
          before it counts as failed. 0 turns this off.
        </p>
      </fieldset>

      <fieldset>
        <legend>Video downloads</legend>
        {#if tools === null}
          <p class="hint muted">Checking yt-dlp…</p>
        {:else}
          <div class="inline">
            <span class="grow">
              {#if tools.ytdlp}
                yt-dlp {tools.ytdlp.version}
                {#if !tools.ytdlp.managed}
                  <span class="muted path" title={tools.ytdlp.path}>· from your system</span>
                {/if}
              {:else}
                yt-dlp is fetched the first time you download a video.
              {/if}
            </span>
            <button type="button" onclick={updateYtdlp} disabled={updating}>
              {updating ? "Getting yt-dlp…" : tools.ytdlp?.managed ? "Update yt-dlp" : "Install yt-dlp"}
            </button>
          </div>
          <p class="hint muted" aria-live="polite">
            {toolsMessage ||
              (tools.ytdlp && !tools.ytdlp.managed
                ? "Found on your system. Installing gives InternetBlazer its own copy, which it keeps up to date."
                : "Sites change often; when videos stop downloading, updating yt-dlp usually fixes it.")}
          </p>
          <div class="inline">
            <span class="grow">
              {#if tools.deno}
                deno {tools.deno.version}
                {#if !tools.deno.managed}
                  <span class="muted path" title={tools.deno.path}>· from your system</span>
                {/if}
              {:else}
                deno isn't installed.
              {/if}
            </span>
            <button type="button" onclick={updateDeno} disabled={updatingDeno}>
              {updatingDeno ? "Getting deno…" : tools.deno?.managed ? "Update deno" : "Install deno"}
            </button>
          </div>
          <p class="hint {tools.deno ? 'muted' : 'warn'}" aria-live="polite">
            {denoMessage ||
              (tools.deno
                ? "yt-dlp uses it to read YouTube, so every quality is offered."
                : "Without it, YouTube offers yt-dlp fewer qualities, and none at all when you're signed in. It's about 45 MB.")}
          </p>
          {#if tools.ffmpeg}
            <p class="hint muted">
              <span class="path" title={tools.ffmpeg}>ffmpeg is installed</span>, so every quality is offered.
            </p>
          {:else}
            <p class="hint warn">
              ffmpeg isn't installed. Without it, YouTube and many other sites offer only the sound or
              low qualities. On Windows, install it with: winget install ffmpeg
            </p>
          {/if}
        {/if}
      </fieldset>

      <fieldset>
        <legend>Browser</legend>
        <label class="check">
          <input type="checkbox" bind:checked={prefs.confirmBrowserDownloads} />
          Ask before downloading from the browser
        </label>
        <p class="hint muted">
          {prefs.confirmBrowserDownloads
            ? "Shows the download dialog for each file the browser hands over."
            : "Files from the browser start right away, in the folder above."}
        </p>
      </fieldset>

      <fieldset>
        <legend>Desktop</legend>
        <label class="check">
          <input type="checkbox" bind:checked={launchAtLogin} />
          Start InternetBlazer when I sign in
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={prefs.closeToTray} />
          Keep running in the tray when the window is closed
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={prefs.notify} />
          Notify me when downloads finish or fail
        </label>
      </fieldset>

      <fieldset>
        <legend>Updates</legend>
        <label class="check">
          <input type="checkbox" bind:checked={prefs.checkUpdates} />
          Check for updates when the app starts
        </label>
        <p class="hint muted">
          A newer version shows a notice with an install button. To check now, open About.
        </p>
      </fieldset>
    {:else if !error}
      <p class="muted">Loading…</p>
    {/if}

    {#if error}
      <p class="error" role="alert"><Icon name="alert" size={16} />{error}</p>
    {/if}

    <footer>
      <button type="button" class="push-left" onclick={openLogs}>Open log folder</button>
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="submit" class="primary" disabled={busy || !prefs}>Save</button>
    </footer>
  </form>
</dialog>
