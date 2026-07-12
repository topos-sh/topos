/**
 * Text-vs-binary classification for display. Deliberately STRICTER than pure UTF-8 validity:
 * a NUL byte in the first 8000 bytes marks binary even when the bytes happen to decode
 * (git's heuristic — it catches UTF-16 and friends), because the goal here is display
 * safety, not fidelity. Text is preserved verbatim: BOM and CRLF survive decoding.
 */

const NUL_SCAN_BYTES = 8000;

export function classifyBytes(bytes: Uint8Array): "text" | "binary" {
  const scan = Math.min(bytes.length, NUL_SCAN_BYTES);
  for (let i = 0; i < scan; i++) {
    if (bytes[i] === 0) {
      return "binary";
    }
  }
  try {
    decodeTextVerbatim(bytes);
  } catch {
    return "binary";
  }
  return "text";
}

/** Fatal-mode UTF-8 decode that keeps a leading BOM (and CRLF) byte-for-byte. */
export function decodeTextVerbatim(bytes: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
}
