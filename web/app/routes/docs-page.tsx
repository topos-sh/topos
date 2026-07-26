import type { LoaderFunctionArgs, MetaFunction } from "react-router";
import { data, redirect, useLoaderData } from "react-router";
import { DocsShell } from "@/components/docs/docs-shell";
import { DOCS_INDEX_ID, docsPageFor, docsPageView } from "@/lib/docs/docs.server";

/**
 * The documentation, rendered by the product itself. ONE module backs both mounts — `/docs` (the
 * index page) and `/docs/*` (every other page) — so the two can never drift apart.
 *
 * Public and sessionless by design: documentation is what a stranger reads before they have an
 * account, and a self-hosted install serves its own copy at its own origin. The content is a
 * committed, build-time-compiled module (app/lib/docs/), so this loader reads no database, no
 * filesystem, and no network — a docs page costs what a static asset costs.
 *
 * A path that names no page answers the house 404, exactly like any other missing route.
 */

export const meta: MetaFunction<typeof loader> = ({ loaderData }) => {
  if (!loaderData) {
    return [{ title: "Documentation · Topos" }];
  }
  // The home page's own title IS the product name, so the usual suffix would read "Topos · Topos".
  const title =
    loaderData.title === "Topos" ? "Topos documentation" : `${loaderData.title} · Topos`;
  return [{ title }, { name: "description", content: loaderData.description }];
};

export async function loader({ params }: LoaderFunctionArgs) {
  const splat = params["*"];
  // The index page has ONE address. `/docs/index` names the same bytes, so it redirects rather
  // than serving a second URL for them.
  if (splat !== undefined && splat.replace(/\/+$/, "") === DOCS_INDEX_ID) {
    throw redirect("/docs");
  }
  const page = docsPageFor(splat);
  if (page === null) {
    throw data(null, { status: 404 });
  }
  return docsPageView(page);
}

export default function DocsPageRoute() {
  return <DocsShell page={useLoaderData<typeof loader>()} />;
}
