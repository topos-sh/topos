/**
 * The one result shape every vault call returns. Failure messages are FIXED strings chosen from
 * the table below — a URL or a credential must never appear in any message. A unit test pins this.
 *
 * Every custody call this tier makes is a READ, and the vault signals a read failure with a
 * non-2xx status — so one mapper, `failureFromResponse`, dispatches on the response status; the
 * two callers that never got a response at all (a dead socket, an aborted oversized stream)
 * build their failure directly.
 */
export type PlaneFailureKind =
  | "not_found"
  | "rate_limited"
  | "denied"
  | "plane_fault"
  | "unreachable"
  | "too_large";

export interface PlaneFailure {
  ok: false;
  kind: PlaneFailureKind;
  /** A stable machine code, when one is known (the outcome discriminant, or an envelope code). */
  code?: string;
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
