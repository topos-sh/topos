import type { LoaderFunctionArgs } from "react-router";
import { fetchClaimPassthrough } from "@/lib/plane/reads.server";
import { allowPublicRead, clientKeyFromXff } from "@/lib/rate-limit.server";

/**
 * The public `/i/<token>` link, whole — it serves one-time ADMIN CLAIM links relayed from the
 * vault (there is no HTML preview page). A verbatim pass-through: the incoming `Accept` rides to
 * the vault and the vault's own content negotiation answers, so this tier serves the SAME two
 * representations the vault does — never a third. A browser displays the vault's plain-text
 * document (human hand-off first). Always `no-store` (the URL path carries the token).
 *
 * The in-process rate limit is the web tier's own modest floor for this unauthenticated lane:
 * proxied fetches reach the vault from THIS server's address, so the vault's per-peer-IP limiter
 * can no longer distinguish callers — the web must apply the per-client budget itself.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  if (!allowPublicRead(clientKeyFromXff(request.headers.get("x-forwarded-for")))) {
    return Response.json(
      { ok: false, error: "rate_limited" },
      { status: 429, headers: { "retry-after": "10", "cache-control": "no-store" } },
    );
  }
  const token = params.token ?? "";
  // An ABSENT Accept must reach the vault as its machine-contract default (JSON) — undici would
  // otherwise inject `*/*` and flip an Accept-less client to markdown on this origin only.
  const accept = request.headers.get("accept") ?? "application/json";
  const answer = await fetchClaimPassthrough(token, accept);
  if (answer === undefined) {
    return Response.json(
      { ok: false, error: "plane_unreachable" },
      { status: 502, headers: { "cache-control": "no-store" } },
    );
  }
  return new Response(answer.body, {
    status: answer.status,
    headers: {
      "content-type": answer.contentType,
      "cache-control": "no-store",
      vary: "accept",
      "x-robots-tag": "noindex",
      ...(answer.retryAfter ? { "retry-after": answer.retryAfter } : {}),
    },
  });
}
