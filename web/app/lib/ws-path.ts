import { useParams } from "react-router";

/**
 * The ONE workspace-URL builder — client-safe and pure, so a page or component never encodes the
 * tenancy grammar itself. Two grammars share one route table:
 *
 *  - SINGLE tenancy (the OSS default): the install IS the workspace, so the signed-in surface is
 *    origin-rooted — there is no workspace segment. `wsSegment` is null and every path builds off
 *    the origin (`/`, `/members`, `/skills/foo`).
 *  - MULTI tenancy (a downstream superset): the same modules mount under `/:ws`, where `:ws` is
 *    the workspace NAME slug. `wsSegment` is that slug and paths nest under it (`/acme/members`).
 *
 * A page reads the active segment from the route params (there is no `:ws` in single mode, so the
 * param is absent and paths stay origin-rooted); it never string-concatenates a workspace prefix.
 */

/**
 * Build a workspace-scoped path. `wsSegment` null → origin-rooted; else nested under the segment.
 * `sub` is the in-workspace path (no leading slash). Omit `sub` for the workspace root. The only
 * trailing slash this ever emits is the bare origin root (`wsHref(null)` → `/`).
 */
export function wsHref(wsSegment: string | null, sub?: string): string {
  const tail = sub ?? "";
  if (wsSegment === null) {
    return `/${tail}`;
  }
  return tail.length > 0 ? `/${wsSegment}/${tail}` : `/${wsSegment}`;
}

/**
 * The hook a signed-in page/component uses to build its own in-workspace links. It reads the
 * active workspace slug from the route params — absent in single mode (origin-rooted), the name
 * slug in multi — so the returned builder yields the right grammar without the caller knowing the
 * mode. Pass the in-workspace sub-path (no leading slash); omit it for the workspace root.
 */
export function useWsPath(): (sub?: string) => string {
  const wsSegment = useParams().ws ?? null;
  return (sub?: string) => wsHref(wsSegment, sub);
}
