import { serverEnv } from "@/env.server";
import { clientKeyFromXff } from "@/lib/rate-limit.server";
import { rateLimited } from "./wire.server";

/**
 * The public door's rate belt — an in-process token bucket over `/api/v1`, replacing the belt the
 * vault used to wear on its own listener (which now sees exactly one peer: this app — a peer-IP
 * bucket there would throttle the whole fleet as one caller). Deliberately simple and secret-free:
 * keyed on the client address the TRUSTED proxy discloses — the LAST `x-forwarded-for` hop, the
 * SAME discipline the app's sign-in limiter uses (`clientKeyFromXff`): earlier hops are
 * client-supplied bytes, so keying on the first hop would let an attacker mint a fresh bucket per
 * forged prefix (the belt never fires) or forge a victim's egress IP to 429 a whole team. Never
 * keyed on the credential, which must not sit in a long-lived map. The 429 it answers is the
 * wire's frozen shape.
 *
 * `TOPOS_WEB_RATELIMIT=off` disables it (mirroring the vault's own knob) — the e2e stacks and any
 * deployment fronted by an edge limiter turn it off. NOTE: without a proxy in front (no
 * `x-forwarded-for`), every caller shares the `unknown` bucket — the single-peer throttle the
 * vault's belt-off avoids; a proxy-less deployment should set `TOPOS_WEB_RATELIMIT=off` and rely on
 * its ingress, exactly as the plane did.
 */
const BURST = 1000;
const REFILL_PER_SECOND = 50;
const MAX_BUCKETS = 100_000;

interface Bucket {
  tokens: number;
  refilledAt: number;
}

const buckets = new Map<string, Bucket>();

function keyFor(request: Request): string {
  return clientKeyFromXff(request.headers.get("x-forwarded-for"));
}

/** A 429 to answer, or null to pass. */
export function checkBelt(request: Request, now = Date.now()): Response | null {
  if (serverEnv().TOPOS_WEB_RATELIMIT === "off") {
    return null;
  }
  const key = keyFor(request);
  let bucket = buckets.get(key);
  if (bucket === undefined) {
    if (buckets.size >= MAX_BUCKETS) {
      buckets.clear();
    }
    bucket = { tokens: BURST, refilledAt: now };
    buckets.set(key, bucket);
  }
  const elapsed = Math.max(0, now - bucket.refilledAt) / 1000;
  bucket.tokens = Math.min(BURST, bucket.tokens + elapsed * REFILL_PER_SECOND);
  bucket.refilledAt = now;
  if (bucket.tokens < 1) {
    return rateLimited(Math.max(1, Math.ceil(1 / REFILL_PER_SECOND)));
  }
  bucket.tokens -= 1;
  return null;
}

/** Test-only: reset the belt between cases. */
export function resetBelt(): void {
  buckets.clear();
}
