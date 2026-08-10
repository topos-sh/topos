import { Buffer } from "node:buffer";
import { bundlePath } from "@/lib/bundle-base";
import { type IdentityHolder, identityHolder } from "@/lib/db/bundle-identity.server";
import { validateCandidateFiles } from "@/lib/mcp/validate.server";

/**
 * THE GATE EVERY `kind: 'mcp'` PUBLISH PASSES — the one place a candidate is checked against
 * what an MCP bundle is allowed to be, wherever the bytes came from (the session lane's
 * publish/propose, or the web import page). It runs BEFORE any custody call: a refused
 * document must leave no ingested bytes behind, exactly like the kind gate above it.
 *
 * Two questions, in order:
 *  1. is the WHOLE candidate something an MCP bundle may be — exactly the allowed file set, a
 *     credential scan over every file's bytes, and a `server.json` this workspace can share at
 *     all (app/lib/mcp/validate.server.ts — remote, https, no template, no credential)? and
 *  2. is its EMBEDDED registry name free here?
 *
 * The second is what keeps the registry read lane unambiguous: `…/servers/{name}` resolves
 * against the embedded names, so two bundles declaring one name would make an agent's lookup a
 * coin flip. It is an indexed read of the recorded claims (app/lib/db/bundle-identity.server.ts),
 * not a scan of the published documents.
 *
 * The name check here answers BEFORE the custody call, which is what a refusal needs — and is
 * therefore too early to be the last word: two publishes of one name can both pass it. So the
 * gate hands the validated name back on success, and every caller that goes on to REGISTER
 * CLAIMS that name inside its final transaction, where the identity key admits one claimant and
 * the conflict IS the refusal — worded by `mcpNameTakenRefusal`, so both looks say the same thing.
 */

/** The file an MCP bundle IS — at the bundle root; only the allowed siblings may stand beside it. */
export const SERVER_JSON = "server.json";

export interface McpGateRefusal {
  code: string;
  message: string;
  /**
   * WHERE THE ANSWER LIVES — the in-workspace path of the bundle already holding the name
   * (`mcp/<name>`, the shape `wsHref`/`useWsPath` roots), set only when a collision was PROVEN.
   * A refusal that names a bundle should be one click from it; a tier that could not read the
   * catalog has nothing to point at and sets nothing.
   */
  at?: string;
}

/**
 * What the gate decided: a refusal, or the embedded registry name the candidate validated to.
 * The name comes back because the caller needs it again — the claim it makes before it
 * registers records THIS name, not a second parse of the same bytes.
 */
export type McpGateOutcome = { refusal: McpGateRefusal } | { refusal: null; serverName: string };

/** The candidate shape both doors already hold (the wire's, and the import page's own). */
export interface CandidateFile {
  path: string;
  content_base64: string;
}

/**
 * How a taken name is worded — ONE place, so the pre-check and every caller's claim refuse in
 * the same words at every door. The refusal names the bundle holding it AND points at it: the
 * next thing anyone does with this answer is go look at that server, so the path rides the
 * message (for a machine reading the wire) and the `at` field (for a surface that can render a
 * real link).
 */
export function mcpNameTakenRefusal(serverName: string, heldBy: IdentityHolder): McpGateRefusal {
  const at = bundlePath("mcp", heldBy.name);
  return {
    code: "MCP_NAME_TAKEN",
    message: `${serverName} is already published in this workspace as ${heldBy.name} — view /${at}`,
    at,
  };
}

/**
 * Check one candidate. `bundleId` is the bundle the candidate belongs to — excluded from the
 * name check, since a new version of a server is not a collision with itself.
 */
export async function mcpCandidateRefusal(
  actor: { workspaceId: string },
  files: CandidateFile[],
  bundleId: string | null,
): Promise<McpGateOutcome> {
  // The WHOLE candidate goes through the pure gate — every file, not just the document: the
  // allowlist, the per-file credential scan, and the server.json rules in one pass.
  const validated = validateCandidateFiles(
    files.map((f) => ({ path: f.path, bytes: Buffer.from(f.content_base64, "base64") })),
  );
  if (!validated.ok) {
    return { refusal: { code: validated.code, message: validated.message } };
  }
  const held = await identityHolder(actor.workspaceId, "mcp", validated.summary.name, bundleId);
  if (held !== null) {
    return { refusal: mcpNameTakenRefusal(validated.summary.name, held) };
  }
  return { refusal: null, serverName: validated.summary.name };
}
