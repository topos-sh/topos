import { describe, expect, it } from "vitest";
import { type CountableSession, sessionCounts } from "@/lib/session-counts";

/**
 * The Sessions page header's ONE invariant: its buckets partition the rows. The page read
 * "2 active sessions · 1 stale" above exactly two rows — one fresh, one stale — because the
 * stale machine was counted twice.
 */

const session = (over: Partial<CountableSession> = {}): CountableSession => ({
  status: "active",
  expired: false,
  freshness: "fresh",
  ...over,
});

describe("the Sessions header counts", () => {
  it("reads 1 active · 1 stale over one fresh and one stale row", () => {
    const counts = sessionCounts([session(), session({ freshness: "stale" })]);
    expect(counts.active).toBe(1);
    expect(counts.stale).toBe(1);
  });

  it("never counts one session twice — the buckets add up to the rows", () => {
    const sessions = [
      session(),
      session(),
      session({ freshness: "stale" }),
      session({ freshness: "never" }),
      session({ expired: true }),
      session({ expired: true, freshness: "stale" }),
      session({ status: "pending", freshness: "never" }),
    ];
    const counts = sessionCounts(sessions);
    const total =
      counts.active + counts.pending + counts.stale + counts.neverReported + counts.expired;
    expect(total).toBe(sessions.length);
    expect(counts).toEqual({
      active: 2,
      pending: 1,
      stale: 1,
      neverReported: 1,
      // An expired credential is expired whatever its last report said — the row's chip agrees.
      expired: 2,
    });
  });

  it("counts a session that has never reported on its own, not as active", () => {
    const counts = sessionCounts([session({ freshness: "never" })]);
    expect(counts.active).toBe(0);
    expect(counts.neverReported).toBe(1);
  });

  it("is all zeros for an empty list", () => {
    expect(sessionCounts([])).toEqual({
      active: 0,
      pending: 0,
      stale: 0,
      neverReported: 0,
      expired: 0,
    });
  });
});
