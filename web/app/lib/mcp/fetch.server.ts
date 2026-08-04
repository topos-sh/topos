import { lookup } from "node:dns/promises";
import { isIPv4 } from "node:net";
import { MAX_SERVER_JSON_BYTES } from "@/lib/mcp/validate.server";

/**
 * THE TWO SERVER-SIDE FETCHES the MCP import page can make, and the guard that makes the
 * second one safe.
 *
 * A pasted document needs no fetch at all — that is the path an internal server takes. The
 * other two reach out from THIS process, which sits inside the deployment's own network, so a
 * URL a member types is an SSRF primitive unless it is checked: the guard below refuses
 * anything but https, resolves the host itself, and refuses every address that is not on the
 * public internet — loopback, private space, link-local (the cloud metadata endpoint lives
 * there), unique-local v6, and the unspecified/multicast ranges. Redirects are `manual`, so a
 * 3xx cannot walk a checked host into an unchecked one; the size cap and timeout bound what a
 * hostile endpoint can cost.
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

/** The v6 ranges that are not the public internet (including v4-mapped forms). */
function isPrivateV6(address: string): boolean {
  const value = address.toLowerCase().split("%")[0] ?? "";
  if (value === "::" || value === "::1") {
    return true; // unspecified · loopback
  }
  // v4-mapped/compatible: judge the embedded v4 address by the v4 rules.
  const mapped = value.match(/^::(?:ffff:)?(\d+\.\d+\.\d+\.\d+)$/);
  if (mapped?.[1] !== undefined) {
    return isPrivateV4(mapped[1]);
  }
  if (
    value.startsWith("fe8") ||
    value.startsWith("fe9") ||
    value.startsWith("fea") ||
    value.startsWith("feb")
  ) {
    return true; // link-local fe80::/10
  }
  if (value.startsWith("fc") || value.startsWith("fd")) {
    return true; // unique-local fc00::/7
  }
  if (value.startsWith("ff")) {
    return true; // multicast
  }
  return false;
}

export type AddressLookup = (hostname: string) => Promise<{ address: string; family: number }[]>;

const dnsLookup: AddressLookup = async (hostname) => await lookup(hostname, { all: true });

/**
 * Resolve a member-supplied URL and refuse it unless EVERY address it resolves to is on the
 * public internet. Every address, not the first: a hostname that answers with one public and
 * one loopback address is a rebinding attempt, and picking the good one would be picking the
 * attacker's other one on the next lookup.
 *
 * This does not close the TOCTOU window between the check and the connection — nothing at this
 * layer can, short of dialing the address directly — but it refuses the whole class of
 * "point it at 169.254.169.254" that a text field otherwise invites.
 */
export async function assertPublicHttpsUrl(
  raw: string,
  resolve: AddressLookup = dnsLookup,
): Promise<URL> {
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
  return url;
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
  return new TextDecoder().decode(bytes);
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

async function httpGet(url: string): Promise<FetchedDocument> {
  let response: Response;
  try {
    response = await fetch(url, {
      // A 3xx is off-script: the guard checked THIS host, and following a redirect would hand
      // the fetch to one it never saw.
      redirect: "manual",
      headers: { accept: "application/json" },
      signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
    });
  } catch {
    throw new McpFetchError("that fetch did not complete");
  }
  if (response.status >= 300 && response.status < 400) {
    await response.body?.cancel();
    throw new McpFetchError("that URL redirects — give the address it redirects to");
  }
  if (!response.ok) {
    await response.body?.cancel();
    throw new McpFetchError(`that URL answered ${response.status}`);
  }
  assertJsonish(response);
  return { text: await readCapped(response), url };
}

/** The default production fetcher: SSRF-guarded, capped, redirect-refusing. */
export const httpFetchServerJson: ServerJsonFetcher = async (url) => {
  await assertPublicHttpsUrl(url);
  return await httpGet(url);
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

/** Where the page's three arms get their bytes. A paste never leaves the process. */
export type McpSourceKind = "registry" | "url" | "paste";

export interface McpSource {
  kind: McpSourceKind;
  value: string;
}

/**
 * The ONE door the import page uses, so the three arms differ in exactly one place. The
 * fetcher is a parameter (the upstream importer's pattern) — production dials the network,
 * tests hand back bytes, and the paste arm calls it not at all.
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
