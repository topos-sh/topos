import type { LoaderFunctionArgs } from "react-router";
import { getAuth, RESOLVED_CLIENT_IP_HEADER } from "@/lib/auth/server";
import { clientKeyFromXff } from "@/lib/rate-limit.server";

/**
 * The Better Auth request handler, mounted at /api/auth/* — the login ceremony's whole surface
 * (sign-in, sign-up, sessions, and any composition-provided rungs). getAuth() constructs lazily
 * on first request, never at build time; the handler answers both GET and POST.
 *
 * The credential endpoints are rate-limited PER CLIENT, and the library resolves that client
 * from a header. Its own `x-forwarded-for` reading trusts a single-hop value only, so behind
 * the ordinary two-hop ingress (edge → reverse proxy → app) it resolves nothing and folds
 * EVERY caller into one global bucket — which, against its 3-per-10s sign-in rule, is a
 * product-wide login outage anyone can hold open. So the app resolves the client itself, with
 * the same last-hop discipline its own limiters use, and hands the result over on a private
 * header. The header is OVERWRITTEN on every request, never merged, so a caller cannot present
 * its own value and mint a fresh bucket per request.
 */
function handler({ request }: LoaderFunctionArgs): Promise<Response> {
  // The library needs a parseable IP, and `unknown` (a direct hit, no proxy) is not one — map it
  // to loopback, which is the one-bucket-for-everyone answer the app's own limiters already give
  // that case, and the same fallback the library uses off-production.
  const key = clientKeyFromXff(request.headers.get("x-forwarded-for"));
  const headers = new Headers(request.headers);
  headers.set(RESOLVED_CLIENT_IP_HEADER, key === "unknown" ? "127.0.0.1" : key);
  return getAuth().handler(new Request(request, { headers }));
}

export const loader = handler;
export const action = handler;
