import { createHash } from "node:crypto";

/**
 * REVISION IDS FOR TESTS, in the shape the database now insists on
 * (`mcpr_` + 32 lowercase hex — `mcp_server_revision_id_shape_check`).
 *
 * A test still names its rows by a readable slug; this derives the stored id from it, the same
 * `'mcpr_' || md5(<slug>)` derivation the catalog's seed migration uses. Deterministic, so a row a
 * test seeds under one name is the row it later reads under that name — and so two tests naming
 * different things can never collide on one id.
 */
export function mcpRevisionId(slug: string): string {
  return `mcpr_${createHash("md5").update(slug).digest("hex")}`;
}
