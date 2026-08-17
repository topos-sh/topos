/**
 * HOW A DOOR SAYS NO — the refusal shape every gate in this app hands back.
 *
 * A refusal is an ANSWER, not a fault: a code a caller may branch on and a sentence a person
 * reads, returned rather than thrown, so a routine "no" never takes a ceremony down with it. The
 * shape started as the MCP publish gate's and is now the house's — the bundle cap, the storage
 * quota and the whole server catalog all speak it, which is why every door's refusals read alike.
 */
export interface McpGateRefusal {
  code: string;
  message: string;
  /**
   * WHERE THE ANSWER LIVES — the in-workspace path of the row a refusal points at (`mcp/<name>`,
   * the shape `wsHref`/`useWsPath` roots), set only when there is one. A refusal that names
   * something should be one click from it; a refusal with nothing to point at sets nothing.
   */
  at?: string;
}
