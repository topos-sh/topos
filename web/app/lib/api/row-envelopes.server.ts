/**
 * The member-lane ROW-OP envelopes — the `JsonEnvelope` bodies every naturally-idempotent row op
 * answers (follow/unfollow/exclude, channel join/leave, curation place/unplace, protect, notices ack,
 * invitation). They sit alongside the transport-fault family in `wire.server.ts` (which this reuses
 * for the uniform 404 + the 500), matching the vault's `wire::map` shapes field-for-field:
 *
 *  - an OK outcome → a 200 `ok_status_envelope` carrying a `status` string (or `ok_envelope` with a
 *    typed `data`, for the invitation);
 *  - a role/gate refusal → a 200 DENIED carrying a SPECIFIC code — never a 403, because the actor is
 *    an authenticated member (nothing to hide, so the refusal names WHY);
 *  - `member_required`/`unknown_skill`/`unknown_channel` → the uniform 404 (indistinguishable from a
 *    missing credential); any out-of-contract status → the 500.
 *
 * The vault serializes these through `serde`; this tier stringifies the identical field set (the unit
 * suite pins the literals). `receipt`/`error` are OMITTED (never serialized as null) on the OK
 * envelopes, exactly as the vault's `skip_serializing_if` drops them.
 */

import { type NextAction, nextAction } from "./next-actions.server";
import { internalError, uniformNotFound } from "./wire.server";

const WIRE_SCHEMA_VERSION = 1;
const JSON_HEADERS = { "content-type": "application/json" } as const;

/** The `[RequestAccess, ContactAdmin]` recovery actions a DENIED carries (on both envelope + error). */
const DENIED_NEXT_ACTIONS: NextAction[] = [
  nextAction("REQUEST_ACCESS", []),
  nextAction("CONTACT_ADMIN", []),
];

/** A success envelope carrying a typed `data` — no error, no receipt (row ops have no receipt). */
export function okDataEnvelope(command: string, data: unknown): Response {
  return new Response(
    JSON.stringify({
      schema_version: WIRE_SCHEMA_VERSION,
      command,
      ok: true,
      data,
      warnings: [],
      next_actions: [],
    }),
    { status: 200, headers: JSON_HEADERS },
  );
}

/** A success envelope carrying only a `status` string — the naturally-idempotent row ops' answer. */
export function okStatusEnvelope(command: string, status: string): Response {
  return okDataEnvelope(command, { status });
}

/**
 * A DENIED envelope carrying a SPECIFIC code (the `*_ROLE_REQUIRED` / `CHANNEL_BUILTIN` /
 * `SKILL_NOT_ACTIVE` / `BAD_NAME` / `UNKNOWN_CHANNEL` family). HTTP 200 — the flat error rides the
 * access-recovery next actions like every DENIED; `affected` is `{}`, the generation fields
 * omitted. An optional recovery `message` rides `error.context` (the transport faults'
 * spelling) — the refusal names the way back, never a fact about what exists.
 */
export function deniedCodeEnvelope(command: string, code: string, message?: string): Response {
  return new Response(
    JSON.stringify({
      schema_version: WIRE_SCHEMA_VERSION,
      command,
      ok: false,
      data: {},
      warnings: [],
      next_actions: DENIED_NEXT_ACTIONS,
      error: {
        code,
        outcome: "DENIED",
        retryable: false,
        affected: {},
        context: message === undefined ? {} : { message },
        next_actions: DENIED_NEXT_ACTIONS,
      },
    }),
    { status: 200, headers: JSON_HEADERS },
  );
}

/**
 * Map a guarded function's raw status string to its wire response, given the op's OK-status map
 * (status → `data.status` value) and DENIED-code map (status → code). `member_required` /
 * `unknown_skill` / `unknown_channel` fold to the uniform 404 (the vault's `AuthorityError::NotFound`
 * arm); any status outside all three sets is an out-of-contract answer → the 500 (the vault's
 * `unexpected` → Internal). This ONE mapper serves every row op except the invitation (which carries
 * typed data + maps `unknown_channel` to a DENIED, handled in its own route).
 */
export function rowOpResponse(
  command: string,
  status: string,
  ok: Record<string, string>,
  denied: Record<string, string>,
): Response {
  const okStatus = ok[status];
  if (okStatus !== undefined) {
    return okStatusEnvelope(command, okStatus);
  }
  const code = denied[status];
  if (code !== undefined) {
    return deniedCodeEnvelope(command, code);
  }
  if (status === "member_required" || status === "unknown_skill" || status === "unknown_channel") {
    return uniformNotFound();
  }
  return internalError();
}

/**
 * The server clock the route stamps ONCE per request (the client never supplies a wall clock): the
 * RFC-3339 `createdAt` string (SECONDS precision + `Z`, matching the vault's `now_utc`, which writes
 * no millis) and `nowMs` in epoch milliseconds. One clock, so two writes in one request agree.
 */
export function nowUtc(): { createdAt: string; nowMs: number } {
  const nowMs = Date.now();
  const createdAt = new Date(nowMs).toISOString().replace(/\.\d{3}Z$/, "Z");
  return { createdAt, nowMs };
}
