import type { LoaderFunctionArgs, MiddlewareFunction } from "react-router";
import { ResourcePage } from "@/components/resource-page";
import { notFound } from "@/lib/auth/guards.server";
import { cardResponse } from "@/lib/card.server";
import { resourceTeaser } from "@/lib/resource-page.server";

export function meta() {
  return [{ title: "A Topos resource address" }];
}

/**
 * `<origin>/<workspace>` — the workspace's shareable address. A non-browser fetcher gets the
 * CONSTANT protocol card (served whole from the middleware, before any loader runs — no
 * existence signal can leak from work that never happens); an anonymous browser gets the
 * constant teaser page; a signed-in member is sent into the workspace surface.
 */
export const middleware: MiddlewareFunction[] = [
  async ({ request }, next) => {
    const card = cardResponse(request);
    if (card) {
      return card;
    }
    return next();
  },
];

export async function loader({ request, params }: LoaderFunctionArgs) {
  const address = params.ws;
  if (!address) {
    notFound();
  }
  return resourceTeaser(request, address, (workspaceId) => `/workspaces/${workspaceId}`);
}

export default function ResourceWorkspace() {
  return <ResourcePage />;
}
