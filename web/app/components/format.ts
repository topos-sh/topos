/**
 * Pure display formatting shared by every surface (dashboard, skill, review, settings). Rendered
 * as text nodes.
 */

/** Calm relative time — "just now", "5 minutes ago", "3 hours ago", "2 days ago". */
export function relativeTime(value: string | Date, now: Date = new Date()): string {
  const then = typeof value === "string" ? new Date(value) : value;
  const millis = now.getTime() - then.getTime();
  if (!Number.isFinite(millis)) {
    return "";
  }
  const seconds = Math.max(0, Math.floor(millis / 1000));
  if (seconds < 60) {
    return "just now";
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return minutes === 1 ? "1 minute ago" : `${minutes} minutes ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  }
  const days = Math.floor(hours / 24);
  if (days < 30) {
    return days === 1 ? "1 day ago" : `${days} days ago`;
  }
  const months = Math.floor(days / 30);
  if (months < 12) {
    return months === 1 ? "1 month ago" : `${months} months ago`;
  }
  const years = Math.floor(months / 12);
  return years === 1 ? "1 year ago" : `${years} years ago`;
}

/** A commit message's title line. */
export function firstLine(message: string): string {
  const line = message.split("\n", 1)[0] ?? "";
  return line.trim();
}

/** The short form of a device id for "device <short>" lines. */
export function shortDevice(deviceId: string): string {
  return deviceId.slice(0, 8);
}

/** "12.3 KiB"-style byte count. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const kib = bytes / 1024;
  if (kib < 1024) {
    return `${kib.toFixed(1)} KiB`;
  }
  return `${(kib / 1024).toFixed(1)} MiB`;
}
