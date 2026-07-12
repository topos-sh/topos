import { QueryClientProvider } from "@tanstack/react-query";
import { type ReactNode, useState } from "react";
import type { WorkspaceMembership } from "@/lib/db/queries.server";
import { getQueryClient } from "@/lib/query/get-query-client";
import { membershipsQueryKey } from "@/lib/query/memberships";

/**
 * The React Query boundary for the signed-in app. React Router has no per-request dehydrate/hydrate
 * cycle the way the RSC layout did — instead the shell route's loader returns the workspace roster
 * and hands it here, and this provider seeds it straight into the cache under the memberships query
 * key. The rail's `useQuery` then paints from that server data on first render (no loading flash),
 * while React Query still owns refetch + invalidation on the client (create a workspace and the rail
 * refetches without a reload). `getQueryClient()` hands back a fresh client on the server and the
 * browser singleton on the client, so the seed never leaks across requests.
 */
export function Providers({
  children,
  memberships,
}: {
  children: ReactNode;
  memberships?: WorkspaceMembership[];
}) {
  const queryClient = getQueryClient();
  // Seed once per mount (a useState initializer fires exactly once): the analog of the old
  // dehydrate/hydrate boundary, without a render-time side effect on every pass.
  useState(() => {
    if (memberships !== undefined) {
      queryClient.setQueryData(membershipsQueryKey, memberships);
    }
    return null;
  });
  return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>;
}
