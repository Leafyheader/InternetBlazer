<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { api, type Prefs, type Settings } from "./api";
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
  });

  async function browse() {
    if (!prefs) return;
    const picked = await pickFolder({
      directory: true,
      defaultPath: prefs.downloadDir || systemDir || undefined,
    });
    if (typeof picked === "string") prefs.downloadDir = picked;
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!prefs || !settings) return;
    busy = true;
    error = "";
    try {
      const downloadDir = prefs.downloadDir?.trim() || null;
      await api.setPrefs({ ...prefs, downloadDir });
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
          <input type="checkbox" bind:checked={prefs.rememberLastDir} />
          Start in the folder I used last
        </label>
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
