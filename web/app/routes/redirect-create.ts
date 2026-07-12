import { redirect } from "react-router";

/**
 * A historical URL shape kept honest: `/create` permanently redirects to the resource route
 * `/workspaces/new`. Resource-oriented routes nest under their collection; this old verb path
 * 301s to its replacement.
 */
export function loader(): Response {
  return redirect("/workspaces/new", 301);
}
