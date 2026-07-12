import { redirect } from "react-router";

/**
 * A historical URL shape kept honest: `/link` permanently redirects to `/workspaces`. The old
 * free-floating path 301s to the resource collection that replaced it.
 */
export function loader(): Response {
  return redirect("/workspaces", 301);
}
