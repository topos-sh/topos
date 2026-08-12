import { Buffer } from "node:buffer";
import { lookup } from "node:dns/promises";
import { request as httpsRequest } from "node:https";
import { isIPv4 } from "node:net";
import { Readable } from "node:stream";
import { MAX_SERVER_JSON_BYTES, nestsTooDeep } from "@/lib/mcp/validate.server";

/**
 * THE TWO SERVER-SIDE FETCHES the add-an-MCP-server page can make, and the guard that makes the
 * second one safe.
 *
 * A pasted document needs no fetch at all — that is the path an internal server takes. The
 * other two reach out from THIS process, which sits inside the deployment's own network, so a
 * URL a member types is an SSRF primitive unless it is checked: the guard below refuses
 * anything but https, resolves the host itself, and refuses every address that is not on the
 * public internet — loopback, private space, link-local (the cloud metadata endpoint lives
 * there), unique-local v6, the NAT64 prefixes that carry v4 space, and the unspecified/multicast
 * ranges. The connection is then DIALED at those vetted addresses, so nothing resolves a second
 * time and a rebinding answer has nowhere to land. A 3xx is refused rather than followed, so a
 * checked host cannot walk the fetch into an unchecked one; the size cap and timeout bound what
 * a hostile endpoint can cost.
 *
 * The official registry is a FIXED host and needs no host guard — but it gets the same cap,
 * timeout and manual-redirect discipline, because "we trust the host" is not a reason to let
 * one answer occupy the process.
 */

/** The registry the import page reads a named server from. */
export const REGISTRY_BASE = "https://registry.modelcontextprotocol.io";
const FETCH_TIMEOUT_MS = 15_000;

/** The injectable seam: production dials the network, tests hand back bytes. */
export type ServerJsonFetcher = (url: string) => Promise<FetchedDocument>;

export interface FetchedDocument {
  /** The document's raw text — what the validator scans and what the bundle stores. */
  text: string;
  /** The URL the bytes actually came from (never a redirect target — redirects are refused). */
  url: string;
}

export class McpFetchError extends Error {}

// ── The SSRF guard ──────────────────────────────────────────────────────────────────────────

/** The v4 ranges that are not the public internet. */
function isPrivateV4(address: string): boolean {
  const parts = address.split(".").map((n) => Number.parseInt(n, 10));
  const [a, b] = parts as [number, number, number, number];
  if (parts.length !== 4 || parts.some((n) => !Number.isInteger(n) || n < 0 || n > 255)) {
    return true; // unparseable is not provably public — refuse
  }
  if (a === 0 || a === 10 || a === 127) {
    return true; // this network · private · loopback
  }
  if (a === 169 && b === 254) {
    return true; // link-local — the cloud metadata address lives here
  }
  if (a === 172 && b >= 16 && b <= 31) {
    return true; // private
  }
  if (a === 192 && b === 168) {
    return true; // private
  }
  if (a === 100 && b >= 64 && b <= 127) {
    return true; // carrier-grade NAT
  }
  if (a === 192 && b === 0) {
    return true; // IETF protocol assignments / 192.0.2.0 documentation
  }
  if (a === 198 && (b === 18 || b === 19)) {
    return true; // benchmarking
  }
  if (a >= 224) {
    return true; // multicast + reserved + broadcast
  }
  return false;
}

/**
 * Parse an IPv6 literal into its eight 16-bit groups (`::` expanded, an embedded dotted quad
 * folded into the last two groups). `null` when the string is not a well-formed address.
 */
function ipv6Groups(value: string): number[] | null {
  let head = value;
  let tail = "";
  const gap = value.indexOf("::");
  if (gap !== -1) {
    if (value.indexOf("::", gap + 1) !== -1) {
      return null; // two gaps
    }
    head = value.slice(0, gap);
    tail = value.slice(gap + 2);
  }
  const parse = (part: string): number[] | null => {
    if (part === "") {
      return [];
    }
    const groups: number[] = [];
    const pieces = part.split(":");
    for (const [i, piece] of pieces.entries()) {
      // A trailing dotted quad (the v4-embedded spelling) becomes the last two groups.
      if (i === pieces.length - 1 && piece.includes(".")) {
        const quad = piece.split(".").map((n) => Number.parseInt(n, 10));
        if (quad.length !== 4 || quad.some((n) => !Number.isInteger(n) || n < 0 || n > 255)) {
          return null;
        }
        const [a, b, c, d] = quad as [number, number, number, number];
        groups.push((a << 8) | b, (c << 8) | d);
        continue;
      }
      if (!/^[0-9a-f]{1,4}$/.test(piece)) {
        return null;
      }
      groups.push(Number.parseInt(piece, 16));
    }
    return groups;
  };
  const headGroups = parse(head);
  const tailGroups = parse(tail);
  if (headGroups === null || tailGroups === null) {
    return null;
  }
  if (gap === -1) {
    return headGroups.length === 8 ? headGroups : null;
  }
  const fill = 8 - headGroups.length - tailGroups.length;
  if (fill < 1) {
    return null; // `::` must stand for at least one zero group
  }
  return [...headGroups, ...Array<number>(fill).fill(0), ...tailGroups];
}

/**
 * The IPv4 address a v6 literal embeds, when its shape says "this IS a v4 address": v4-mapped
 * `::ffff:a.b.c.d` / `::ffff:7f00:1`, v4-compatible `::a.b.c.d`, and the NAT64-ish translated
 * `::ffff:0:a.b.c.d` — hex and dotted spellings alike, because the groups are numbers by now.
 */
function embeddedV4(groups: number[]): string | null {
  const [g0, g1, g2, g3, g4, g5, g6, g7] = groups as [
    number,
    number,
    number,
    number,
    number,
    number,
    number,
    number,
  ];
  if (g0 !== 0 || g1 !== 0 || g2 !== 0 || g3 !== 0) {
    return null;
  }
  const prefix =
    (g4 === 0 && g5 === 0xffff) || // v4-mapped ::ffff:0:0/96
    (g4 === 0xffff && g5 === 0) || // v4-translated ::ffff:0:0:0/96 (the NAT64-ish shape)
    (g4 === 0 && g5 === 0 && (g6 !== 0 || g7 > 1)); // v4-compatible ::/96 (not :: or ::1)
  if (!prefix) {
    return null;
  }
  return `${g6 >> 8}.${g6 & 0xff}.${g7 >> 8}.${g7 & 0xff}`;
}

/** The v6 ranges that are not the public internet (including every v4-embedded form). */
function isPrivateV6(address: string): boolean {
  const value = address.toLowerCase().split("%")[0] ?? "";
  const groups = ipv6Groups(value);
  if (groups === null) {
    return true; // unparseable is not provably public — refuse
  }
  const [g0] = groups as [number];
  if (groups.every((g, i) => (i === 7 ? g <= 1 : g === 0))) {
    return true; // unspecified (::) · loopback (::1), whatever spelling they arrived in
  }
  // v4-mapped / v4-compatible / v4-translated — hex or dotted spelling: judge the EMBEDDED v4
  // address by the v4 rules (`[::ffff:7f00:1]` is 127.0.0.1 to the socket layer).
  const mapped = embeddedV4(groups);
  if (mapped !== null) {
    return isPrivateV4(mapped);
  }
  const [, g1, g2] = groups as [number, number, number];
  if (g0 === 0x0064 && g1 === 0xff9b) {
    // NAT64: 64:ff9b::/96 (the well-known prefix) and 64:ff9b:1::/48 (local-use). Both are v4
    // space wearing a v6 address — a translator on the path turns them back into an IPv4
    // connection, which is the whole v4 private range reachable through a shape the v4 rules
    // above never see. Refused entire, translator or not.
    return groups.slice(2, 6).every((g) => g === 0) || g2 === 0x0001;
  }
  if ((g0 & 0xffc0) === 0xfe80) {
    return true; // link-local fe80::/10
  }
  if ((g0 & 0xfe00) === 0xfc00) {
    return true; // unique-local fc00::/7
  }
  if (g0 >= 0xff00) {
    return true; // multicast
  }
  return false;
}

export type AddressLookup = (hostname: string) => Promise<{ address: string; family: number }[]>;

const dnsLookup: AddressLookup = async (hostname) => await lookup(hostname, { all: true });

/** A URL that passed the guard, carrying the exact addresses the guard proved public. */
export interface VettedUrl {
  url: URL;
  /** What this hostname resolved to at check time — and the ONLY addresses the fetch dials. */
  addresses: { address: string; family: number }[];
}

/**
 * Resolve a member-supplied URL and refuse it unless EVERY address it resolves to is on the
 * public internet. Every address, not the first: a hostname that answers with one public and
 * one loopback address is a rebinding attempt, and picking the good one would be picking the
 * attacker's other one on the next lookup.
 *
 * The vetted addresses come BACK rather than being thrown away, because the connection is then
 * made to exactly them (see [`dial`]) — a second resolution between this check and the socket
 * is what a rebinding attack is, and there is no window left for it if nothing resolves twice.
 */
export async function assertPublicHttpsUrl(
  raw: string,
  resolve: AddressLookup = dnsLookup,
): Promise<VettedUrl> {
  let url: URL;
  try {
    url = new URL(raw.trim());
  } catch {
    throw new McpFetchError("that is not a URL");
  }
  if (url.protocol !== "https:") {
    throw new McpFetchError("the URL must be https");
  }
  if (url.username !== "" || url.password !== "") {
    throw new McpFetchError("the URL must not carry credentials");
  }
  // A bracketed v6 literal arrives with its brackets on `hostname`-adjacent forms; strip them
  // so the range test sees the address itself.
  const hostname = url.hostname.replace(/^\[|\]$/g, "");
  let addresses: { address: string; family: number }[];
  try {
    addresses = await resolve(hostname);
  } catch {
    throw new McpFetchError("that host does not resolve");
  }
  if (addresses.length === 0) {
    throw new McpFetchError("that host does not resolve");
  }
  for (const entry of addresses) {
    const isV4 = entry.family === 4 || isIPv4(entry.address);
    if (isV4 ? isPrivateV4(entry.address) : isPrivateV6(entry.address)) {
      throw new McpFetchError("that host is on a private network — this server will not fetch it");
    }
  }
  return {
    url,
    // Normalized: a resolver that answers without a family tag still has to be dialable, and the
    // socket layer needs the number.
    addresses: addresses.map((entry) => ({
      address: entry.address,
      family:
        entry.family === 4 || entry.family === 6 ? entry.family : isIPv4(entry.address) ? 4 : 6,
    })),
  };
}

// ── The fetches ─────────────────────────────────────────────────────────────────────────────

/** Read a response body under the document cap, cancelling the stream the moment it is over. */
async function readCapped(response: Response): Promise<string> {
  const declared = response.headers.get("content-length");
  if (declared !== null && Number(declared) > MAX_SERVER_JSON_BYTES) {
    await response.body?.cancel();
    throw new McpFetchError("that document is too large to be a server.json");
  }
  const body = response.body;
  if (body === null) {
    return "";
  }
  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    total += value.byteLength;
    if (total > MAX_SERVER_JSON_BYTES) {
      await reader.cancel();
      throw new McpFetchError("that document is too large to be a server.json");
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  // STRICT decode — a lossy replacement char here would hand the validator different bytes than
  // the endpoint served, and the gate downstream refuses invalid UTF-8 outright anyway.
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new McpFetchError("that document is not valid UTF-8");
  }
}

/** JSON, or something close enough that a JSON body is plausible. An HTML page is not. */
function assertJsonish(response: Response): void {
  const type = (response.headers.get("content-type") ?? "").toLowerCase();
  if (type === "") {
    return; // unstated — the parse decides
  }
  const jsonish = type.includes("json") || type.startsWith("text/plain");
  if (!jsonish) {
    throw new McpFetchError(`that URL served ${type.split(";")[0]}, not a JSON document`);
  }
}

/** What one dial sends, when it is not the plain GET the import arms make. */
export interface DialRequest {
  method?: "GET" | "POST";
  /** Extra request headers — merged over the defaults, never replacing `accept-encoding`. */
  headers?: Record<string, string>;
  /** A request body, for the POST arm. Sent as-is; the caller states its content type. */
  body?: string;
  /** How long the whole exchange may take. */
  timeoutMs?: number;
}

/**
 * THE CONNECTION ITSELF, made to the addresses the guard vetted and to no others.
 *
 * `fetch` resolves the hostname a SECOND time, inside itself, which is the rebinding window the
 * guard could never close: an attacker's DNS answers public on the check and 169.254.169.254 on
 * the connection. `https.request` takes a `lookup`, so the socket layer asks US for the address
 * and gets back exactly what was proved public a moment earlier. Nothing resolves twice.
 *
 * TLS is unaffected: the hostname still rides as SNI and the certificate is still checked
 * against it — pinning the address is not the same as connecting to an IP.
 *
 * The IncomingMessage is handed back as an ordinary `Response` so the status, content-type and
 * body-cap rules below stay one implementation.
 */
function dial(vetted: VettedUrl, options: DialRequest = {}): Promise<Response> {
  const { url, addresses } = vetted;
  const hostname = url.hostname.replace(/^\[|\]$/g, "");
  const body = options.body;
  return new Promise<Response>((resolve, reject) => {
    const request = httpsRequest(
      {
        hostname,
        port: url.port === "" ? 443 : Number(url.port),
        path: `${url.pathname}${url.search}`,
        method: options.method ?? "GET",
        // `identity` because this client does not decompress: a body that arrived compressed is
        // refused below rather than handed to the validator as bytes it cannot read.
        headers: {
          accept: "application/json",
          ...options.headers,
          "accept-encoding": "identity",
          ...(body === undefined
            ? {}
            : { "content-length": String(Buffer.byteLength(body, "utf8")) }),
        },
        // No pooling: every request gets its own socket, so no earlier connection to this host
        // can carry this one.
        agent: false,
        signal: AbortSignal.timeout(options.timeoutMs ?? FETCH_TIMEOUT_MS),
        lookup: (_host, lookupOptions, callback) => {
          const wanted =
            lookupOptions.family === 4 || lookupOptions.family === 6
              ? addresses.filter((entry) => entry.family === lookupOptions.family)
              : addresses;
          if (wanted.length === 0 || wanted[0] === undefined) {
            callback(new Error("no vetted address"), "", 0);
            return;
          }
          if (lookupOptions.all === true) {
            callback(null, wanted as never, 0);
            return;
          }
          callback(null, wanted[0].address, wanted[0].family);
        },
      },
      (message) => {
        const status = message.statusCode ?? 0;
        const headers = new Headers();
        for (const [name, value] of Object.entries(message.headers)) {
          try {
            headers.set(name, Array.isArray(value) ? value.join(", ") : (value ?? ""));
          } catch {
            // A header this runtime will not model is not one any rule below reads.
          }
        }
        // A body-less status cannot be given a body, so it is answered as the empty document it
        // is — the validator then refuses it for what it is rather than for how it arrived.
        if (status === 204 || status === 205 || status === 304 || status < 200) {
          message.destroy();
          resolve(new Response(null, { status: status < 200 ? 500 : status, headers }));
          return;
        }
        resolve(
          new Response(Readable.toWeb(message) as ReadableStream<Uint8Array>, { status, headers }),
        );
      },
    );
    request.on("error", reject);
    if (body !== undefined) {
      request.write(body);
    }
    request.end();
  });
}

/**
 * The SAME guarded connection, opened by a caller that is not fetching a document: one request to
 * a vetted address, with its own method, headers, body and clock. Everything the import arms rely
 * on holds here too — https only, every resolved address proved public, the socket dialled at
 * exactly those addresses, no redirect followed, no connection reuse — because it is one
 * implementation, not a second one with the same intentions.
 *
 * The `Response` comes back whole (status, headers, an unread body stream) so the CALLER decides
 * what a status means. A probe's rules are not a fetch's: a 401 is a healthy answer to it and an
 * error to the import page.
 */
export async function guardedRequest(vetted: VettedUrl, options: DialRequest): Promise<Response> {
  return await dial(vetted, options);
}

/**
 * Read at most `cap` bytes of a response body, then stop reading — for callers that need the text
 * whatever it turns out to be (a probe reads answers that are not documents). Over-long bodies are
 * TRUNCATED rather than refused: the first kilobytes are what any classification reads, and a
 * hostile endpoint must not be able to hold the socket open by streaming forever.
 */
export async function readAtMost(response: Response, cap: number): Promise<string> {
  const body = response.body;
  if (body === null) {
    return "";
  }
  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      chunks.push(value);
      total += value.byteLength;
      if (total >= cap) {
        await reader.cancel();
        break;
      }
    }
  } catch {
    // A body that stops mid-stream is read as far as it got — what arrived is still evidence.
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  // LOSSY on purpose, unlike the document fetch: this text is classified and thrown away, never
  // stored or hashed, so a stray byte must not turn a readable answer into an exception.
  return new TextDecoder("utf-8").decode(bytes.subarray(0, cap));
}

async function httpGet(vetted: VettedUrl): Promise<FetchedDocument> {
  const url = vetted.url.toString();
  let response: Response;
  try {
    response = await dial(vetted);
  } catch {
    throw new McpFetchError("that fetch did not complete");
  }
  if (response.status >= 300 && response.status < 400) {
    // A 3xx is off-script: the guard checked THIS host, and following a redirect would hand the
    // fetch to one it never saw.
    await response.body?.cancel();
    throw new McpFetchError("that URL redirects — give the address it redirects to");
  }
  if (!response.ok) {
    await response.body?.cancel();
    throw new McpFetchError(`that URL answered ${response.status}`);
  }
  const encoding = (response.headers.get("content-encoding") ?? "").toLowerCase();
  if (encoding !== "" && encoding !== "identity") {
    await response.body?.cancel();
    throw new McpFetchError(`that URL served ${encoding} bytes, not a JSON document`);
  }
  assertJsonish(response);
  return { text: await readCapped(response), url };
}

/** The default production fetcher: SSRF-guarded, address-pinned, capped, redirect-refusing. */
export const httpFetchServerJson: ServerJsonFetcher = async (url) => {
  return await httpGet(await assertPublicHttpsUrl(url));
};

/**
 * Fetch one named server's latest version from the official registry. The name's slash MUST be
 * percent-encoded — it is one path SEGMENT, not two.
 */
export async function fetchRegistryServer(
  name: string,
  fetcher: ServerJsonFetcher = httpFetchServerJson,
): Promise<FetchedDocument> {
  const token = name.trim();
  if (token.length === 0 || token.length > 200) {
    throw new McpFetchError("that is not a registry server name");
  }
  return await fetcher(
    `${REGISTRY_BASE}/v0.1/servers/${encodeURIComponent(token)}/versions/latest`,
  );
}

/**
 * Where the page's three CUSTOM arms get their bytes. Only two of them reach the network — a
 * paste never leaves the process. (The picker is not here at all: a built-in row's document is
 * committed data the page already holds, so choosing one asks this tier nothing.)
 */
export type McpSourceKind = "registry" | "url" | "paste";

/** The two arms that make an outbound request — the belt and the SSRF guard are theirs alone. */
export function fetchesUpstream(kind: McpSourceKind): boolean {
  return kind === "registry" || kind === "url";
}

export interface McpSource {
  kind: McpSourceKind;
  value: string;
}

/**
 * The ONE door the custom arms use, so the three differ in exactly one place. The fetcher is a
 * parameter (the upstream importer's pattern) — production dials the network, tests hand back
 * bytes, and the paste arm calls it not at all.
 */
export async function loadServerDocument(
  source: McpSource,
  fetcher: ServerJsonFetcher = httpFetchServerJson,
): Promise<FetchedDocument> {
  if (source.kind === "paste") {
    return { text: source.value, url: "" };
  }
  if (source.kind === "registry") {
    return await fetchRegistryServer(source.value, fetcher);
  }
  return await fetcher(source.value);
}

/**
 * The registry answers `{ server, _meta }` for a single server and `{ servers: [...] }` for a
 * list; a raw URL or a paste is the bare document. Unwrap to the SERVER document either way —
 * that, canonicalized, is what the bundle stores.
 */
export function unwrapServerDocument(text: string): Record<string, unknown> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return null;
  }
  // A document nested past the gate's depth cap passes through UNTOUCHED: `JSON.stringify` in
  // the canonicalizer below is recursive, so re-serializing it would blow the stack — and the
  // gate refuses such a document with its typed answer anyway, which this hand-off preserves.
  if (nestsTooDeep(parsed)) {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return null;
  }
  const record = parsed as Record<string, unknown>;
  const inner = record.server;
  if (typeof inner === "object" && inner !== null && !Array.isArray(inner)) {
    return inner as Record<string, unknown>;
  }
  return record;
}

/**
 * The bytes an MCP bundle stores: the server document, pretty-printed with a trailing newline.
 * Canonicalizing here (rather than storing whatever arrived) is what makes the registry
 * envelope and a hand-pasted document converge on the same bundle when they say the same
 * thing — the version id is the hash of these bytes.
 */
export function canonicalServerJson(document: Record<string, unknown>): string {
  return `${JSON.stringify(document, null, 2)}\n`;
}
