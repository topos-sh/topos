import type { LoaderFunctionArgs } from "react-router";
import { docsLlmsTxt } from "@/lib/docs/docs.server";
import { followBase } from "@/lib/plane/follow-base.server";

/**
 * GET /docs/llms.txt — the documentation index in the llms.txt convention: every page, its
 * one-sentence description, and its ABSOLUTE url. The origin comes from this deployment's own
 * canonical base (the same resolution the setup line and the protocol card use), so a
 * self-hosted install indexes itself rather than someone else's address.
 *
 * Served as text/plain so a browser renders it readable instead of downloading it. A resource
 * route: the loader's Response is returned directly.
 */

export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  return new Response(docsLlmsTxt(followBase(request)), {
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
