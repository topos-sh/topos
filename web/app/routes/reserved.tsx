import { notFound } from "@/lib/auth/guards.server";

/**
 * A reserved top-level segment that answers the house 404. In MULTI tenancy `claim` has no page
 * (no boot workspace is minted), but it must still be REGISTERED — otherwise the `/:ws` face would
 * swallow `/claim` and treat "claim" as a workspace name. A 404 here discloses nothing (a reserved
 * segment is never a workspace), and it keeps "claim" off the addressable workspace-name space.
 */
export async function loader() {
  notFound();
}

export default function Reserved() {
  return null;
}
