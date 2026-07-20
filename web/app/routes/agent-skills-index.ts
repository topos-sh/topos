import { agentSkills } from "@/lib/agent-skills.server";

/**
 * GET /.well-known/agent-skills/index.json — the agent-skills discovery index (the well-known
 * URI convention agents probe on any origin). ONE entry: the repo's own downloadable `topos`
 * skill, `type: "skill-md"`, its URL path-absolute under this same well-known base so it
 * resolves against whatever origin served the index, its digest computed from the exact bytes
 * the sibling file route serves. A resource route — no protocol-card interception.
 */
export async function loader(): Promise<Response> {
  const { indexJson } = await agentSkills();
  return new Response(indexJson, {
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
