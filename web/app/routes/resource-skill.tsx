import type { LoaderFunctionArgs, MiddlewareFunction } from "react-router";
import { ResourcePage } from "@/components/resource-page";
import { notFound } from "@/lib/auth/guards.server";
import { cardResponse } from "@/lib/card.server";
import { resourceTeaser } from "@/lib/resource-page.server";

export function meta() {
  return [{ title: "A Topos resource address" }];
}

/**
 * `<origin>/<workspace>/skills/<name>` — a skill's shareable address. Same three faces as the
 * workspace address: constant card (middleware), constant teaser, or the member's skill page.
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
  const name = params.name;
  if (!address || !name) {
    notFound();
  }
  return resourceTeaser(
    request,
    address,
    (workspaceId) => `/workspaces/${workspaceId}/skills/${encodeURIComponent(name)}`,
  );
}

export default function ResourceSkill() {
  return <ResourcePage />;
}
