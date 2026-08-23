import { Buffer } from "node:buffer";
/**
 * The session-lane wire envelopes — the transport-fault family every `/api/v1` route answers with,
 * matching the vault's frozen shapes field-for-field (`JsonEnvelope` + flat `WireError`; the
 * committed OpenAPI is the contract, and the unit suite pins these literals against it).
 *
 * The posture, verbatim from the vault: a protocol outcome (OK / DENIED / CONFLICT) is ALWAYS a
 * 200 carrying its envelope; a non-2xx is ONLY a transport/auth fault — 400 for a malformed
 * body/id, 404 for EVERY miss (missing/blank credential, unknown credential, an ended session,
 * unknown workspace, non-member — one indistinguishable body, never a 401/403), 426 for a client
 * below the version floor, 429 from the belt, 500 for a store fault. Nothing here discloses what
 * exists.
 */

import { SERVER_RELEASE_VERSION } from "@/lib/plane/contract/version";
import { type NextAction, nextAction } from "./next-actions.server";

const WIRE_SCHEMA_VERSION = 1;

interface WireErrorShape {
  code: string;
  outcome: "PERMANENT_FAILURE" | "RETRYABLE_FAILURE";
  retryable: boolean;
  affected: Record<string, never>;
  context: Record<string, unknown>;
  next_actions: NextAction[];
}

function errorEnvelope(command: string, error: WireErrorShape): Record<string, unknown> {
  return {
    schema_version: WIRE_SCHEMA_VERSION,
    command,
    ok: false,
    data: {},
    warnings: [],
    next_actions: error.next_actions,
    error,
  };
}

const JSON_HEADERS = { "content-type": "application/json" } as const;

/** The ONE uniform miss — every auth/existence failure on the lane answers this exact body. */
export function uniformNotFound(): Response {
  return new Response(
    JSON.stringify(
      errorEnvelope("error", {
        code: "NOT_FOUND",
        outcome: "PERMANENT_FAILURE",
        retryable: false,
        affected: {},
        context: { message: "not found" },
        next_actions: [],
      }),
    ),
    { status: 404, headers: JSON_HEADERS },
  );
}

/**
 * A machine token presented where only a person's session may act. Typed on purpose — the
 * caller HOLDS a token (the prefix is client-known, so naming the refusal reveals nothing
 * about whether that token is live), and "not found" would send them debugging the wrong
 * thing. Every write lane answers this; the read lanes that accept tokens never see it.
 */
export function machineTokenRefused(): Response {
  return new Response(
    JSON.stringify(
      errorEnvelope("error", {
        code: "MACHINE_TOKEN_READ_ONLY",
        outcome: "PERMANENT_FAILURE",
        retryable: false,
        affected: {},
        context: {
          message:
            "machine tokens are read-only — this action needs a person's session (run `topos login`)",
        },
        next_actions: [],
      }),
    ),
    { status: 403, headers: JSON_HEADERS },
  );
}

/** A malformed body or identifier — the message names the problem, never an internal detail. */
export function badRequest(message: string): Response {
  return new Response(
    JSON.stringify(
      errorEnvelope("error", {
        code: "BAD_REQUEST",
        outcome: "PERMANENT_FAILURE",
        retryable: false,
        affected: {},
        context: { message },
        next_actions: [],
      }),
    ),
    { status: 400, headers: JSON_HEADERS },
  );
}

/** A store/transport fault — flat and retryable, detail stays server-side (logged by the caller). */
export function internalError(): Response {
  const retry = nextAction("RETRY", []);
  return new Response(
    JSON.stringify(
      errorEnvelope("error", {
        code: "INTERNAL",
        outcome: "RETRYABLE_FAILURE",
        retryable: true,
        affected: {},
        context: { message: "internal store error" },
        next_actions: [retry],
      }),
    ),
    { status: 500, headers: JSON_HEADERS },
  );
}

/** The frozen 429 — `Retry-After` + the RATE_LIMITED envelope, byte-shaped like the vault's. */
export function rateLimited(retryAfterSeconds: number): Response {
  const retry = nextAction("RETRY", []);
  return new Response(
    JSON.stringify(
      errorEnvelope("rate_limited", {
        code: "RATE_LIMITED",
        outcome: "RETRYABLE_FAILURE",
        retryable: true,
        affected: {},
        context: { retry_after_seconds: retryAfterSeconds },
        next_actions: [retry],
      }),
    ),
    {
      status: 429,
      headers: { ...JSON_HEADERS, "retry-after": String(retryAfterSeconds) },
    },
  );
}

/**
 * The version floor's refusal — HTTP 426, the ONE answer a client below the floor gets on every
 * lane path. A dead end, not a fault: the same request refuses until the binary is replaced, so
 * the envelope is permanent, un-retryable, and carries the fix STRUCTURALLY (`SELF_UPDATE`) as
 * well as in prose. Both versions ride in `context` so a client too old to have been taught this
 * status can still read what is required of it.
 *
 * `clientVersion` is the version RE-RENDERED from the numbers the floor parsed (`null` when the
 * caller named none) — never a span of the caller's own header bytes, which are attacker-shaped
 * and would otherwise land in a body this server hands back out.
 *
 * The floor itself rides as a parameter rather than an import: `compat.server.ts` owns that
 * number and reaches for this envelope, so reading it back from there would close a cycle.
 */
export function upgradeRequired(clientVersion: string | null, minCliVersion: string): Response {
  const selfUpdate = nextAction("SELF_UPDATE", ["topos", "self-update"]);
  const who =
    clientVersion === null ? "a client that does not name its version" : `topos ${clientVersion}`;
  return new Response(
    JSON.stringify(
      errorEnvelope("error", {
        code: "CLI_UPDATE_REQUIRED",
        outcome: "PERMANENT_FAILURE",
        retryable: false,
        affected: {},
        context: {
          message:
            `this server no longer speaks to ${who} — it requires topos ` +
            `${minCliVersion} or later; run topos self-update`,
          min_cli_version: minCliVersion,
          server_version: SERVER_RELEASE_VERSION,
        },
        next_actions: [selfUpdate],
      }),
    ),
    { status: 426, headers: JSON_HEADERS },
  );
}

/** Per-member hot reads (`/me`, `/delivery`, the describes) are never cacheable. */
export const NO_STORE = { "cache-control": "no-store" } as const;

/**
 * Read a request body under a hard byte cap. A declared `Content-Length` over the cap is refused UP
 * FRONT — before the body is read — so the common oversize case never buffers (the memory
 * amplification an unauthenticated caller could otherwise trip before the credential resolve). A
 * chunked body declares no length, so the cap is ALSO enforced while the stream is consumed —
 * the read stops and cancels at the first byte past the cap, so an undeclared body can never
 * buffer past it either. The vault enforces its equivalent 64 KiB enroll-lane cap at the
 * streaming extractor; this is the served routes' matching discipline.
 * Returns the body text, or a 400 `Response` to answer directly.
 */
export async function readCappedBody(
  request: Request,
  cap: number,
  what: string,
): Promise<string | Response> {
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    const n = Number(declared);
    if (Number.isFinite(n) && n > cap) {
      return badRequest(`${what} too large`);
    }
  }
  // The declared length is only a HINT — a caller may omit it entirely (chunked transfer), and
  // then a plain `request.text()` would buffer the whole stream before anything could object.
  // So the cap is enforced AS the body arrives: the read is abandoned the moment it is exceeded,
  // and the connection is cancelled rather than drained. Every authenticated lane route also
  // resolves its credential BEFORE reading the body, so this cap bounds what an AUTHENTICATED
  // caller can make the tier buffer — an unauthenticated one buffers nothing.
  const body = request.body;
  if (body === null) {
    return "";
  }
  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    total += value.byteLength;
    if (total > cap) {
      await reader.cancel();
      return badRequest(`${what} too large`);
    }
    chunks.push(value);
  }
  return new TextDecoder().decode(Buffer.concat(chunks));
}

/**
 * Extract the Bearer credential exactly like the vault's edge does: strip a literal `Bearer ` or
 * `bearer ` prefix (those two spellings only — no other casing, no leading whitespace), then trim
 * the remainder; a missing header, wrong scheme, or blank token is `null` — the caller folds it to
 * the uniform 404 (the credential's absence is as undisclosed as its invalidity).
 */
export function bearerToken(request: Request): string | null {
  const raw = request.headers.get("authorization");
  if (raw === null) {
    return null;
  }
  const rest = raw.startsWith("Bearer ")
    ? raw.slice("Bearer ".length)
    : raw.startsWith("bearer ")
      ? raw.slice("bearer ".length)
      : null;
  const token = rest?.trim() ?? "";
  return token === "" ? null : token;
}
