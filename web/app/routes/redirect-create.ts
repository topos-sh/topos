import { redirect } from "react-router";

/**
 * A historical URL shape kept honest: `/create` permanently redirects to `/workspaces`. This
 * install serves ONE boot-minted workspace, so there is no create page to point at — the
 * collection route resolves a visitor to their seat (or the honest miss); a hosted deployment
 * that offers creation appends its own route for it.
 */
export function loader(): Response {
  return redirect("/workspaces", 301);
}
