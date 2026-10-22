<script lang="ts">
  import { onMount } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import AddDialog from "./lib/AddDialog.svelte";
  import ContextMenu, { type MenuItem } from "./lib/ContextMenu.svelte";
  import { api, type Incoming, MAIN_QUEUE, onBrowserDownload, type Queue } from "./lib/api";
  import Details from "./lib/Details.svelte";
  import FileIcon from "./lib/FileIcon.svelte";
  import {
    downloads,
    eta,
    FILTERS,
    inView,
    percent,
    retryIn,
    STATUS_LABELS,
    type Item,
    type View,
  } from "./lib/downloads.svelte";
  import { bytes, duration } from "./lib/format";
  import Icon from "./lib/Icon.svelte";
  import MoveDialog from "./lib/MoveDialog.svelte";
  import PropertiesDialog from "./lib/PropertiesDialog.svelte";
  import QueueDialog from "./lib/QueueDialog.svelte";
  import RemoveDialog from "./lib/RemoveDialog.svelte";
  import SettingsDialog from "./lib/SettingsDialog.svelte";
  import SpeedLimitDialog from "./lib/SpeedLimitDialog.svelte";
  import UrlDialog from "./lib/UrlDialog.svelte";
  import { clock } from "./lib/clock.svelte";

  let view = $state<View>({ filter: "all" });
  const selected = new SvelteSet<number>();
  /** Where a Shift+click range starts. */
  let anchor: number | null = null;
  let adding = $state(false);
  /** Downloads from the browser, confirmed one at a time in the add dialog. */
  let incoming = $state<Incoming[]>([]);
  let removing = $state<Item[] | null>(null);
  /** The queue being edited, with `queue: null` for a new one. */
  let queueDialog = $state<{ queue: Queue | null } | null>(null);
  /** Whose speed limit is being changed. */
  let limitDialog = $state<"global" | Item | null>(null);
  /** The download whose address is being changed. */
  let urlDialog = $state<Item | null>(null);
  let settingsOpen = $state(false);
  /** The download being moved or renamed. */
  let moveDialog = $state<Item | null>(null);
  /** The download whose properties are shown; looked up live, so they stay current. */
  let propertiesId = $state<number | null>(null);
  let propertiesItem = $derived(propertiesId === null ? undefined : downloads.items[propertiesId]);
  let menu = $state<{ x: number; y: number; items: MenuItem[] } | null>(null);
  let toast = $state("");
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  let visible = $derived(downloads.sorted.filter((item) => inView(view, item)));
  let currentQueue = $derived.by(() => {
    const v = view;
    return "queue" in v ? downloads.queues.find((q) => q.id === v.queue) : undefined;
  });
  let selection = $derived(visible.filter((item) => selected.has(item.id)));
  let single = $derived(selection.length === 1 ? selection[0] : null);
  let activeCount = $derived(
    downloads.sorted.filter((i) => ["downloading", "queued", "retrying"].includes(i.status)).length,
  );

  // A download waiting to retry can be started right away, or paused to stop retrying.
  const canResume = (i: Item) => ["paused", "failed", "retrying"].includes(i.status);
  const canPause = (i: Item) => ["downloading", "queued", "retrying"].includes(i.status);

  // A removed queue cannot stay in view.
  $effect(() => {
    if ("queue" in view && downloads.queues.length > 0 && !currentQueue) view = { filter: "all" };
  });

  onMount(() => {
    const disconnect = downloads.connect((id) => selected.delete(id));
    disconnect.catch(showError);
    const takeIncoming = () =>
      api
        .takeBrowserDownloads()
        .then((list) => incoming.push(...list))
        .catch(showError);
    const unlistenBrowser = onBrowserDownload(takeIncoming);
    // Also takes those that arrived before the listener, such as the one that started the app.
    unlistenBrowser.then(takeIncoming);
    return () => {
      disconnect.then((f) => f());
      unlistenBrowser.then((f) => f());
    };
  });

  function showError(err: unknown) {
    toast = String(err);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = ""), 6000);
  }

  async function each(items: Item[], action: (id: number) => Promise<void>) {
    const results = await Promise.allSettled(items.map((item) => action(item.id)));
    const failed = results.find((r) => r.status === "rejected");
    if (failed) showError(failed.reason);
  }

  const resume = () => each(selection.filter(canResume), api.start);
  const pause = () => each(selection.filter(canPause), api.pause);
  const restart = () => each(selection, api.restart);
  const openFile = (item: Item) => api.openFile(item.id).catch(showError);
  const showInFolder = (item: Item) => api.showInFolder(item.id).catch(showError);
  const moveTo = (queue: number) => each(selection, (id) => api.setQueue(id, queue));
  const openWith = (item: Item) => api.openWith(item.id).catch(showError);
  /** Only Windows has a system dialog to pick the program. */
  const canOpenWith = navigator.userAgent.includes("Windows");

  /** The menu for the selected downloads. */
  function downloadMenu(): MenuItem[] {
    const items = selection;
    const one = items.length === 1 ? items[0] : null;
    const finished = items.filter((i) => i.status === "completed");
    const resumable = items.filter(canResume);
    const pausable = items.filter(canPause);
    const queues = new Set(items.map((i) => i.queue));
    const menu: MenuItem[] = [];
    if (one?.status === "completed") {
      menu.push({ label: "Open", icon: "open", hint: "Enter", action: () => openFile(one) });
      if (canOpenWith) menu.push({ label: "Open with…", action: () => openWith(one) });
    }
    if (one) menu.push({ label: "Show in folder", icon: "folder", action: () => showInFolder(one) });
    if (menu.length > 0) menu.push("separator");
    if (resumable.length > 0) {
      menu.push({ label: "Resume", icon: "play", action: () => each(resumable, api.start) });
    }
    if (pausable.length > 0) {
      menu.push({ label: "Pause", icon: "pause", action: () => each(pausable, api.pause) });
    }
    if (finished.length > 0) {
      menu.push({
        label: "Download again",
        icon: "restart",
        action: () => each(finished, api.restart),
      });
    }
    if (one) {
      const running = canPause(one);
      menu.push({
        label: running ? "Move / rename… (pause first)" : "Move / rename…",
        disabled: running,
        action: () => (moveDialog = one),
      });
    }
    menu.push({
      label: "Add to queue",
      icon: "list",
      items: downloads.queues.map((q) => ({
        label: q.name,
        checked: queues.size === 1 && queues.has(q.id),
        action: () => moveTo(q.id),
      })),
    });
    menu.push("separator", {
      label: items.length > 1 ? `Remove ${items.length} downloads…` : "Remove…",
      icon: "trash",
      hint: "Del",
      danger: true,
      action: () => (removing = items),
    });
    if (one) {
      menu.push("separator", {
        label: "Properties",
        icon: "sliders",
        action: () => (propertiesId = one.id),
      });
    }
    return menu;
  }

  function openMenu(x: number, y: number, items: MenuItem[]) {
    menu = null;
    // A new menu mounts fresh, so it measures and places itself again.
    queueMicrotask(() => (menu = { x, y, items }));
  }

  function rowMenu(e: MouseEvent, item: Item) {
    e.preventDefault();
    if (!selected.has(item.id)) {
      selected.clear();
      selected.add(item.id);
      anchor = item.id;
    }
    openMenu(e.clientX, e.clientY, downloadMenu());
  }

  function listMenu(e: MouseEvent) {
    if (e.defaultPrevented) return;
    e.preventDefault();
    selected.clear();
    openMenu(e.clientX, e.clientY, [
      { label: "Add download…", icon: "plus", hint: "Ctrl+N", action: () => (adding = true) },
      {
        label: "Select all",
        hint: "Ctrl+A",
        disabled: visible.length === 0,
        action: () => visible.forEach((i) => selected.add(i.id)),
      },
    ]);
  }

  /**
   * Replaces the browser's own menu, with its Reload and Inspect. Text fields keep it, for
   * cut, copy and paste; selected text gets a Copy menu.
   */
  function windowMenu(e: MouseEvent) {
    if (e.defaultPrevented) return;
    const target = e.target as HTMLElement;
    if (target.closest("input:not([type=checkbox]), textarea")) return;
    e.preventDefault();
    const text = getSelection()?.toString() ?? "";
    if (text) {
      openMenu(e.clientX, e.clientY, [
        {
          label: "Copy",
          hint: "Ctrl+C",
          action: () => navigator.clipboard.writeText(text).catch(showError),
        },
      ]);
    } else {
      menu = null;
    }
  }
  const startQueue = (q: Queue) => api.startQueue(q.id).catch(showError);
  const stopQueue = (q: Queue) => api.stopQueue(q.id).catch(showError);

  function saveLimit(limit: number | null) {
    const target = limitDialog;
    if (target === "global") return api.setSpeedLimit(limit);
    if (target) return api.setDownloadSpeedLimit(target.id, limit);
    return Promise.resolve();
  }

  const inQueue = (q: Queue, test: (i: Item) => boolean) =>
    downloads.sorted.some((i) => i.queue === q.id && test(i));
  const queueRunning = (q: Queue) => inQueue(q, canPause);

  function select(e: MouseEvent, item: Item) {
    if (e.shiftKey && anchor !== null) {
      const from = visible.findIndex((i) => i.id === anchor);
      const to = visible.findIndex((i) => i.id === item.id);
      if (from !== -1) {
        if (!e.ctrlKey && !e.metaKey) selected.clear();
        const [lo, hi] = from < to ? [from, to] : [to, from];
        visible.slice(lo, hi + 1).forEach((i) => selected.add(i.id));
        return;
      }
    }
    if (e.ctrlKey || e.metaKey) {
      if (selected.has(item.id)) selected.delete(item.id);
      else selected.add(item.id);
    } else {
      selected.clear();
      selected.add(item.id);
    }
    anchor = item.id;
  }

  function activate(item: Item) {
    if (item.status === "completed") openFile(item);
    else if (canResume(item)) api.start(item.id).catch(showError);
  }

  /** Browser shortcuts that make no sense in an app, such as reloading the page. */
  function isBrowserKey(e: KeyboardEvent) {
    const ctrl = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    return e.key === "F5" || (ctrl && ["r", "p", "f", "g", "u", "j"].includes(key));
  }

  function onkeydown(e: KeyboardEvent) {
    // Development builds keep them, to reload and inspect the UI.
    if (import.meta.env.PROD && isBrowserKey(e)) {
      e.preventDefault();
      return;
    }
    if (menu) {
      if (e.key === "Escape") menu = null;
      return;
    }
    if (
      adding ||
      incoming.length > 0 ||
      removing ||
      queueDialog ||
      limitDialog ||
      urlDialog ||
      settingsOpen ||
      moveDialog ||
      propertiesItem
    ) {
      return;
    }
    const target = e.target as HTMLElement;
    if (target.closest("input, textarea, select, dialog")) return;
    const ctrl = e.ctrlKey || e.metaKey;
    if (ctrl && e.key.toLowerCase() === "n") {
      e.preventDefault();
      adding = true;
    } else if (ctrl && e.key.toLowerCase() === "a") {
      e.preventDefault();
      visible.forEach((i) => selected.add(i.id));
    } else if (e.key === "Delete" && selection.length > 0) {
      removing = selection;
    } else if (e.key === "Enter" && single) {
      activate(single);
    } else if (e.key === "Escape") {
      selected.clear();
    } else if ((e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) && selection.length > 0) {
      e.preventDefault();
      const row = document.querySelector(".row.selected")?.getBoundingClientRect();
      if (row) openMenu(row.left + 40, row.bottom, downloadMenu());
    }
  }

  function count(v: View) {
    return downloads.sorted.filter((i) => inView(v, i)).length;
  }

  function isCurrent(v: View) {
    return "queue" in v
      ? "queue" in view && view.queue === v.queue
      : "filter" in view && view.filter === v.filter;
  }
</script>

<svelte:window {onkeydown} oncontextmenu={windowMenu} />

<div class="shell">
  <aside class="sidebar">
    <div class="brand">
      <span class="logo"><Icon name="download" size={16} /></span>
      InternetBlazer
    </div>
    <button class="primary add" onclick={() => (adding = true)} title="Add download (Ctrl+N)">
      <Icon name="plus" />Add download
    </button>
    <nav aria-label="Filters">
      {#each FILTERS as f (f.id)}
        {@const v = { filter: f.id }}
        <button class="filter" class:current={isCurrent(v)} onclick={() => (view = v)}>
          <span>{f.label}</span>
          <span class="count">{count(v)}</span>
        </button>
      {/each}
    </nav>
    <nav aria-label="Queues">
      <div class="section">
        <span>Queues</span>
        <button
          class="icon"
          onclick={() => (queueDialog = { queue: null })}
          aria-label="New queue"
          title="New queue"
        >
          <Icon name="plus" size={16} />
        </button>
      </div>
      {#each downloads.queues as q (q.id)}
        {@const v = { queue: q.id }}
        {@const running = queueRunning(q)}
        <button class="filter" class:current={isCurrent(v)} onclick={() => (view = v)}>
          <span class="queue-name">
            <span class="dot" class:running title={running ? "Running" : "Stopped"}></span>
            <span class="text">{q.name}</span>
            {#if q.schedule.enabled}
              <span class="muted" title="Scheduled"><Icon name="clock" size={14} /></span>
            {/if}
          </span>
          <span class="count">{count(v)}</span>
        </button>
      {/each}
    </nav>
    <button class="filter settings" onclick={() => (settingsOpen = true)}>
      <span class="queue-name"><Icon name="settings" size={16} />Settings</span>
    </button>
  </aside>

  <main>
    <div class="toolbar" role="toolbar" aria-label="Actions">
      <button onclick={resume} disabled={!selection.some(canResume)} title="Resume (Enter)">
        <Icon name="play" />Resume
      </button>
      <button onclick={pause} disabled={!selection.some(canPause)}>
        <Icon name="pause" />Pause
      </button>
      <button onclick={restart} disabled={selection.length === 0} title="Download again from the start">
        <Icon name="restart" />Restart
      </button>
      <button
        onclick={() => (removing = selection)}
        disabled={selection.length === 0}
        title="Remove (Delete)"
      >
        <Icon name="trash" />Remove
      </button>
      <span class="divider"></span>
      <button onclick={() => single && openFile(single)} disabled={single?.status !== "completed"}>
        <Icon name="open" />Open
      </button>
      <button onclick={() => single && showInFolder(single)} disabled={!single}>
        <Icon name="folder" />Show in folder
      </button>
      <select
        class="move"
        value=""
        disabled={selection.length === 0}
        aria-label="Move to queue"
        onchange={(e) => {
          moveTo(Number(e.currentTarget.value));
          e.currentTarget.value = "";
        }}
      >
        <option value="" disabled>Move to queue…</option>
        {#each downloads.queues as q (q.id)}
          <option value={String(q.id)}>{q.name}</option>
        {/each}
      </select>
    </div>

    {#if currentQueue}
      {@const q = currentQueue}
      <div class="queuebar">
        <strong>{q.name}</strong>
        <span class="muted">
          {q.maxActive} at a time{#if q.schedule.enabled}&nbsp;· on a schedule{/if}
        </span>
        <span class="spacer"></span>
        <button onclick={() => startQueue(q)} disabled={!inQueue(q, canResume)}>
          <Icon name="play" />Start queue
        </button>
        <button onclick={() => stopQueue(q)} disabled={!queueRunning(q)}>
          <Icon name="stop" />Stop queue
        </button>
        <button onclick={() => (queueDialog = { queue: q })}>
          <Icon name="sliders" />Settings
        </button>
      </div>
    {/if}

    <div
      class="list"
      role="listbox"
      tabindex="-1"
      aria-multiselectable="true"
      aria-label="Downloads"
      oncontextmenu={listMenu}
    >
      <div class="head" aria-hidden="true">
        <span>Name</span>
        <span>Size</span>
        <span>Progress</span>
        <span class="num">Speed</span>
        <span class="num">Time left</span>
      </div>
      {#each visible as item (item.id)}
        {@const pct = percent(item)}
        {@const left = eta(item)}
        <!-- Keyboard selection is handled by the window shortcuts. -->
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <div
          class="row"
          class:selected={selected.has(item.id)}
          role="option"
          tabindex="-1"
          aria-selected={selected.has(item.id)}
          onclick={(e) => select(e, item)}
          ondblclick={() => activate(item)}
          oncontextmenu={(e) => rowMenu(e, item)}
        >
          <span class="name">
            <FileIcon {item} />
            <span class="text" title={item.fileName}>{item.fileName}</span>
            {#if !item.resumable && item.status !== "completed"}
              <span class="no-resume" title="The server doesn't support resuming. Pausing restarts this download from the beginning.">
                <Icon name="alert" size={14} />
              </span>
            {/if}
          </span>
          <span class="muted">{item.total === null ? "—" : bytes(item.total)}</span>
          <span
            class="progress {item.status}"
            class:stalled={item.status === "downloading" && item.stalled}
            title={item.status === "downloading" ? (item.problem ?? undefined) : undefined}
          >
            <span class="bar" class:indeterminate={pct === null && item.status === "downloading"}>
              <span class="fill" style="width: {pct ?? 0}%"></span>
            </span>
            <span class="label">
              {#if item.status === "downloading" && item.problem}
                Reconnecting…
              {:else if item.status === "downloading" && item.stalled}
                No data{pct === null ? "" : ` · ${pct.toFixed(1)}%`}
              {:else if item.status === "downloading" && pct !== null}
                {pct.toFixed(1)}%
              {:else if item.status === "downloading"}
                {bytes(item.downloaded)}
              {:else if item.status === "retrying"}
                {@const wait = retryIn(item, clock.now)}
                {wait ? `Retrying in ${duration(wait)}` : "Retrying…"}
              {:else if (item.status === "paused" || item.status === "queued") && pct}
                {STATUS_LABELS[item.status]} · {pct.toFixed(0)}%
              {:else}
                {STATUS_LABELS[item.status]}
              {/if}
            </span>
          </span>
          <span class="num muted">{item.speed > 0 ? `${bytes(item.speed)}/s` : ""}</span>
          <span class="num muted">{left === null ? "" : duration(left)}</span>
        </div>
      {:else}
        <div class="empty">
          {#if downloads.sorted.length === 0}
            <Icon name="download" size={40} />
            <p>No downloads yet.</p>
            <button class="primary" onclick={() => (adding = true)}><Icon name="plus" />Add download</button>
          {:else}
            <p>Nothing here.</p>
          {/if}
        </div>
      {/each}
    </div>

    {#if single}
      {@const item = single}
      <Details {item} onlimit={() => (limitDialog = item)} onaddress={() => (urlDialog = item)} />
    {/if}

    <footer class="statusbar">
      <span>{downloads.sorted.length} downloads</span>
      <span>{activeCount} active</span>
      <button
        class="limit"
        class:on={downloads.speedLimit !== null}
        onclick={() => (limitDialog = "global")}
        title="Speed limit for all downloads"
      >
        <Icon name="gauge" size={14} />
        {downloads.speedLimit === null ? "No speed limit" : `Limit ${bytes(downloads.speedLimit)}/s`}
      </button>
      <span class="speed">↓ {bytes(downloads.totalSpeed)}/s</span>
    </footer>
  </main>
</div>

{#if adding}
  <AddDialog queue={currentQueue?.id ?? MAIN_QUEUE} onclose={() => (adding = false)} />
{:else if incoming.length > 0}
  {#key incoming[0]}
    <AddDialog prefill={incoming[0]} onclose={() => incoming.shift()} />
  {/key}
{/if}
{#if queueDialog}
  <QueueDialog
    queue={queueDialog.queue}
    onclose={() => (queueDialog = null)}
    oncreated={(q) => {
      // Shown before the `queuesChanged` reload arrives.
      downloads.queues = [...downloads.queues.filter((x) => x.id !== q.id), q];
      view = { queue: q.id };
    }}
  />
{/if}
{#if limitDialog}
  {@const target = limitDialog}
  <SpeedLimitDialog
    title={target === "global" ? "Speed limit" : "Download speed limit"}
    note={target === "global"
      ? "Shared by all downloads together."
      : `For “${target.fileName}” only, on top of the overall limit.`}
    limit={target === "global" ? downloads.speedLimit : target.speedLimit}
    onsave={saveLimit}
    onclose={() => (limitDialog = null)}
  />
{/if}
{#if urlDialog}
  <UrlDialog item={urlDialog} onclose={() => (urlDialog = null)} />
{/if}
{#if settingsOpen}
  <SettingsDialog onclose={() => (settingsOpen = false)} />
{/if}
{#if moveDialog}
  <MoveDialog item={moveDialog} onclose={() => (moveDialog = null)} />
{/if}
{#if propertiesItem}
  {@const item = propertiesItem}
  <PropertiesDialog
    {item}
    onclose={() => (propertiesId = null)}
    onlimit={() => (limitDialog = item)}
    onaddress={() => (urlDialog = item)}
  />
{/if}
{#if menu}
  <ContextMenu x={menu.x} y={menu.y} items={menu.items} onclose={() => (menu = null)} />
{/if}
{#if removing}
  <RemoveDialog items={removing} onclose={() => (removing = null)} />
{/if}
{#if toast}
  <div class="toast" role="alert">
    <Icon name="alert" size={16} />
    <span>{toast}</span>
    <button class="icon" onclick={() => (toast = "")} aria-label="Dismiss"><Icon name="close" size={16} /></button>
  </div>
{/if}
