import { redirect } from "react-router";
import { type BundleBase, baseForKind, bundlePath } from "@/lib/bundle-base";
import { wsPathServer } from "@/lib/ws-url.server";

/**
 * The kind FENCE every bundle page runs after its catalog probe: a bundle is reachable only
 * under the base its kind belongs to. Addressing a server as a skill (or the reverse) sends the
 * reader to the canonical path — a MEMBER, and only a member, ever gets here (the guard above
 * has already answered every anonymous and non-member request with the uniform 404), and a
 * member can see both sections anyway, so the redirect discloses nothing a 404 would hide. It
 * also keeps every older link — an invitation's first destination, a channel row, a bookmark —
 * landing on the right page instead of a dead end.
 *
 * `tail` is the sub-page path the caller sits on (`/history`), `search` the query string to
 * carry across (a paging cursor). Returns when the base is already canonical.
 */
export function requireCanonicalBase(args: {
  /** The workspace NAME — the multi-tenancy URL segment; ignored in single. */
  wsName: string;
  /** The base this request arrived on. */
  base: BundleBase;
  /** The catalog row's kind — the authority on where the bundle belongs. */
  kind: string;
  /** The bundle's catalog name (the URL key). */
  name: string;
  tail?: string;
  search?: string;
}): void {
  const canonical = baseForKind(args.kind);
  if (canonical === args.base) {
    return;
  }
  throw redirect(
    wsPathServer(args.wsName, bundlePath(canonical, args.name, args.tail ?? "")) +
      (args.search ?? ""),
  );
}
