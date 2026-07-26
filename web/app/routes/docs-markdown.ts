import type { LoaderFunctionArgs } from "react-router";
import { DOCS_BASE, DOCS_INDEX_ID, docsMarkdown } from "@/lib/docs/docs.server";

/**
 * GET /docs/<page>.md — every documentation page, as the plain markdown it was written in.
 *
 * PATH SHAPE decides the face here, the way it does across this app: an agent handed a docs URL
 * appends `.md` and gets prose it can read without parsing a page — the component tags reduced to
 * ordinary markdown (an aside becomes a labelled paragraph, a step list becomes numbered
 * headings), the author's own code fences and tables untouched. `/docs.md` is the index page's
 * twin, so the rule "any docs path plus .md" holds for the docs root too.
 *
 * A resource route (loader only): its Response is returned as-is, so the protocol-card
 * interception — which sees document renders — never touches it.
 */

export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const { pathname } = new URL(request.url);
  const id =
    pathname === `${DOCS_BASE}.md`
      ? DOCS_INDEX_ID
      : pathname.slice(`${DOCS_BASE}/`.length, -".md".length);
  const markdown = docsMarkdown(id);
  if (markdown === null) {
    throw new Response("Not Found", { status: 404 });
  }
  return new Response(markdown, {
    headers: {
      "content-type": "text/markdown; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
