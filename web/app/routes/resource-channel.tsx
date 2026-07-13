import type { LoaderFunctionArgs } from "react-router";
import { ResourcePage } from "@/components/resource-page";
import { notFound } from "@/lib/auth/guards.server";
import { resourceTeaser } from "@/lib/resource-page.server";

export function meta() {
  return [{ title: "A Topos resource address" }];
}

/**
 * `<origin>/<workspace>/channels/<name>` — a channel's shareable address. Same three faces as
 * the workspace address: constant card (the server entry), constant teaser, or the member's
 * channel page.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const address = params.ws;
  const name = params.name;
  if (!address || !name) {
    notFound();
  }
  return resourceTeaser(
    request,
    address,
    (workspaceId) => `/workspaces/${workspaceId}/channels/${encodeURIComponent(name)}`,
  );
}

export default function ResourceChannel() {
  return <ResourcePage />;
}
