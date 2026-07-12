/**
 * Minimal in-process token buckets for the web tier's own belts. One bucket per client key,
 * refilled continuously; memory is bounded by evicting the stalest keys past a cap. Honest
 * scope: per-PROCESS (one web instance), mirroring the vault's own in-process limiter — a real
 * multi-instance deployment moves this to the edge.
 */
interface Bucket {
  tokens: number;
  updatedAt: number;
}

function bucketLimiter({
  burst,
  refillPerSec,
  maxKeys,
}: {
  burst: number;
  refillPerSec: number;
  maxKeys: number;
}): (key: string, now?: number) => boolean {
  const buckets = new Map<string, Bucket>();
  return function allow(key: string, now = Date.now()): boolean {
    const bucket = buckets.get(key) ?? { tokens: burst, updatedAt: now };
    const elapsed = Math.max(0, now - bucket.updatedAt) / 1000;
    bucket.tokens = Math.min(burst, bucket.tokens + elapsed * refillPerSec);
    bucket.updatedAt = now;
    const allowed = bucket.tokens >= 1;
    if (allowed) {
      bucket.tokens -= 1;
    }
    // Refresh recency on EVERY touch — allowed or denied. `Map.set` on an existing key keeps its
    // old position, so without the delete a hammering (denied) key would look stalest and get
    // evicted first — handing the hottest abuser a fresh full burst.
    buckets.delete(key);
    buckets.set(key, bucket);
    if (buckets.size > maxKeys) {
      const oldest = buckets.keys().next().value;
      if (oldest !== undefined) {
        buckets.delete(oldest);
      }
    }
    return allowed;
  };
}

/**
 * The PUBLIC, unauthenticated read belt (the `/i/<token>` pass-through route — every fetch of a
 * share link rides it), keyed per client address.
 */
export const allowPublicRead = bucketLimiter({ burst: 30, refillPerSec: 2, maxKeys: 10_000 });

/**
 * The proposal-comment WRITE belt, keyed per acting email (the guard-minted actor): route
 * actions bypass every route-level limiter, so the action itself wears the belt. Burst 5, one
 * token back every ~10 s — a human conversation never notices; a runaway loop does.
 */
export const allowCommentWrite = bucketLimiter({ burst: 5, refillPerSec: 0.1, maxKeys: 10_000 });

/**
 * The session-revert WRITE belt, keyed per acting email (the guard-minted actor): route actions
 * bypass every route-level limiter, so the action itself wears the belt. Same human-paced shape
 * as the comment belt — burst 5, one token back every ~10 s — a reviewer clicking through
 * history never notices; a runaway loop does. (The vault's own limiter is the authority; this is
 * the web tier's matching belt.)
 */
export const allowRevertWrite = bucketLimiter({ burst: 5, refillPerSec: 0.1, maxKeys: 10_000 });

/**
 * The STEP-UP password re-entry belt, keyed per acting USER ID: a step-up prompt must never
 * become a free password oracle for a hijacked session. Deliberately the tightest belt here —
 * burst 5, one token back every ~20 s; a human confirming a ceremony never notices.
 */
export const allowStepUpAttempt = bucketLimiter({ burst: 5, refillPerSec: 0.05, maxKeys: 10_000 });

/**
 * The per-client limiter key from an `x-forwarded-for` value: the LAST hop — the one address the
 * trusted edge itself appended from the socket peer. Earlier hops are client-supplied bytes (an
 * attacker rotating forged prefixes must not mint fresh buckets or poison a victim's). No header
 * at all (a direct hit, dev) shares one bucket.
 */
export function clientKeyFromXff(xff: string | null): string {
  const hops = (xff ?? "").split(",");
  return hops.at(-1)?.trim() || "unknown";
}
