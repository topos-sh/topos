import type { WorkspaceMembership } from "@/lib/db/queries.server";

/**
 * The shared query identity for the signed-in user's seats. The SERVER prefetches them (calling
 * the DAL directly) and dehydrates; the CLIENT rail reads the hydrated data and, after a
 * membership change, invalidates this exact key to refetch — the rail updates without a reload.
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
