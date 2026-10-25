// Packs extension/ into the zips the stores take, in target/extension/:
//   internetblazer-edge-<version>.zip     for Microsoft Edge Add-ons (and later the Chrome Web Store)
//   internetblazer-firefox-<version>.zip  for addons.mozilla.org
// The stores give the extension its own ID, so the `key` that fixes the unpacked ID is left out,
// and each browser gets only the background entry it uses.
import { deflateRawSync } from "node:zlib";
import { mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const source = join(root, "extension");
const out = join(root, "target", "extension");

const manifest = JSON.parse(readFileSync(join(source, "manifest.json"), "utf8"));
const { key, browser_specific_settings, background, ...common } = manifest;

const targets = {
  edge: { ...common, background: { service_worker: background.service_worker } },
  firefox: { ...common, background: { scripts: background.scripts }, browser_specific_settings },
};

function files(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? files(path) : [path];
  });
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(data) {
  let c = 0xffffffff;
  for (const byte of data) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** A zip of `entries` ({ name, data }), deflated, with a fixed date so builds are repeatable. */
function zip(entries) {
  const DOS_TIME = 0;
  const DOS_DATE = ((2026 - 1980) << 9) | (1 << 5) | 1;
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const { name, data } of entries) {
    const nameBytes = Buffer.from(name, "utf8");
    const packed = deflateRawSync(data, { level: 9 });
    const crc = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6); // UTF-8 names
    local.writeUInt16LE(8, 8); // deflate
    local.writeUInt16LE(DOS_TIME, 10);
    local.writeUInt16LE(DOS_DATE, 12);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(packed.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    locals.push(local, nameBytes, packed);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0x0800, 8);
    central.writeUInt16LE(8, 10);
    central.writeUInt16LE(DOS_TIME, 12);
    central.writeUInt16LE(DOS_DATE, 14);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(packed.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, nameBytes);
    offset += local.length + nameBytes.length + packed.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

mkdirSync(out, { recursive: true });
const sources = files(source)
  .map((path) => ({ name: relative(source, path).split(sep).join("/"), path }))
  .filter(({ name }) => name !== "manifest.json" && !name.endsWith(".md"))
  .sort((a, b) => a.name.localeCompare(b.name));

for (const [target, targetManifest] of Object.entries(targets)) {
  const entries = [
    { name: "manifest.json", data: Buffer.from(JSON.stringify(targetManifest, null, 2) + "\n") },
    ...sources.map(({ name, path }) => ({ name, data: readFileSync(path) })),
  ];
  const file = join(out, `internetblazer-${target}-${manifest.version}.zip`);
  writeFileSync(file, zip(entries));
  console.log(`${relative(root, file)}  (${entries.length} files)`);
}
