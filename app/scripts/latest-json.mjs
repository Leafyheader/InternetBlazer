// Writes latest.json, which installed copies read to find an update. It points them at the update
// installer, the app without yt-dlp, deno and ffmpeg: those come with the full installer, an
// update keeps them, and the app fetches any that are missing.
//
// Usage: node scripts/latest-json.mjs <tag> <folder holding InternetBlazer_<version>_x64-update.exe(.sig)>
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [tag, dir] = process.argv.slice(2);
if (!tag || !dir) throw new Error("usage: latest-json.mjs <tag> <folder>");
const version = tag.replace(/^v/, "");
const file = `InternetBlazer_${version}_x64-update.exe`;
const platform = {
  signature: readFileSync(join(dir, `${file}.sig`), "utf8").trim(),
  url: `https://github.com/Leafyheader/InternetBlazer/releases/download/${tag}/${file}`,
};
const latest = {
  version,
  notes: "See the commits since the last release for what changed.",
  pub_date: new Date().toISOString(),
  // The updater looks for its `{os}-{arch}-{installer}` key first, then `{os}-{arch}`.
  platforms: { "windows-x86_64-nsis": platform, "windows-x86_64": platform },
};
writeFileSync(join(dir, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);
console.log(`latest.json: ${version} -> ${file}`);
