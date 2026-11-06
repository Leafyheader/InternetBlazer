// Fetches yt-dlp, deno and ffmpeg for the installer, so video downloads work without downloading
// anything after installing. Each lands in src-tauri/tools/<tool>/, which the installer ships and
// the app looks in after its own updatable copies. Every file is checked against the checksum its
// project publishes, since these are programs that will run.
//
// ffmpeg is BtbN's LGPL build of the newest release branch: the LGPL lets it ship with the app
// with its license and a pointer to its source, which go in tools/ffmpeg/. The shared build has
// ffmpeg and ffprobe use one set of DLLs, where the static one holds a copy in each (250 MB).
//
// Already fetched versions are kept; delete src-tauri/tools, or pass --fresh, to fetch again.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

if (process.platform !== "win32" || process.arch !== "x64") {
  console.log("fetch-tools: only the Windows x64 installer bundles tools; skipping.");
  process.exit(0);
}

const out = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "tools");
const fresh = process.argv.includes("--fresh");
// Windows' own tar reads zip files; Git's GNU tar, which may come first on PATH, does not.
const tar = join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");

async function get(url, as = "buffer") {
  const resp = await fetch(url, { headers: { "User-Agent": "InternetBlazer-build" } });
  if (!resp.ok) throw new Error(`${url}: ${resp.status} ${resp.statusText}`);
  return as === "json" ? resp.json() : as === "text" ? resp.text() : Buffer.from(await resp.arrayBuffer());
}

/** The hash for `name` in a `<hash> <name>` list, where names may start with `*`. */
function hashIn(sums, name) {
  for (const line of sums.split("\n")) {
    const [hash, file] = line.trim().split(/\s+/);
    if (file?.replace(/^\*/, "") === name) return hash.toLowerCase();
  }
  throw new Error(`no checksum listed for ${name}`);
}

function checked(data, expected, name) {
  const actual = createHash("sha256").update(data).digest("hex");
  if (actual !== expected) throw new Error(`${name} does not match its published checksum`);
  return data;
}

/** Unpacks `zip` and returns the folder it went to. */
function unzip(zip) {
  const dir = mkdtempSync(join(tmpdir(), "blazer-tools-"));
  const file = join(dir, "download.zip");
  writeFileSync(file, zip);
  const files = join(dir, "files");
  mkdirSync(files);
  execFileSync(tar, ["-xf", file, "-C", files], { stdio: "inherit" });
  return files;
}

async function tool(name, fetchInto) {
  const dest = join(out, name);
  if (existsSync(dest) && !fresh) {
    console.log(`fetch-tools: ${name} already fetched`);
    return;
  }
  console.log(`fetch-tools: fetching ${name}…`);
  rmSync(dest, { recursive: true, force: true });
  const partial = `${dest}.partial`;
  rmSync(partial, { recursive: true, force: true });
  mkdirSync(partial, { recursive: true });
  await fetchInto(partial);
  cpSync(partial, dest, { recursive: true });
  rmSync(partial, { recursive: true, force: true });
}

// The folder build, which starts in under a second where the single file unpacks itself first.
await tool("yt-dlp", async (dest) => {
  const base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";
  const sums = await get(`${base}SHA2-256SUMS`, "text");
  const zip = checked(await get(`${base}yt-dlp_win.zip`), hashIn(sums, "yt-dlp_win.zip"), "yt-dlp");
  cpSync(unzip(zip), dest, { recursive: true });
});

await tool("deno", async (dest) => {
  const asset = "deno-x86_64-pc-windows-msvc.zip";
  const base = "https://github.com/denoland/deno/releases/latest/download/";
  const expected = (await get(`${base}${asset}.sha256sum`, "text")).match(/[0-9a-fA-F]{64}/)?.[0];
  if (!expected) throw new Error(`could not read the checksum published for ${asset}`);
  const zip = checked(await get(`${base}${asset}`), expected.toLowerCase(), "deno");
  cpSync(join(unzip(zip), "deno.exe"), join(dest, "deno.exe"));
});

await tool("ffmpeg", async (dest) => {
  const release = await get("https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/tags/latest", "json");
  const version = (name) => name.match(/^ffmpeg-n([\d.]+)-latest-win64-lgpl-shared-[\d.]+\.zip$/)?.[1];
  const newest = release.assets
    .filter((a) => version(a.name))
    .sort((a, b) => version(b.name).localeCompare(version(a.name), undefined, { numeric: true }))[0];
  const sumsAsset = release.assets.find((a) => a.name === "checksums.sha256");
  if (!newest || !sumsAsset) throw new Error("BtbN's latest release has no LGPL Windows build");
  const sums = await get(sumsAsset.browser_download_url, "text");
  const zip = checked(await get(newest.browser_download_url), hashIn(sums, newest.name), "ffmpeg");
  const files = unzip(zip);
  const [top] = readdirSync(files);
  // Every DLL, and the two programs yt-dlp runs; not the player.
  for (const file of readdirSync(join(files, top, "bin"))) {
    if (file !== "ffplay.exe") cpSync(join(files, top, "bin", file), join(dest, file));
  }
  cpSync(join(files, top, "LICENSE.txt"), join(dest, "LICENSE.txt"));
  writeFileSync(
    join(dest, "SOURCE.txt"),
    `FFmpeg ${version(newest.name)}, built by https://github.com/BtbN/FFmpeg-Builds (${newest.name}),\n` +
      "under the GNU Lesser General Public License, version 2.1 or later (LICENSE.txt).\n" +
      "Its source code: https://ffmpeg.org/download.html and https://github.com/BtbN/FFmpeg-Builds\n",
  );
});
