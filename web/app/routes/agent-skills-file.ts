import type { LoaderFunctionArgs } from "react-router";
import { agentSkills } from "@/lib/agent-skills.server";

/**
 * GET /.well-known/agent-skills/topos/:file — the built-in `topos` skill's three files
 * (SKILL.md, INSTALL.md, reference.md), served under the SAME base path as the discovery index
 * so SKILL.md's relative sibling references resolve wherever the index was fetched from. The
 * bytes come from the one process-lifetime read the index digest was computed over — served
 * and advertised bytes cannot drift. Anything but the three names answers a constant 404.
 */
export async function loader({ params }: LoaderFunctionArgs): Promise<Response> {
  const { files } = await agentSkills();
  const bytes = files.get(params.file ?? "");
  if (!bytes) {
    throw new Response("Not Found", { status: 404 });
  }
  return new Response(new Uint8Array(bytes), {
    headers: {
      "content-type": "text/markdown; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
