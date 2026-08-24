import type { SessionFreshness } from "@/lib/db/queries.sessions.server";

/** The fields the header counts read — every session row structurally satisfies this. */
export interface CountableSession {
  status: "active" | "pending";
  expired: boolean;
  freshness: SessionFreshness;
}

/**
 * The Sessions page header's buckets, and the ONE rule they keep: they PARTITION the rows.
 * Every session lands in exactly one, so the numbers always add up to the list underneath —
 * the header read "2 active sessions · 1 stale" over two rows before, because `active` was the
 * whole live set and `stale` a slice of it, so one machine was counted twice.
 *
 * Each bucket is named by the same word its row's chip carries: a session that is live and has
 * reported inside the staleness window is `active`; one that has not is `stale`; one that has
 * never reported at all is `neverReported`; a credential past the workspace's expiry is
 * `expired`; and one still awaiting an owner's approval is `pending`.
 */
export interface SessionCounts {
  active: number;
  pending: number;
  stale: number;
  neverReported: number;
  expired: number;
}

export function sessionCounts(sessions: readonly CountableSession[]): SessionCounts {
  const counts: SessionCounts = {
    active: 0,
    pending: 0,
    stale: 0,
    neverReported: 0,
    expired: 0,
  };
  for (const session of sessions) {
    if (session.status === "pending") {
      counts.pending += 1;
    } else if (session.expired) {
      // An EXPIRED credential no longer resolves — it is not a live machine, and the row's chip
      // says so instead of a freshness.
      counts.expired += 1;
    } else if (session.freshness === "stale") {
      counts.stale += 1;
    } else if (session.freshness === "never") {
      counts.neverReported += 1;
    } else {
      counts.active += 1;
    }
  }
  return counts;
}
