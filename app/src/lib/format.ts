import type { MediaFormat } from "./api";

const UNITS = ["B", "KB", "MB", "GB", "TB"];

export function bytes(n: number): string {
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${n} B` : `${value.toFixed(value >= 100 ? 0 : 1)} ${UNITS[unit]}`;
}

export function duration(secs: number): string {
  secs = Math.round(secs);
  if (secs >= 86400) return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
  if (secs >= 3600) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  if (secs >= 60) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  return `${secs}s`;
}

export function date(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleString();
}

/** Upper-case extension for the file badge, or an empty string. */
export function extension(fileName: string): string {
  const dot = fileName.lastIndexOf(".");
  return dot > 0 && fileName.length - dot <= 6 ? fileName.slice(dot + 1).toUpperCase() : "";
}

const KINDS: [string, string[]][] = [
  ["video", ["MP4", "MKV", "WEBM", "AVI", "MOV", "WMV", "FLV", "M4V"]],
  ["audio", ["MP3", "FLAC", "WAV", "OGG", "M4A", "AAC", "OPUS"]],
  ["archive", ["ZIP", "RAR", "7Z", "TAR", "GZ", "XZ", "BZ2", "ZST"]],
  ["program", ["EXE", "MSI", "DMG", "PKG", "DEB", "RPM", "APPIMAGE", "APK"]],
  ["document", ["PDF", "DOC", "DOCX", "XLS", "XLSX", "PPT", "PPTX", "TXT", "EPUB"]],
  ["image", ["PNG", "JPG", "JPEG", "GIF", "WEBP", "SVG", "AVIF"]],
  ["disc", ["ISO", "IMG"]],
];

/** A coarse file category, used to color the file badge. */
export function kind(ext: string): string {
  return KINDS.find(([, exts]) => exts.includes(ext))?.[0] ?? "other";
}

/** "HH:MM" for minutes after midnight, as `<input type="time">` uses. */
export function clock(minutes: number): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(minutes / 60))}:${pad(minutes % 60)}`;
}

/** Minutes after midnight for an "HH:MM" time. */
export function minutes(clock: string): number {
  const [h, m] = clock.split(":").map(Number);
  return (h || 0) * 60 + (m || 0);
}

/** The quality a video download asked for. */
export function quality(format: MediaFormat): string {
  if (format.type === "audio") return "Audio only";
  return format.maxHeight === null ? "Best available" : `Up to ${format.maxHeight}p`;
}
