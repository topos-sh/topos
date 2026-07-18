/**
 * The device-lane wire envelopes — the transport-fault family every `/api/v1` route answers with,
 * matching the vault's frozen shapes field-for-field (`JsonEnvelope` + flat `WireError`; the
 * committed OpenAPI is the contract, and the unit suite pins these literals against it).
 *
 * The posture, verbatim from the vault: a protocol outcome (OK / DENIED / CONFLICT) is ALWAYS a
 * 200 carrying its envelope; a non-2xx is ONLY a transport/auth fault — 400 for a malformed
 * body/id, 404 for EVERY miss (missing/blank credential, unknown credential, revoked device,
 * unknown workspace, non-member — one indistinguishable body, never a 401/403), 429 from the
 * belt, 500 for a store fault. Nothing here discloses what exists.
 */

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

/** Per-member hot reads (`/me`, `/delivery`, the describes) are never cacheable. */
export const NO_STORE = { "cache-control": "no-store" } as const;

/**
 * Read a request body under a hard byte cap. A declared `Content-Length` over the cap is refused UP
 * FRONT — before the body is read — so the common oversize case never buffers (the memory
 * amplification an unauthenticated caller could otherwise trip before the credential resolve). A
 * chunked body declares no length, so it is still read and then length-checked: nothing over the
 * cap is ever ACCEPTED, though a chunked oversize body is buffered before rejection (bounded by the
 * runtime's own request ceiling). The vault enforces its equivalent 64 KiB enroll-lane cap at the
 * streaming extractor; this is the closest the served routes get without a streaming reader.
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
  const text = await request.text();
  if (text.length > cap) {
    return badRequest(`${what} too large`);
  }
  return text;
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
