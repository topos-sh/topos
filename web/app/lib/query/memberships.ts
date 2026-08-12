import type { WorkspaceMembership } from "@/lib/db/queries.server";

/**
 * The shared query identity for the signed-in user's seats. The SERVER reads them (calling the
 * DAL directly) and seeds them into the cache under this key; the CLIENT rail paints from that
 * seed and refetches on its own — nothing invalidates this key by hand, so the rail catches up
 * the next time it remounts or the window regains focus past the 60s staleTime.
 * `WorkspaceMembership` is a type-only import (erased at compile time), so this stays client-safe.
 */
export const membershipsQueryKey = ["memberships"] as const;

/** The client-side fetcher: the guarded GET route, which returns `membershipsFor` as JSON. */
export async function fetchMemberships(): Promise<WorkspaceMembership[]> {
  const res = await fetch("/api/memberships", {
    headers: { accept: "application/json" },
  });
  if (!res.ok) {
    throw new Error(`memberships request failed: ${res.status}`);
  }
  return (await res.json()) as WorkspaceMembership[];
}
