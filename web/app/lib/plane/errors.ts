/**
 * The one result shape every vault call returns. Failure messages are FIXED strings chosen from
 * the table below — a URL or a credential must never appear in any message. A unit test pins this.
 *
 * The vault's internal lane answers WRITES `200`-for-all-outcomes: the decision rides the JSON
 * body's `outcome` discriminant (`{outcome: "denied", reason}` etc.), not the HTTP status. So the
 * write mappers (`reviewFailureFrom`, `revertFailureFrom`, `createFailureFrom`) dispatch on the
 * parsed OUTCOME, while READ failures — which the vault still signals with a non-2xx status —
 * dispatch on the response status through `failureFromResponse`.
 */
export type PlaneFailureKind =
  | "not_found"
  | "rate_limited"
  | "denied"
  | "create_denied"
  | "review_denied"
  | "review_conflict"
  | "revert_denied"
  | "revert_conflict"
  | "plane_fault"
  | "unreachable"
  | "too_large";

export interface PlaneFailure {
  ok: false;
  kind: PlaneFailureKind;
  /** A stable machine code, when one is known (the outcome discriminant, or an envelope code). */
  code?: string;
  /**
   * The vault's static, typed denial reason (one of its fixed authority strings — the same bytes
   * the device lane emits). Always one of the vault's fixed strings, never free text.
   */
  reason?: string;
  retryable: boolean;
  message: string;
  status?: number;
}

export type PlaneResult<T> = { ok: true; data: T; status?: number } | PlaneFailure;

/**
 * Fixed, credential-free copy per kind. The 404 copy is deliberately "not found" — the vault
 * answers 404 for missing AND unauthorized alike (its posture), and this tier must not distort
 * that into an access claim. 403 is an ops/config statement, never a user-permissions claim.
 */
const MESSAGES: Record<PlaneFailureKind, string> = {
  not_found: "not found",
  rate_limited: "the server is rate limiting requests — try again shortly",
  denied: "the server declined this request — a deployment configuration fault",
  create_denied: "the server declined this workspace creation",
  review_denied: "the server declined this review decision",
  review_conflict: "current moved — this decision was refused as stale",
  revert_denied: "the server declined this roll back",
  revert_conflict: "current moved — this roll back was refused as stale",
  plane_fault: "the server reported an internal fault",
  unreachable: "couldn't reach the server",
  too_large: "this object exceeds the size cap and wasn't fetched",
};

/** Best-effort extraction of a vault error envelope's stable code + retryability from a body. */
function envelopeError(body: unknown): { code?: string; retryable?: boolean } {
  if (typeof body !== "object" || body === null || !("error" in body)) {
    return {};
  }
  const error = (body as { error: unknown }).error;
  if (typeof error !== "object" || error === null) {
    return {};
  }
  const code = "code" in error ? (error as { code: unknown }).code : undefined;
  const retryable = "retryable" in error ? (error as { retryable: unknown }).retryable : undefined;
  return {
    code: typeof code === "string" ? code : undefined,
    retryable: typeof retryable === "boolean" ? retryable : undefined,
  };
}

function kindForStatus(status: number): PlaneFailureKind {
  if (status === 404) {
    return "not_found";
  }
  if (status === 429) {
    return "rate_limited";
  }
  if (status === 401 || status === 403) {
    return "denied";
  }
  return "plane_fault";
}

/**
 * Map a non-2xx vault READ response (plus its already-parsed body, when JSON) to a PlaneFailure.
 * A `Retry-After` header always marks the failure retryable.
 */
export function failureFromResponse(response: Response, body: unknown): PlaneFailure {
  const status = response.status;
  const kind = kindForStatus(status);
  const { code, retryable } = envelopeError(body);
  const retryAfter = response.headers.get("retry-after") !== null;
  return {
    ok: false,
    kind,
    code,
    retryable: retryAfter || kind === "rate_limited" || (retryable ?? false),
    message: MESSAGES[kind],
    status,
  };
}

/** Network-level failure (DNS, refused connection, aborted socket): the vault never answered. */
export function unreachableFailure(): PlaneFailure {
  return {
    ok: false,
    kind: "unreachable",
    retryable: true,
    message: MESSAGES.unreachable,
  };
}

/** A streamed object crossed the caller's byte cap and the fetch was aborted. */
export function tooLargeFailure(): PlaneFailure {
  return {
    ok: false,
    kind: "too_large",
    retryable: false,
    message: MESSAGES.too_large,
  };
}

/**
 * Parse a write response's typed OUTCOME body. The internal lane answers writes 200-for-all so
 * the outcome is always in the body; a non-JSON body (a fault page) yields `undefined`, which the
 * mappers treat as a generic denial.
 */
export async function readOutcome<T>(res: Response): Promise<T | undefined> {
  try {
    return (await res.json()) as T;
  } catch {
    return undefined;
  }
}

/** The vault's static create-denial reason for the per-owner cap (rendered specially). */
export const CAP_REASON = "workspace creation limit reached";

/** The shape every write outcome shares: a discriminant + an optional static reason string. */
export interface OutcomeLike {
  outcome: string;
  reason?: string;
}

/** Best-effort extraction of a typed denial's static `reason` string. */
function reasonOf(outcome: OutcomeLike | undefined): string {
  return typeof outcome?.reason === "string" ? outcome.reason : "";
}

/**
 * Map a `POST /internal/v1/workspaces` create outcome to a failure. `denied` carries one of the
 * vault's fixed reasons (the per-owner cap = CAP_REASON, a reused request id). `created`/`replayed`
 * are the caller's success arms, never mapped here.
 */
export function createFailureFrom(outcome: OutcomeLike | undefined): PlaneFailure {
  return {
    ok: false,
    kind: "create_denied",
    code: "create_denied",
    reason: reasonOf(outcome),
    retryable: false,
    message: MESSAGES.create_denied,
  };
}

/**
 * The session-review routes' static `review_denied` reasons, verbatim from the vault — a BYTE
 * contract (the unit test pins these against drift; the vault's e2e pins the same literals on
 * the wire). The review action branches on them to render HONEST per-case copy; anything not
 * listed here degrades to the generic declined copy, never a crash.
 */
export const REVIEW_DENIED_REASONS = {
  /** The acting email holds a plain member seat — deciding needs owner|reviewer. */
  roleGate: "approving or rejecting needs an owner or reviewer seat",
  /** Four-eyes: under review-required the proposer cannot approve their own proposal. */
  fourEyes: "the proposer may not approve their own proposal under review-required",
  /** No open proposal row keyed by this candidate + base (resolved, staled, or never proposed). */
  notOpen: "no open proposal for this candidate and base",
  /** The proposal was already accepted (a re-approve under a NEW request id). */
  alreadyAccepted: "the proposal is already accepted",
} as const;

/**
 * Map a review-decision OUTCOME (approve/reject) to a failure. `conflict` is the stale-base CAS
 * refusal (approve only — the page re-renders a fresh diff); `denied` carries one of the static
 * `review_denied` reasons; `not_found` is the uniform miss. `approved`/`rejected` are the caller's
 * success arms. An absent/unrecognized outcome folds into `review_denied` with no reason.
 */
export function reviewFailureFrom(outcome: OutcomeLike | undefined): PlaneFailure {
  if (outcome?.outcome === "conflict") {
    return {
      ok: false,
      kind: "review_conflict",
      code: "review_conflict",
      retryable: false,
      message: MESSAGES.review_conflict,
    };
  }
  if (outcome?.outcome === "not_found") {
    return {
      ok: false,
      kind: "not_found",
      code: "not_found",
      retryable: false,
      message: MESSAGES.not_found,
    };
  }
  return {
    ok: false,
    kind: "review_denied",
    code: "review_denied",
    reason: reasonOf(outcome),
    retryable: false,
    message: MESSAGES.review_denied,
  };
}

/**
 * The session-revert route's static `revert_denied` reasons, verbatim from the vault — a BYTE
 * contract (the unit test pins these; the vault's e2e pins the same literals on the wire). The
 * revert action branches on them: `roleGate` is the vault's SHARED approve/reject role string
 * (the same op relays it for both verbs, so it will NOT say "roll back" — the web substitutes
 * its own verb-appropriate copy); `opIdReused` means a concurrent duplicate of THIS revert
 * already applied under the same request id (a benign double-click — the action treats it as
 * success). Any OTHER static reason (e.g. a non-accepted target) relays verbatim.
 */
export const REVERT_DENIED_REASONS = {
  /** The acting email holds a plain member seat — rolling back needs owner|reviewer. */
  roleGate: "approving or rejecting needs an owner or reviewer seat",
  /** A concurrent duplicate of this revert already applied (same request id) — benign success. */
  opIdReused: "op id reused with a different request",
} as const;

/**
 * Map a revert OUTCOME to a failure. `conflict` is the stale-generation CAS refusal (the page
 * reloads to the live current); `denied` carries one of the vault's static reasons (the
 * reviewer-role gate, a non-accepted target, a reused request id); `not_found` is the uniform
 * miss. `reverted` is the caller's success arm. An absent/unrecognized outcome folds into
 * `revert_denied` with no reason.
 */
export function revertFailureFrom(outcome: OutcomeLike | undefined): PlaneFailure {
  if (outcome?.outcome === "conflict") {
    return {
      ok: false,
      kind: "revert_conflict",
      code: "revert_conflict",
      retryable: false,
      message: MESSAGES.revert_conflict,
    };
  }
  if (outcome?.outcome === "not_found") {
    return {
      ok: false,
      kind: "not_found",
      code: "not_found",
      retryable: false,
      message: MESSAGES.not_found,
    };
  }
  return {
    ok: false,
    kind: "revert_denied",
    code: "revert_denied",
    reason: reasonOf(outcome),
    retryable: false,
    message: MESSAGES.revert_denied,
  };
}
