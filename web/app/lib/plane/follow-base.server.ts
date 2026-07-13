import { serverEnv } from "@/env.server";

/**
 * The two public bases this app discloses, now that it IS the door:
 *
 *  - `followBase` — the origin a shareable resource address rides (`topos follow
 *    <base>/<address>`, the invite mail, the settings page's address block). The app serves those
 *    addresses itself (every one answers the constant protocol card to a non-browser fetcher).
 *  - `apiBase` — the API base the protocol card teaches a client to re-root onto: this origin's
 *    `/api` mount, where the device lane (`/api/v1/…`) is served. The CLI joins `/v1/…` paths onto
 *    whatever base the card declares, so the `/api` suffix lives HERE, once.
 *
 * The origin is `TOPOS_PUBLIC_URL` when configured, else the request's own origin. The config
 * override is REQUIRED behind a TLS-terminating reverse proxy: the container speaks plain HTTP, so
 * a request-derived origin would be `http://…` and the CLI refuses to re-root an https link onto an
 * http base (a silent transport downgrade). Same-origin deployments (the local compose `up`, the
 * e2e) leave it unset and the request origin is correct. (This replaces the interim
 * `PLANE_PUBLIC_URL`, which pointed at a separately-exposed plane that no longer exists.)
 */
export function followBase(request: Request): string {
  const configured = serverEnv().TOPOS_PUBLIC_URL;
  if (configured !== undefined) {
    return configured.replace(/\/+$/, "");
  }
  return new URL(request.url).origin;
}

export function apiBase(request: Request): string {
  return `${followBase(request)}/api`;
}
