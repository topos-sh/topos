import type { LoaderFunctionArgs } from "react-router";
import { redirect } from "react-router";
import {
  DOCS_BASE,
  DOCS_INDEX_ID,
  docsMarkdown,
  docsMarkdownResponse,
} from "@/lib/docs/docs.server";

/**
 * GET /docs/<page>.md — every documentation page, as the plain markdown it was written in.
 *
 * The twin an agent can link to and paste around: append `.md` to any docs URL and get prose it
 * can read without parsing a page — the component tags reduced to ordinary markdown (an aside
 * becomes a labelled paragraph, a step list becomes numbered headings), the author's own code
 * fences and tables untouched. `/docs.md` is the index page's twin, so the rule "any docs path
 * plus .md" holds for the docs root too. Fetching the PAGE path with a non-HTML `Accept` answers
 * with these same bytes (`docsNegotiatedMarkdown`); this route is the addressable spelling.
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
  // The index twin has ONE address, `/docs.md` — the same one-address rule the page routes keep.
  if (id === DOCS_INDEX_ID && pathname !== `${DOCS_BASE}.md`) {
    return redirect(`${DOCS_BASE}.md`);
  }
  const markdown = docsMarkdown(id);
  if (markdown === null) {
    throw new Response("Not Found", { status: 404 });
  }
  return docsMarkdownResponse(markdown);
}
