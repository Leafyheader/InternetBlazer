// Writes latest.json, which installed copies read to find an update, and notes.md, the release's
// description on GitHub. Windows copies get the update installer, the app without yt-dlp, deno and
// ffmpeg: those come with the full installer, an update keeps them, and the app fetches any that
// are missing. Linux copies get the same .deb or .rpm people download.
//
// The notes are the version's section of CHANGELOG.md, which the app's update dialog shows.
//
// Usage: node scripts/latest-json.mjs <tag> <folder holding the update files and their .sig files>
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [tag, dir] = process.argv.slice(2);
if (!tag || !dir) throw new Error("usage: latest-json.mjs <tag> <folder>");
const version = tag.replace(/^v/, "");

/** The lines under `## <version>` in the changelog, up to the next section. */
function changes() {
  const changelog = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "CHANGELOG.md");
  const lines = readFileSync(changelog, "utf8").split(/\r?\n/);
  const start = lines.findIndex((line) => line.trim() === `## ${version}`);
  if (start < 0) throw new Error(`CHANGELOG.md has no "## ${version}" section`);
  const end = lines.findIndex((line, i) => i > start && line.startsWith("## "));
  return lines
    .slice(start + 1, end < 0 ? undefined : end)
    .join("\n")
    .trim();
}

/** The update named `file` in `dir`, with its signature. */
function update(file) {
  return {
    signature: readFileSync(join(dir, `${file}.sig`), "utf8").trim(),
    url: `https://github.com/Leafyheader/InternetBlazer/releases/download/${tag}/${file}`,
  };
}

/** The one file in `dir` whose name starts with `prefix` and ends with `suffix`. */
function named(prefix, suffix) {
  const found = readdirSync(dir).filter((name) => name.startsWith(prefix) && name.endsWith(suffix));
  if (found.length !== 1) throw new Error(`expected one ${prefix}…${suffix} in ${dir}, found ${found.length}`);
  return found[0];
}

const windows = update(`InternetBlazer_${version}_x64-update.exe`);
const notes = changes();
const latest = {
  version,
  notes,
  pub_date: new Date().toISOString(),
  // The updater looks for its `{os}-{arch}-{installer}` key first, then `{os}-{arch}`.
  platforms: {
    "windows-x86_64-nsis": windows,
    "windows-x86_64": windows,
    "linux-x86_64-deb": update(named(`InternetBlazer_${version}_`, ".deb")),
    "linux-x86_64-rpm": update(named(`InternetBlazer-${version}-`, ".rpm")),
  },
};
writeFileSync(join(dir, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);
writeFileSync(join(dir, "notes.md"), `${notes}\n`);
console.log(`latest.json: ${version} -> ${Object.keys(latest.platforms).join(", ")}`);
