<script lang="ts">
  import { getTauriVersion, getVersion } from "@tauri-apps/api/app";
  import { onMount } from "svelte";
  import { api, type UpdateInfo } from "./api";
  import Icon from "./Icon.svelte";
  import logo from "./logo.png";

  /** A newer version the app already found when it started. */
  let { found = null, onclose }: { found?: UpdateInfo | null; onclose: () => void } = $props();

  let dialog: HTMLDialogElement;
  let version = $state("");
  let tauriVersion = $state("");
  let update = $state<UpdateInfo | null>(null);
  let checking = $state(false);
  let installing = $state(false);
  let message = $state("");
  let copied = $state(false);

  onMount(() => {
    dialog.showModal();
    update = found;
    getVersion().then((v) => (version = v));
    getTauriVersion().then((v) => (tauriVersion = v));
  });

  async function checkUpdate() {
    checking = true;
    message = "";
    try {
      update = await api.checkUpdate();
      if (!update) message = "You have the latest version.";
    } catch (err) {
      message = `Couldn't check for updates: ${err}`;
    } finally {
      checking = false;
    }
  }

  async function installUpdate() {
    installing = true;
    message = "Downloading the update. InternetBlazer restarts when it's installed.";
    try {
      await api.installUpdate();
    } catch (err) {
      message = `Couldn't install the update: ${err}`;
      installing = false;
    }
  }

  /** What a bug report needs to say about this copy. */
  async function copyVersion() {
    const text = `InternetBlazer ${version} (Tauri ${tauriVersion}; ${navigator.userAgent})`;
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch (err) {
      message = String(err);
    }
  }

  const openPage = (page: "home" | "releases" | "issues" | "license") =>
    api.openProjectPage(page).catch((err) => (message = String(err)));
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="about-title" class="about">
  <form method="dialog">
    <header>
      <h2 id="about-title">About</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>

    <div class="about-head">
      <img class="logo big" src={logo} alt="" />
      <div>
        <p class="product">InternetBlazer</p>
        <p class="muted">Version {version}</p>
        <p class="muted">A fast, free and open-source download manager.</p>
      </div>
    </div>

    <fieldset>
      <legend>Updates</legend>
      <div class="inline">
        <span class="grow">
          {update ? `Version ${update.version} is available.` : "Get the newest version from GitHub."}
        </span>
        {#if update}
          <button type="button" class="primary" onclick={installUpdate} disabled={installing}>
            {installing ? "Installing…" : `Install ${update.version}`}
          </button>
        {:else}
          <button type="button" onclick={checkUpdate} disabled={checking}>
            {checking ? "Checking…" : "Check for updates"}
          </button>
        {/if}
      </div>
      <p class="hint muted" aria-live="polite">
        {message ||
          (update
            ? "Running downloads pause and carry on after the restart."
            : "Updates are only installed when signed with the project's key.")}
      </p>
    </fieldset>

    <div class="links">
      <button type="button" onclick={() => openPage("home")}><Icon name="open" size={16} />Website</button>
      <button type="button" onclick={() => openPage("releases")}>What's new</button>
      <button type="button" onclick={() => openPage("issues")}>Report a problem</button>
      <button type="button" onclick={copyVersion}>
        {#if copied}<Icon name="check" size={16} />Copied{:else}Copy version info{/if}
      </button>
    </div>

    <p class="hint muted">
      Video downloads use yt-dlp, deno and ffmpeg, each under its own license. InternetBlazer is free software under the
      <button type="button" class="link" onclick={() => openPage("license")}>MIT or Apache-2.0 license</button>.
    </p>
    <p class="copyright">© 2026 Leafyhead Technologies</p>

    <footer>
      <button type="button" class="primary" onclick={() => dialog.close()}>Close</button>
    </footer>
  </form>
</dialog>
