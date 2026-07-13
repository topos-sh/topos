import { redirect } from "react-router";
import { serverEnv } from "@/env.server";
import { cardFace } from "@/lib/card.server";

/**
 * The CANONICAL-ORIGIN redirect for BROWSERS on an alias origin. A deployment may route several
 * domains to this app (the hosted plane keeps its legacy API domain as an alias); the machine
 * faces are origin-agnostic — the constant card re-roots every client onto the declared API base
 * — but a browser SESSION is not: auth is anchored on the canonical origin, so a person browsing
 * an alias would render pages whose sign-in the auth layer refuses (cross-origin). Rather than a
 * half-working app, a browser-shaped request on a non-canonical HOST gets one constant 301 to
 * the same path on the canonical origin.
 *
 * Non-browser faces pass untouched (the CLI's card fetches, `/api` dials, and claim reads on an
 * alias keep working), and the redirect is host-keyed and path-blind — constant for every path,
 * never an existence signal. Without `TOPOS_PUBLIC_URL` (single-origin deployments, dev, the
 * composed e2e) this is a no-op.
 */
export function canonicalOriginRedirect(request: Request): Response | null {
  const canonical = serverEnv().TOPOS_PUBLIC_URL?.replace(/\/+$/, "");
  if (!canonical) {
    return null;
  }
  const canonicalHost = new URL(canonical).host;
  const url = new URL(request.url);
  // HOST comparison, not origin: behind the TLS proxy the container speaks plain http while the
  // canonical origin is https — the host is the identity that matters.
  if (url.host === canonicalHost) {
    return null;
  }
  if (cardFace(request) !== "html") {
    return null;
  }
  return redirect(`${canonical}${url.pathname}${url.search}`, 301);
}
