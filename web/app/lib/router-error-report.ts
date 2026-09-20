import { isRouteErrorResponse } from "react-router";

/**
 * Whether an error the router hands `handleError` is the ROUTER'S OWN verdict on a malformed
 * request rather than a fault in this app: a 4xx `ErrorResponse` it minted itself. Its two
 * shapes in production are a POST to a route that has no action (405 — a scanner posting
 * WordPress paths at the catch-all, or `/` and `/signin`) and a data request for a route that
 * does not exist (404). Neither carries a frame of ours or anything to fix, and reported
 * one-for-one they arrive in waves of hundreds a day that bury every real error in the
 * dashboard. So the server entry logs and reports everything EXCEPT these.
 *
 * The line is drawn at the status, not at "is a route error response": a 5xx `ErrorResponse`
 * is still a server-side failure and reports, as does every thrown `Error`. Pure predicate, kept
 * out of the server entry so it can be tested without booting migrations.
 */
export function isClientRouteError(error: unknown): boolean {
  return isRouteErrorResponse(error) && error.status >= 400 && error.status < 500;
}
