import { isServer, QueryClient } from "@tanstack/react-query";

/**
 * The QueryClient factory (TanStack's recommended SSR shape). A `staleTime` above zero keeps the
 * client from refetching data the server already sent the moment it hydrates.
 */
function makeQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        staleTime: 60 * 1000,
      },
    },
  });
}

let browserQueryClient: QueryClient | undefined;

/**
 * Server: a FRESH client per request — never share cache across requests (one user's data must
 * not leak into another's render). Browser: a singleton, so navigations reuse one cache and a
 * mutation's invalidation reaches every mounted consumer.
 */
export function getQueryClient(): QueryClient {
  if (isServer) {
    return makeQueryClient();
  }
  browserQueryClient ??= makeQueryClient();
  return browserQueryClient;
}
