// Puts a "Download video" button on videos while the pointer is over them or they start playing.
// The button lists the video's qualities; picking one sends it to the app with that quality.
//
// Like IDM, it first uses the HLS or DASH playlist the player fetched: the app reads its
// qualities the moment it loads, so they are ready when the button is clicked. Pages without
// one, such as YouTube, whose player fetches the video in pieces, are asked about with yt-dlp,
// which a video that keeps playing gets in the background while the app runs.

(() => {
  // Private windows stay private, as with downloads.
  if (chrome.extension.inIncognitoContext) return;

  /** Smaller videos are previews and ads. */
  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 110;
  /** How long the button stays after the pointer leaves the video, or after it starts playing. */
  const LINGER_MS = 2500;
  const PLAY_SHOW_MS = 4000;
  /** How long a video plays before its qualities are looked up, which skips previews. */
  const PREFETCH_AFTER_MS = 2000;
  /** How long a lookup is reused. The app keeps its own for as long. */
  const ANSWER_TTL_MS = 5 * 60 * 1000;

  let enabled = true;
  let host = null;
  let button;
  let menu;
  /** The video the button belongs to. */
  let video = null;
  /** Set while the button waits to hide. */
  let hideTimer = null;
  let frame = 0;
  let menuOpen = false;

  chrome.storage.local.get({ videoButton: true }).then((s) => (enabled = s.videoButton));
  chrome.storage.local.onChanged.addListener((changes) => {
    if (!changes.videoButton) return;
    enabled = changes.videoButton.newValue ?? true;
    if (!enabled) hide();
  });

  const STYLE = `
    :host { all: initial; }
    .wrap {
      position: fixed; top: 0; left: 0; z-index: 2147483647;
      font: 13px/1.35 "Segoe UI Variable", "Segoe UI", system-ui, -apple-system, sans-serif;
      color: #e7e9ec; color-scheme: dark;
    }
    button { font: inherit; color: inherit; cursor: pointer; }
    .open {
      display: flex; align-items: center; gap: 6px;
      padding: 5px 10px 5px 8px; border: 0; border-radius: 999px;
      background: #f0612f; color: #fff; font-weight: 600;
      box-shadow: 0 2px 8px rgb(0 0 0 / 0.35);
      opacity: 0.92; transition: opacity 0.15s;
    }
    .open:hover, .open:focus-visible { opacity: 1; background: #d9481a; }
    .open svg { width: 16px; height: 16px; flex: none; }
    .menu {
      position: absolute; top: calc(100% + 6px); right: 0;
      width: 320px; max-height: 400px; overflow-y: auto;
      padding: 6px; border-radius: 10px;
      background: #1d2025; border: 1px solid #2c3036;
      box-shadow: 0 8px 24px rgb(0 0 0 / 0.45);
    }
    .near-left .menu { right: auto; left: 0; }
    .menu[hidden] { display: none; }
    .title { margin: 4px 8px 6px; font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .note { margin: 6px 8px; color: #8d949e; }
    .error { margin: 6px 8px; color: #f05d5d; overflow-wrap: anywhere; }
    .heading { margin: 10px 8px 4px; color: #8d949e; font-size: 12px; }
    .item {
      display: flex; width: 100%; align-items: baseline; gap: 8px;
      padding: 6px 8px; border: 0; border-radius: 6px; background: none; text-align: left;
    }
    .item:hover, .item:focus-visible { background: #2c3036; outline: none; }
    .item .label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .item .meta { color: #8d949e; font-size: 12px; white-space: nowrap; }
    .video { flex-direction: column; align-items: stretch; gap: 1px; }
    .video .label { flex: none; }
    .video .meta { overflow: hidden; text-overflow: ellipsis; }
    .spinner {
      display: inline-block; width: 12px; height: 12px; margin-right: 6px; vertical-align: -2px;
      border: 2px solid #8d949e; border-top-color: transparent; border-radius: 50%;
      animation: spin 0.8s linear infinite;
    }
    @keyframes spin { to { transform: rotate(360deg); } }
  `;

  /** An SVG element with `attrs`. Built without HTML strings, which store reviewers ask for. */
  function svgEl(tag, attrs) {
    const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, value);
    return node;
  }

  function downloadIcon() {
    const svg = svgEl("svg", {
      viewBox: "0 0 16 16",
      fill: "none",
      stroke: "currentColor",
      "stroke-width": "1.8",
      "stroke-linecap": "round",
      "stroke-linejoin": "round",
      "aria-hidden": "true",
    });
    svg.append(svgEl("path", { d: "M8 2v8M4.5 6.5 8 10l3.5-3.5M3 13h10" }));
    return svg;
  }

  function build() {
    host = document.createElement("internetblazer-video");
    const root = host.attachShadow({ mode: "closed" });
    const wrap = el("div", "wrap");
    button = el("button", "open");
    button.type = "button";
    button.title = "Download with InternetBlazer";
    button.append(downloadIcon(), "Download video");
    menu = el("div", "menu");
    menu.setAttribute("role", "menu");
    menu.hidden = true;
    wrap.append(button, menu);
    root.append(el("style", null, STYLE), wrap);
    button.addEventListener("click", (e) => {
      e.stopPropagation();
      menuOpen ? closeMenu() : openMenu();
    });
    // Clicks in the menu must not reach the page, which would pause the video or follow links.
    for (const type of ["click", "pointerdown", "mousedown", "dblclick"]) {
      wrap.addEventListener(type, (e) => e.stopPropagation());
    }
    wrap.addEventListener("pointerenter", cancelHide);
    wrap.addEventListener("pointerleave", () => lingerThenHide());
  }

  function attach() {
    if (!host) build();
    // In full screen only the full-screen element and its children show.
    const parent = document.fullscreenElement ?? document.documentElement;
    if (host.parentNode !== parent) parent.appendChild(host);
  }

  function usable(v) {
    if (!v.isConnected) return false;
    const r = v.getBoundingClientRect();
    return r.width >= MIN_WIDTH && r.height >= MIN_HEIGHT && getComputedStyle(v).visibility !== "hidden";
  }

  function show(v) {
    if (!enabled) return;
    if (video !== v) closeMenu();
    cancelHide();
    video = v;
    attach();
    host.style.display = "";
    if (!frame) frame = requestAnimationFrame(place);
  }

  function hide() {
    closeMenu();
    cancelHide();
    video = null;
    if (host) host.style.display = "none";
    cancelAnimationFrame(frame);
    frame = 0;
  }

  function lingerThenHide(ms = LINGER_MS) {
    if (menuOpen) return;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(hide, ms);
  }

  function cancelHide() {
    clearTimeout(hideTimer);
    hideTimer = null;
  }

  /** Keeps the button in the video's top right corner while the page scrolls or resizes. */
  function place() {
    frame = 0;
    if (!video || !usable(video)) {
      if (!menuOpen) hide();
      return;
    }
    const r = video.getBoundingClientRect();
    const wrap = button.parentElement;
    const width = button.offsetWidth;
    const left = Math.min(Math.max(r.right - width - 10, r.left + 4), innerWidth - width - 4);
    const top = Math.max(r.top + 10, 4);
    wrap.style.transform = `translate(${Math.round(left)}px, ${Math.round(top)}px)`;
    // The menu opens leftwards from the button, unless that would leave the window.
    wrap.classList.toggle("near-left", left + width < 330);
    frame = requestAnimationFrame(place);
  }

  /** The largest usable video under the point. */
  function videoAt(x, y) {
    let best = null;
    let bestArea = 0;
    for (const v of document.getElementsByTagName("video")) {
      const r = v.getBoundingClientRect();
      if (x < r.left || x > r.right || y < r.top || y > r.bottom) continue;
      const area = r.width * r.height;
      if (area > bestArea && usable(v)) {
        best = v;
        bestArea = area;
      }
    }
    return best;
  }

  // Pages often cover their videos with controls, so the pointer's target is rarely the video.
  let pending = false;
  document.addEventListener(
    "pointermove",
    (e) => {
      if (pending || !enabled) return;
      pending = true;
      requestAnimationFrame(() => {
        pending = false;
        const v = videoAt(e.clientX, e.clientY);
        if (v) show(v);
        else if (video && !hideTimer) lingerThenHide();
      });
    },
    // Captured, as some players stop the event on its way up.
    { passive: true, capture: true },
  );

  document.addEventListener(
    "play",
    (e) => {
      const v = e.target;
      if (!(v instanceof HTMLVideoElement) || !usable(v)) return;
      if (!menuOpen) {
        show(v);
        lingerThenHide(PLAY_SHOW_MS);
      }
      setTimeout(() => prefetch(v), PREFETCH_AFTER_MS);
    },
    true,
  );

  document.addEventListener("fullscreenchange", () => host && video && attach());
  document.addEventListener("keydown", (e) => menuOpen && e.key === "Escape" && closeMenu(), true);
  document.addEventListener("pointerdown", (e) => menuOpen && !e.composedPath().includes(host) && closeMenu());

  // The menu.

  function closeMenu() {
    if (!menuOpen) return;
    menuOpen = false;
    menu.hidden = true;
    menu.replaceChildren();
    lingerThenHide();
  }

  function openMenu() {
    menuOpen = true;
    cancelHide();
    menu.hidden = false;
    showMenu();
  }

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  function item(label, meta, onclick) {
    const node = el("button", "item");
    node.type = "button";
    node.setAttribute("role", "menuitem");
    node.append(el("span", "label", label));
    if (meta) node.append(el("span", "meta", meta));
    node.addEventListener("click", onclick);
    return node;
  }

  function bytes(n) {
    if (n == null) return "";
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    while (n >= 1000 && i < units.length - 1) {
      n /= 1000;
      i++;
    }
    return `about ${n < 10 && i > 0 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
  }

  /** A message from the menu to the background script, which talks to the app. */
  async function ask(message) {
    try {
      return (await chrome.runtime.sendMessage(message)) ?? { ok: false, error: "No reply." };
    } catch (e) {
      // The extension was updated or reloaded since this page loaded.
      return { ok: false, error: "Reload the page to use InternetBlazer here." };
    }
  }

  /** Answers about pages and streams by URL, each a promise with the time it was asked. */
  const answers = new Map();

  /** What yt-dlp finds at `url`. A prefetch never starts the app. */
  function inspect(url, prefetching = false) {
    return lookup(url, prefetching).reply;
  }

  function lookup(url, prefetching) {
    const known = answers.get(url);
    // A click asks again after a failure, which may have been brief.
    if (known && Date.now() - known.at < ANSWER_TTL_MS && !(known.failed && !prefetching)) {
      return known;
    }
    const reply = ask({ type: prefetching ? "prefetch" : "inspect", url });
    const entry = { reply, at: Date.now(), failed: false, prefetch: prefetching };
    answers.set(url, entry);
    reply.then((r) => {
      // Asked again later when the app wasn't running.
      if (r.appRunning === false) answers.delete(url);
      else entry.failed = !r.ok;
    });
    return entry;
  }

  /** Looks up a video that is still playing after a while, and that isn't a muted preview. */
  async function prefetch(v) {
    if (!enabled || v.paused || !usable(v)) return;
    if (v.muted && v.getBoundingClientRect().width < innerWidth * 0.4) return;
    // The background reads playlists as they load.
    const { streams = [] } = await ask({ type: "streams" });
    if (!streams.some((s) => s.kind !== "file")) inspect(location.href, true);
  }

  /** The most playlists the menu reads at once. */
  const MAX_PLAYLISTS = 4;

  async function showMenu() {
    const loading = el("p", "note", "Finding the qualities…");
    loading.prepend(el("span", "spinner"));
    menu.replaceChildren(loading);
    const { streams = [], title = "" } = await ask({ type: "streams" });
    const playlists = streams.filter((s) => s.kind !== "file").slice(0, MAX_PLAYLISTS);
    if (playlists.length > 0) {
      // Mostly read already, while the video loaded; reading one now takes a moment.
      const answers = await Promise.all(
        playlists.map((s) => (s.found?.ok ? s.found : inspect(s.url))),
      );
      if (!menuOpen) return;
      let found = playlists
        .map((stream, i) => ({ stream, media: answers[i].ok ? answers[i].media : null }))
        .filter((f) => f.media?.choices.length);
      // A master playlist lists every quality; the one-quality playlists it points to are left
      // out. Players fetch those too as they switch qualities.
      if (found.some((f) => f.media.choices.length > 1)) {
        found = found.filter((f) => f.media.choices.length > 1);
      }
      if (found.length > 0) {
        showFound(found, title);
        showFiles(streams);
        return;
      }
      // DRM and other reasons that stop every playlist.
      const reason = answers.find((a) => !a.ok)?.error;
      if (reason && !reason.startsWith("the link does not lead")) {
        menu.replaceChildren(el("p", "error", tidy(reason)));
        showFiles(streams);
        return;
      }
    }
    // No playlist the app can read, so yt-dlp looks at the page.
    await lookAt(location.href, true);
  }

  /** The qualities found in the page's playlists, one group per playlist. */
  function showFound(found, title) {
    const name = title || found[0].media.title;
    menu.replaceChildren(el("p", "title", name));
    menu.firstChild.title = name;
    for (const { stream, media } of found) {
      if (found.length > 1) menu.append(el("p", "heading", nameOf(stream.url)));
      showQualities(stream.url, media, name);
    }
  }

  /** One entry per quality, named as the file will be, with the quality and size below. */
  function showQualities(url, media, title) {
    // A title is no path, though it may hold slashes, as in "AC/DC".
    const stem = title.replace(/[\\/]/g, "_");
    for (const c of media.choices) {
      const fileName = `${stem}.${c.ext}`;
      const meta = [qualityOf(c, media), c.ext.toUpperCase(), bytes(c.size)].filter(Boolean).join(" · ");
      const node = item(fileName, meta, () => send({ type: "download", url, media: c.format, fileName }));
      node.classList.add("video");
      node.title = fileName;
      menu.append(node);
    }
  }

  /** A playlist with one quality doesn't say which; the playing video does. */
  function qualityOf(choice, media) {
    const single = media.choices.filter((c) => c.format.type === "video").length === 1;
    const height = video?.videoHeight;
    if (single && choice.format.type === "video" && choice.label === "Best quality" && height) {
      return `${height}p`;
    }
    return choice.label;
  }

  /** Video files the tab played directly, which download as they are. */
  function showFiles(streams) {
    const files = streams.filter((s) => s.kind === "file");
    if (files.length === 0) return;
    menu.append(el("p", "heading", "Video files on this page"));
    for (const s of files) {
      const meta = ["File", bytes(s.size)].filter(Boolean).join(" · ");
      menu.append(item(nameOf(s.url), meta, () => send({ type: "download", url: s.url })));
    }
  }

  /** Lists the qualities yt-dlp finds at `url`, the page itself or a stream on it. */
  async function lookAt(url, isPage) {
    const loading = el("p", "note", "Finding the qualities…");
    loading.prepend(el("span", "spinner"));
    menu.replaceChildren(loading);
    const entry = lookup(url, false);
    let reply = await entry.reply;
    // A lookup from while the video played, which may have found the app closed.
    if (entry.prefetch && !reply.ok) reply = await inspect(url);
    if (!menuOpen) return;
    const media = reply.ok ? reply.media : null;
    if (media?.choices.length) {
      showChoices(url, media);
    } else {
      const reason = !reply.ok
        ? reply.error
        : media?.ffmpeg === false
          ? "This video needs ffmpeg to join its picture and sound. Install ffmpeg (on Windows: winget install ffmpeg) and try again."
          : "yt-dlp found the video but no way to download it.";
      menu.replaceChildren(el("p", "error", tidy(reason)));
      if (isPage) await showStreams();
    }
  }

  /** yt-dlp's errors start with "ERROR: [generic] Unsupported URL: …". */
  function tidy(message) {
    const text = String(message).replace(/^ERROR:\s*/, "").replace(/^\[[^\]]+\]\s*/, "");
    return /^Unsupported URL/i.test(text) ? "yt-dlp doesn't know how to get videos from this page." : text;
  }

  function showChoices(url, media) {
    menu.replaceChildren(el("p", "title", media.title));
    menu.firstChild.title = media.title;
    showQualities(url, media, media.title);
  }

  /** Offers what the browser fetched for the video, for pages yt-dlp doesn't know. */
  async function showStreams() {
    const reply = await ask({ type: "streams" });
    if (!menuOpen) return;
    const streams = reply.streams ?? [];
    const src = video?.currentSrc;
    if (src && /^https?:/i.test(src) && !streams.some((s) => s.url === src)) {
      streams.unshift({ url: src, kind: "file" });
    }
    if (streams.length === 0) {
      menu.append(el("p", "note", "No video streams were seen on this page. Play the video, then try again."));
      return;
    }
    menu.append(el("p", "heading", "Video streams on this page"));
    for (const s of streams) {
      const label = nameOf(s.url);
      if (s.kind === "file") {
        const meta = ["File", bytes(s.size)].filter(Boolean).join(" · ");
        menu.append(item(label, meta, () => send({ type: "download", url: s.url })));
      } else {
        menu.append(item(label, s.kind.toUpperCase(), () => lookAt(s.url, false)));
      }
    }
  }

  function nameOf(url) {
    try {
      const u = new URL(url);
      const last = decodeURIComponent(u.pathname.split("/").pop() || "");
      return last ? `${last} (${u.hostname})` : u.hostname;
    } catch {
      return url;
    }
  }

  async function send(message) {
    const sending = el("p", "note", "Sending to InternetBlazer…");
    sending.prepend(el("span", "spinner"));
    menu.replaceChildren(sending);
    const reply = await ask(message);
    if (!menuOpen) return;
    if (reply.ok) {
      menu.replaceChildren(el("p", "note", "Sent to InternetBlazer."));
      setTimeout(closeMenu, 1200);
    } else {
      menu.replaceChildren(el("p", "error", reply.error ?? "InternetBlazer did not reply."));
    }
  }
})();
