/**
 * Pure display formatting shared by every surface (dashboard, skill, review, settings). Rendered
 * as text nodes.
 */

/**
 * Calm relative time — "just now", "5 minutes ago", "3 hours ago", "2 days ago".
 *
 * The default anchor is the current MINUTE, not the current instant: this string renders as a
 * server-rendered text node that the client re-computes during hydration, and any drift between
 * the two clock reads that crosses a bucket edge is a hydration mismatch (React re-renders the
 * whole page over it). Flooring both reads to the minute makes them agree unless the render
 * pair straddles a minute tick — and no bucket below is finer than a minute, so the display
 * loses nothing.
 */
export function relativeTime(value: string | Date, now?: Date): string {
  const anchor = now ?? new Date(Math.floor(Date.now() / 60_000) * 60_000);
  const then = typeof value === "string" ? new Date(value) : value;
  const millis = anchor.getTime() - then.getTime();
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

/**
 * The absolute instant behind a relative time — "2026-08-24 19:23 UTC".
 *
 * UTC and fixed-format on purpose: a locale- or zone-derived string is computed differently on
 * the server and in the browser, and this renders as a server-rendered text node the client
 * re-computes at hydration. One spelling everywhere is what keeps them equal.
 */
export function utcStamp(value: number | string | Date): string {
  const then = value instanceof Date ? value : new Date(value);
  const at = then.getTime();
  if (!Number.isFinite(at)) {
    return "";
  }
  return `${then.toISOString().slice(0, 16).replace("T", " ")} UTC`;
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

/**
 * A custody-recorded author fit for humans, or nothing. The client folds its device id
 * (`d_` + 32 hex) into the commit as the author — machine identity that keeps the
 * content-addressed commit id stable, not a name — so an id-shaped author is withheld and the
 * directory-backed "proposed by" line stays the human attribution. Any other recorded author
 * passes through untouched.
 */
export function humanAuthor(author: string | undefined): string | undefined {
  if (author === undefined || /^d_[0-9a-f]{32}$/.test(author)) {
    return undefined;
  }
  return author;
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
