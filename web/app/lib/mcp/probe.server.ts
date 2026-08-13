import type { PublishActor } from "@/lib/db/queries.custody.server";
import { recordMcpProbe } from "@/lib/db/queries.mcp.server";
import {
  type AddressLookup,
  assertPublicHttpsUrl,
  guardedRequest,
  McpFetchError,
  readAtMost,
  type VettedUrl,
} from "@/lib/mcp/fetch.server";
import {
  type McpProbeOutcome,
  PROBE_DETAIL_PRIVATE,
  PROBE_DETAIL_UNRESOLVED,
} from "@/lib/mcp/probe-state";

/**
 * THE ADVISORY PROBE — one question, asked once, after a publish has already landed.
 *
 * A publish records what a workspace now holds. It says nothing about whether anything answers at
 * the address inside those bytes, and a catalog that shows an address with no idea whether it is
 * alive is a catalog people learn not to trust. So the plane asks: it opens ONE request to the
 * endpoint the document names, reads a bounded answer, writes down what it saw, and stops.
 *
 * THREE PROPERTIES MAKE IT SAFE TO RUN AT ALL:
 *
 *  1. **It is never a gate.** It runs strictly AFTER the publish transaction commits, its result
 *     goes into a table nothing reads to decide anything, and every failure path — a hostile
 *     endpoint, a database that will not take the row, a bug in this file — ends in the publish
 *     standing exactly as it did. `probeAndRecord` cannot throw.
 *  2. **It reuses the guarded fetch**, so an endpoint address supplied by a member cannot make
 *     this process reach into its own network: https only, every resolved address proved public,
 *     the socket dialled at exactly the addresses that were vetted, no redirect followed, no
 *     connection reuse (app/lib/mcp/fetch.server.ts).
 *  3. **It never records a verdict it did not earn.** An address this plane cannot reach or vet is
 *     `not_verifiable` — a NEUTRAL state, because an internal server is a first-class thing to
 *     share and "we could not reach it from the cloud" is a fact about the cloud. WHICH fact it is
 *     rides in the detail (a private network, or a name that does not resolve here), because those
 *     two point a reader at different things. Anything that goes wrong on OUR side records nothing
 *     at all, and the version reads "not checked yet".
 *
 * THE CLASSIFICATION RULES, and why each one is what it is (reimplemented from the MIT-licensed
 * TypeScript MCP SDK's client behaviour — the rules, not its code):
 *
 *  · **401/403 outranks the body.** A server that demands a sign-in is a HEALTHY server: it
 *    answered, and it answered the way an OAuth-protected MCP endpoint is supposed to. Its body
 *    will not parse as an initialize result, and reading that as a fault would mark every properly
 *    protected server broken. The spec's own signal is a `WWW-Authenticate` challenge; a 401 that
 *    omits it is still a server asking for credentials, so the outcome is the same word and the
 *    detail records which of the two arrived.
 *  · **Silence is an outage, never a protocol verdict.** A timeout, a refused connection, a TLS
 *    failure — none of them says anything about what the server is; they say it is not answering.
 *  · **5xx is never protocol evidence.** Neither is 429. A server having a bad minute has not
 *    stopped being an MCP server.
 *  · **A redirect is not followed.** The guard vetted THIS host; chasing a 30x would hand the
 *    request to one it never saw. It is recorded as not-responding with the status in the detail.
 *  · **Anything 2xx is read.** A correlated JSON-RPC answer to the initialize we sent — result or
 *    error — means an MCP server is on the other end and talking. A 200 carrying something else
 *    (a login page, an HTML error) is a live host that is not this protocol, and says so in the
 *    detail rather than in a state of its own: the catalog's vocabulary is four words wide.
 */

/** The protocol revision the probe announces. The `verify` verb owns the pinned connect sequence;
 *  this asks one question and never follows up, so it needs the version and nothing else. */
const PROTOCOL_VERSION = "2025-11-25";

/** The correlation id every answer must carry back. One request, one id, no ambiguity. */
const REQUEST_ID = 1;

/** A probe gets a short clock: nobody is waiting on it, but a socket held open forever is a leak. */
const PROBE_TIMEOUT_MS = 8_000;

/** How much of an answer is read before the rest is dropped. A JSON-RPC initialize result is a
 *  few hundred bytes; everything past this cap is a body arguing with its own content-length. */
const PROBE_BODY_CAP = 64 * 1024;

/** What one probe concluded, ready for the row. */
export interface ProbeVerdict {
  outcome: McpProbeOutcome;
  /** One short machine-written note — a status, a reason. Never a body, never a header value. */
  detail: string | null;
}

/** The initialize request, verbatim — the one thing this probe ever sends. */
export function initializeRequestBody(): string {
  return JSON.stringify({
    jsonrpc: "2.0",
    id: REQUEST_ID,
    method: "initialize",
    params: {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "topos", version: "1" },
    },
  });
}

/**
 * The JSON payloads an answer carries, whichever way it was framed: a plain JSON body, or an SSE
 * stream whose `data:` lines carry them. The SSE reader handles what the format actually allows —
 * comment lines, `event:`/`id:` fields, data spread over several lines, several events in one
 * body — and stops at whatever the read cap delivered, since a truncated last event simply fails
 * to parse.
 */
export function jsonPayloadsOf(body: string, contentType: string | null): unknown[] {
  const isEventStream = (contentType ?? "").toLowerCase().includes("text/event-stream");
  if (!isEventStream) {
    try {
      return [JSON.parse(body)];
    } catch {
      return [];
    }
  }
  const payloads: unknown[] = [];
  let data: string[] = [];
  const flush = () => {
    if (data.length > 0) {
      try {
        payloads.push(JSON.parse(data.join("\n")));
      } catch {
        // A partial or non-JSON event is no answer; the next one may be.
      }
      data = [];
    }
  };
  for (const rawLine of body.split(/\r\n|\r|\n/)) {
    if (rawLine === "") {
      flush();
      continue;
    }
    if (rawLine.startsWith(":")) {
      continue; // a comment — the keep-alive spelling
    }
    const colon = rawLine.indexOf(":");
    const field = colon === -1 ? rawLine : rawLine.slice(0, colon);
    let value = colon === -1 ? "" : rawLine.slice(colon + 1);
    if (value.startsWith(" ")) {
      value = value.slice(1);
    }
    if (field === "data") {
      data.push(value);
    }
  }
  flush();
  return payloads;
}

/** Is this the answer to the initialize WE sent — same protocol envelope, same id? */
function answersOurInitialize(payload: unknown): { ok: boolean; error: number | null } {
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) {
    return { ok: false, error: null };
  }
  const message = payload as Record<string, unknown>;
  if (message.jsonrpc !== "2.0" || message.id !== REQUEST_ID) {
    return { ok: false, error: null };
  }
  if (typeof message.result === "object" && message.result !== null) {
    return { ok: true, error: null };
  }
  if (typeof message.error === "object" && message.error !== null) {
    const code = (message.error as Record<string, unknown>).code;
    return { ok: true, error: typeof code === "number" ? code : null };
  }
  return { ok: false, error: null };
}

/** The status/body rules, as a pure function — the whole classification, testable without a socket. */
export function classifyProbeAnswer(input: {
  status: number;
  wwwAuthenticate: string | null;
  contentType: string | null;
  body: string;
}): ProbeVerdict {
  const { status } = input;
  if (status === 401 || status === 403) {
    return {
      outcome: "sign_in_required",
      detail:
        input.wwwAuthenticate === null
          ? `${status}, no challenge header`
          : `${status} with a sign-in challenge`,
    };
  }
  if (status >= 300 && status < 400) {
    return { outcome: "not_responding", detail: `redirected (${status}), not followed` };
  }
  if (status >= 200 && status < 300) {
    for (const payload of jsonPayloadsOf(input.body, input.contentType)) {
      const answer = answersOurInitialize(payload);
      if (answer.ok) {
        return {
          outcome: "responding",
          detail: answer.error === null ? null : `protocol error ${answer.error}`,
        };
      }
    }
    return { outcome: "not_responding", detail: `answered ${status}, not as an MCP server` };
  }
  return { outcome: "not_responding", detail: `answered ${status}` };
}

/** The seam: production dials, tests answer. Mirrors the import arms' `ServerJsonFetcher`. */
export type ProbeTransport = (vetted: VettedUrl) => Promise<Response>;

const httpsProbe: ProbeTransport = async (vetted) =>
  await guardedRequest(vetted, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      // Both framings are legal answers to one initialize, so both are asked for.
      accept: "application/json, text/event-stream",
      "mcp-protocol-version": PROTOCOL_VERSION,
    },
    body: initializeRequestBody(),
    timeoutMs: PROBE_TIMEOUT_MS,
  });

/**
 * Ask one endpoint. Returns the verdict, or NULL when this plane learned nothing it may write
 * down — which is not the same as learning that something is wrong.
 */
export async function probeEndpoint(
  url: string,
  transport: ProbeTransport = httpsProbe,
  /** The guard's resolver — production asks DNS; a test hands back addresses, or throws. */
  resolve?: AddressLookup,
): Promise<ProbeVerdict | null> {
  let vetted: VettedUrl;
  try {
    vetted = await assertPublicHttpsUrl(url, resolve);
  } catch (error) {
    if (error instanceof McpFetchError) {
      // The guard's answer is "this plane cannot reach or vet that address" — a NEUTRAL state,
      // deliberately: an internal server is a first-class thing for a team to share. But WHICH of
      // the two it is decides what the reader does next, so the reason is carried through rather
      // than folded into one word: a private network is somebody's own network working as
      // intended, and a name that does not resolve here is a typo or a name that lives inside
      // one. A refusal with no reason is about the URL's shape, and says neither.
      return {
        outcome: "not_verifiable",
        detail:
          error.reason === "private"
            ? PROBE_DETAIL_PRIVATE
            : error.reason === "unresolved"
              ? PROBE_DETAIL_UNRESOLVED
              : null,
      };
    }
    return null;
  }
  let response: Response;
  try {
    response = await transport(vetted);
  } catch (error) {
    const name = error instanceof Error ? error.name : "";
    return {
      outcome: "not_responding",
      detail: name === "TimeoutError" || name === "AbortError" ? "no answer in time" : "no answer",
    };
  }
  try {
    const body = await readAtMost(response, PROBE_BODY_CAP);
    return classifyProbeAnswer({
      status: response.status,
      wwwAuthenticate: response.headers.get("www-authenticate"),
      contentType: response.headers.get("content-type"),
      body,
    });
  } catch {
    return { outcome: "not_responding", detail: "the answer did not finish arriving" };
  }
}

/** What a caller hands the probe once its publish has landed. */
export interface PublishProbeTarget {
  bundleId: string;
  versionId: string;
  /** The endpoint the published document places — null for a package-only bundle. */
  endpoint: string | null;
}

/**
 * Probe one just-published version and record what came back. NEVER THROWS and never rejects: it
 * is called after the work that matters is already durable, so every failure it can meet — a
 * hostile endpoint, an unwritable row, a bug in this file — must end with the publish standing and
 * the version reading "not checked yet".
 *
 * A PACKAGE-ONLY bundle is not probed: there is no address for this plane to ask. Nothing is
 * recorded for it, which is the honest answer — whether a package runs is a question its own
 * machine answers, not one the cloud can.
 */
export async function probeAndRecord(
  actor: PublishActor,
  target: PublishProbeTarget,
  transport: ProbeTransport = httpsProbe,
): Promise<ProbeVerdict | null> {
  try {
    if (target.endpoint === null) {
      return null;
    }
    const verdict = await probeEndpoint(target.endpoint, transport);
    if (verdict === null) {
      return null;
    }
    await recordMcpProbe(actor, {
      bundleId: target.bundleId,
      versionId: target.versionId,
      outcome: verdict.outcome,
      detail: verdict.detail,
    });
    return verdict;
  } catch {
    return null;
  }
}

/**
 * Fire the probe and DO NOT WAIT. The publish's answer is already decided by the time this is
 * called, so making the caller hold its response open for somebody else's server would be paying
 * for a report with the latency of the act that produced it. The floating promise is deliberate
 * and cannot reject (see above); the void marks it as such.
 */
export function schedulePublishProbe(actor: PublishActor, target: PublishProbeTarget): void {
  void probeAndRecord(actor, target);
}
