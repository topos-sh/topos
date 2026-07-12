import { serverEnv } from "@/env.server";

/**
 * The origin a `topos follow <base>/<address>` command should dial. Until this app fronts the
 * API, an agent that fetches the app's own `/{workspace}` path gets the app's 404, not the
 * protocol card the CLI re-roots from — so a deployment that still exposes the plane directly
 * sets PLANE_PUBLIC_URL and every emitted follow command (the address blocks, the invite mail)
 * points there instead. Unset, the app's own origin is the default — the door-cutover target
 * state, and the right answer once the app serves the card.
 */
export function followBase(request: Request): string {
  const configured = serverEnv().PLANE_PUBLIC_URL;
  if (configured !== undefined) {
    return configured.replace(/\/$/, "");
  }
  return new URL(request.url).origin;
}
