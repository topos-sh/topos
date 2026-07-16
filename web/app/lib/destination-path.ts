/**
 * The DESTINATION pathname of a request, document or data. React Router's client-side
 * navigations and submissions hit a suffixed URL (single fetch): a trailing-slash destination
 * is spelled `<path>/_.data` (`/` itself becomes `/_.data`), everything else `<path>.data` —
 * same loaders and actions, a different URL. Anything deriving state from the request path
 * (the chrome's active-seat match, a ceremony's return-to/scope, a login bounce's `next`) must
 * see the DESTINATION, never the raw data URL — a mailed link or redirect built from the raw
 * pathname opens the single-fetch endpoint as a document (raw payload bytes, dead page).
 *
 * Mirrors the framework's own server-runtime normalization (react-router
 * `lib/server-runtime/urls.ts`, `getNormalizedPath`). A workspace slug
 * (`^[a-z0-9][a-z0-9-]*$`) can never collide with either spelling. Client-safe (no server
 * import), pure.
 */
export function destinationPathname(request: Request): string {
  const { pathname } = new URL(request.url);
  if (pathname.endsWith("/_.data")) {
    return pathname.slice(0, -"_.data".length);
  }
  if (pathname.endsWith(".data")) {
    return pathname.slice(0, -".data".length);
  }
  return pathname;
}
