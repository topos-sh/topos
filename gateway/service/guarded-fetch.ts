import { lookup } from "node:dns/promises";
import { isIP } from "node:net";
import type { Logger } from "./log";

/**
 * The GUARDED fetch — the one transport every upstream request rides (proxied MCP calls, the
 * OAuth discovery walk, token exchanges). Policy:
 *
 *  · https only — an upstream address arrived DB-checked (`^https://`), and every hop after it
 *    (redirects, discovered endpoints) is held to the same bar;
 *  · loopback, RFC 1918, CGNAT, link-local, unique-local, and metadata ranges refuse unless
 *    `allowPrivate` (GATEWAY_ALLOW_PRIVATE_UPSTREAMS=1 — self-host with internal servers);
 *  · redirects are handled MANUALLY and re-guarded per hop, so an upstream cannot bounce the
 *    gateway into a private range;
 *  · the hostname's EVERY resolved address is classified before the connection.
 *
 * RESIDUAL GAP, stated honestly: Bun's fetch exposes no connect-by-IP-with-SNI seam, so the
 * connection re-resolves the name after this check — a DNS answer that flips between the check
 * and the dial (rebinding with a ~0 TTL) can still reach a private address. The window is one
 * resolver round-trip; closing it needs a socket-level dial (a follow-up once Bun exposes one).
 */

export interface FetchPolicy {
  allowPrivate: boolean;
  /** Test rigs only — production construction never sets it. */
  allowInsecure?: boolean;
}

const MAX_REDIRECTS = 5;

// Headers safe to carry to a DIFFERENT origin on a redirect: content negotiation and the MCP
// protocol framing, none of which is a credential. Everything else — `Authorization`, any
// document-declared secret header, cookies — is dropped, so no secret follows a cross-origin
// bounce regardless of the name it rode under.
const CROSS_ORIGIN_SAFE_HEADERS = new Set([
  "accept",
  "accept-encoding",
  "content-type",
  "content-length",
  "user-agent",
  "mcp-protocol-version",
  "mcp-method",
  "mcp-name",
]);

function ipv4Blocked(addr: string): boolean {
  const parts = addr.split(".").map(Number);
  const [a, b] = parts;
  if (a === undefined || b === undefined) {
    return true;
  }
  return (
    a === 0 || // "this network"
    a === 10 || // RFC 1918
    a === 127 || // loopback
    (a === 100 && b >= 64 && b <= 127) || // CGNAT (cloud metadata lives here too)
    (a === 169 && b === 254) || // link-local incl. 169.254.169.254
    (a === 172 && b >= 16 && b <= 31) || // RFC 1918
    (a === 192 && b === 0) || // IETF protocol assignments + TEST-NET-1
    (a === 192 && b === 168) || // RFC 1918
    (a === 198 && (b === 18 || b === 19)) || // benchmarking
    (a === 198 && b === 51) || // TEST-NET-2
    (a === 203 && b === 0) || // TEST-NET-3
    a >= 224 // multicast, reserved, broadcast
  );
}

/** True when the address must not be dialed under the default policy. Unparseable ⇒ blocked. */
export function addressBlocked(addr: string): boolean {
  const family = isIP(addr);
  if (family === 4) {
    return ipv4Blocked(addr);
  }
  if (family !== 6) {
    return true;
  }
  const lower = addr.toLowerCase();
  // IPv4-mapped forms classify as the IPv4 they carry — BOTH spellings (the WHATWG URL parser
  // re-serializes ::ffff:127.0.0.1 into the hex form ::ffff:7f00:1).
  const dotted = lower.match(/^::ffff:(\d+\.\d+\.\d+\.\d+)$/);
  if (dotted?.[1] !== undefined) {
    return ipv4Blocked(dotted[1]);
  }
  const hexed = lower.match(/^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/);
  if (hexed?.[1] !== undefined && hexed[2] !== undefined) {
    const hi = Number.parseInt(hexed[1], 16);
    const lo = Number.parseInt(hexed[2], 16);
    return ipv4Blocked(`${hi >> 8}.${hi & 255}.${lo >> 8}.${lo & 255}`);
  }
  if (lower === "::" || lower === "::1") {
    return true;
  }
  return (
    lower.startsWith("fe8") || // link-local fe80::/10 (fe80–febf all start fe8..feb)
    lower.startsWith("fe9") ||
    lower.startsWith("fea") ||
    lower.startsWith("feb") ||
    lower.startsWith("fc") || // unique-local fc00::/7
    lower.startsWith("fd") ||
    lower.startsWith("ff") || // multicast
    lower.startsWith("64:ff9b") || // NAT64 — an embedded v4 we refuse rather than unwrap
    lower.startsWith("2001:db8") // documentation
  );
}

async function assertDialable(url: URL, policy: FetchPolicy): Promise<void> {
  if (url.protocol !== "https:" && policy.allowInsecure !== true) {
    throw new Error("upstream requests are https-only");
  }
  if (url.username !== "" || url.password !== "") {
    throw new Error("upstream URLs must not carry userinfo");
  }
  if (policy.allowPrivate) {
    return;
  }
  const bare = url.hostname.replace(/^\[|\]$/g, "");
  if (isIP(bare) !== 0) {
    if (addressBlocked(bare)) {
      throw new Error("upstream address is in a blocked range");
    }
    return;
  }
  let addresses: { address: string }[];
  try {
    addresses = await lookup(bare, { all: true, verbatim: true });
  } catch {
    throw new Error("upstream hostname does not resolve");
  }
  if (addresses.length === 0) {
    throw new Error("upstream hostname does not resolve");
  }
  // EVERY answer must be public — a name that mixes a public and a private address is exactly
  // the rebinding shape this exists to refuse.
  for (const { address } of addresses) {
    if (addressBlocked(address)) {
      throw new Error("upstream address is in a blocked range");
    }
  }
}

/** Build the CoreEnv fetch: guard, dial, and walk redirects one guarded hop at a time. */
export function createGuardedFetch(
  policy: FetchPolicy,
  log: Logger,
  baseFetch: typeof fetch = fetch,
): typeof fetch {
  const guarded = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    // Normalize to one Request so a redirect hop can re-derive method/headers once. Bodies are
    // one-shot streams: hop 0 sends the original; a later hop either dropped it (GET demotion)
    // or refuses (a stream cannot replay, and buffering would break the pipe-don't-buffer rule).
    const request = input instanceof Request ? new Request(input, init) : new Request(input, init);
    const hadBody = request.body !== null;
    let url = new URL(request.url);
    let method = request.method;
    let headers = new Headers(request.headers);
    for (let hop = 0; ; hop += 1) {
      await assertDialable(url, policy);
      const response = await baseFetch(
        new Request(url, {
          method,
          headers,
          body: hop === 0 ? request.body : null,
          redirect: "manual",
          signal: request.signal,
          // Bun needs this for streamed request bodies; Node's undici requires it too.
          ...(hadBody && hop === 0 ? { duplex: "half" as const } : {}),
        } as RequestInit),
      );
      if (response.status < 300 || response.status >= 400) {
        return response;
      }
      const location = response.headers.get("location");
      if (location === null) {
        return response;
      }
      if (hop + 1 > MAX_REDIRECTS) {
        throw new Error("upstream redirect chain too long");
      }
      const next = new URL(location, url);
      // 303 (and the legacy 301/302-on-POST behavior) demotes to GET, dropping the body; 307/308
      // preserve the method — which would mean replaying a stream already sent, so refuse.
      const demoted =
        response.status === 303 ||
        (method === "POST" && response.status !== 307 && response.status !== 308);
      if (demoted) {
        method = "GET";
      } else if (hadBody) {
        throw new Error("upstream redirected a request whose body cannot be replayed");
      }
      // Credentials never travel across origins on a redirect. The caller attaches the upstream
      // secret under `Authorization` OR a document-declared custom header (e.g. `X-API-Key`), and
      // this wrapper cannot know which — so cross-origin we KEEP only a safelist of non-credential
      // headers and drop the rest. Anything carrying a secret is dropped by construction.
      if (next.origin !== url.origin) {
        const kept = new Headers();
        headers.forEach((value, name) => {
          if (CROSS_ORIGIN_SAFE_HEADERS.has(name.toLowerCase())) {
            kept.set(name, value);
          }
        });
        headers = kept;
      }
      log("info", "following upstream redirect", { status: response.status, hop: hop + 1 });
      url = next;
    }
  };
  return guarded as typeof fetch;
}
