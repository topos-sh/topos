/**
 * The /verify page's pass-through params, shape-validated in ONE place — shared by the verify
 * page itself (its selfPath + the action's hidden-field re-read) and the /login loader (which
 * REBUILDS a `next` that targets /verify from these validated components, so the resume target
 * after a sign-in — same browser or a mailed magic link opened anywhere — is server-derived,
 * never an echoed raw string). Client-safe: pure string work, no server import.
 *
 * The params are all NON-SECRET: `device` is the hex of the flow code's own SHA-256 (the
 * pre-arm challenge — identifying, never revealing), `port`/`state` name the CLI's ephemeral
 * 127.0.0.1 listener. Anything off-shape is dropped, not echoed.
 */

/** The loopback listener coordinates a CLI-opened arrival carries. */
export interface Loopback {
  port: string;
  state: string;
}

/** Read + shape-validate the pass-through params from a query or form-data source. */
export function loopbackFrom(source: { get(name: string): string | null | FormDataEntryValue }): {
  device: string | null;
  loopback: Loopback | null;
} {
  const device = String(source.get("device") ?? "");
  const port = String(source.get("port") ?? "");
  const state = String(source.get("state") ?? "");
  return {
    device: /^[0-9a-f]{64}$/.test(device) ? device : null,
    loopback:
      /^\d{4,5}$/.test(port) &&
      Number(port) >= 1024 &&
      Number(port) <= 65535 &&
      /^[A-Za-z0-9_-]{8,128}$/.test(state)
        ? { port, state }
        : null,
  };
}

/** The verify page's own address, carrying only the validated pass-through params. */
export function verifySelfPath(device: string | null, loopback: Loopback | null): string {
  const qs = new URLSearchParams();
  if (device !== null) {
    qs.set("device", device);
  }
  if (loopback !== null) {
    qs.set("port", loopback.port);
    qs.set("state", loopback.state);
  }
  const search = qs.toString();
  return `/verify${search === "" ? "" : `?${search}`}`;
}

/**
 * Rebuild a `next` value that targets /verify into the CANONICAL verify path — validated
 * components only, everything else dropped. Returns null when the value does not target
 * /verify (the caller falls back to its ordinary same-app-path validation). Parsing rides the
 * WHATWG URL machinery against a throwaway base, so backslash/escape tricks normalize before
 * the pathname check instead of surviving into a redirect.
 */
export function rebuildVerifyNext(raw: string | undefined): string | null {
  if (raw === undefined || !raw.startsWith("/verify")) {
    return null;
  }
  let url: URL;
  try {
    url = new URL(raw, "http://verify.invalid");
  } catch {
    return null;
  }
  if (url.host !== "verify.invalid" || url.pathname !== "/verify") {
    return null;
  }
  const { device, loopback } = loopbackFrom(url.searchParams);
  return verifySelfPath(device, loopback);
}
