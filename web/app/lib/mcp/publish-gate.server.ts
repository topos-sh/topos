import { Buffer } from "node:buffer";
import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import { mcpNameTaken } from "@/lib/mcp/catalog.server";
import { MAX_SERVER_JSON_BYTES, validateServerJson } from "@/lib/mcp/validate.server";

/**
 * THE GATE EVERY `kind: 'mcp'` PUBLISH PASSES — the one place a candidate is checked against
 * what an MCP bundle is allowed to be, wherever the bytes came from (the session lane's
 * publish/propose, or the web import page). It runs BEFORE any custody call: a refused
 * document must leave no ingested bytes behind, exactly like the kind gate above it.
 *
 * Two questions, in order:
 *  1. is the candidate's `server.json` a document this workspace can share at all
 *     (app/lib/mcp/validate.server.ts — remote, https, no template, no credential)? and
 *  2. is its EMBEDDED registry name free here?
 *
 * The second is what keeps the registry read lane unambiguous: `…/servers/{name}` resolves
 * against the embedded names, so two bundles declaring one name would make an agent's lookup a
 * coin flip. `MCP_NAME_TAKEN` covers both a proven collision and a catalog this tier could not
 * read end to end — the answer either way is "that name is not available here", and guessing
 * the other way would let the ambiguity in.
 */

/** The file an MCP bundle IS — at the bundle root, no directory, no siblings required. */
export const SERVER_JSON = "server.json";

export interface McpGateRefusal {
  code: string;
  message: string;
}

/** The candidate shape both doors already hold (the wire's, and the import page's own). */
export interface CandidateFile {
  path: string;
  content_base64: string;
}

/**
 * Check one candidate. `bundleId` is the bundle the candidate belongs to — excluded from the
 * name check, since a new version of a server is not a collision with itself. Returns null when
 * the candidate may proceed.
 */
export async function mcpCandidateRefusal(
  actor: MemberActor | SessionActor,
  files: CandidateFile[],
  bundleId: string | null,
): Promise<McpGateRefusal | null> {
  const file = files.find((f) => f.path === SERVER_JSON);
  if (file === undefined) {
    return {
      code: "MCP_INVALID",
      message: `an MCP bundle carries ${SERVER_JSON} at its root — this candidate has none`,
    };
  }
  const bytes = Buffer.from(file.content_base64, "base64");
  if (bytes.byteLength > MAX_SERVER_JSON_BYTES) {
    return { code: "MCP_INVALID", message: `${SERVER_JSON} is too large to be a server document` };
  }
  const validated = validateServerJson(bytes);
  if (!validated.ok) {
    return { code: validated.code, message: validated.message };
  }
  const taken = await mcpNameTaken(actor, validated.summary.name, bundleId);
  if (taken.kind === "taken") {
    return {
      code: "MCP_NAME_TAKEN",
      message: `${validated.summary.name} is already published in this workspace as ${taken.by.name}`,
    };
  }
  if (taken.kind === "indeterminate") {
    return {
      code: "MCP_NAME_TAKEN",
      message: `this workspace's MCP catalog could not be read end to end, so ${validated.summary.name} cannot be confirmed free`,
    };
  }
  return null;
}
