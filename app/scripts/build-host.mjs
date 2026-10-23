// Builds blazer-host for the installer, which ships it next to the app. Tauri expects it under
// the target's name, as in binaries/blazer-host-x86_64-pc-windows-msvc.exe.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const triple = execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (\S+)$/m)[1];
const ext = process.platform === "win32" ? ".exe" : "";

execFileSync("cargo", ["build", "--release", "-p", "blazer-host"], { cwd: root, stdio: "inherit" });
const out = join(root, "app", "src-tauri", "binaries");
mkdirSync(out, { recursive: true });
copyFileSync(join(root, "target", "release", `blazer-host${ext}`), join(out, `blazer-host-${triple}${ext}`));
