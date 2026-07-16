/**
 * The workspace ADDRESS-SLUG shape and its derivation — one client-safe rule shared by the
 * create form (client), its action (server), and the create DAL (server), so nothing drifts.
 * The regex mirrors the `workspace_name` CHECK in `schema.app.ts` (`^[a-z0-9][a-z0-9-]*$`,
 * length ≤ 100); this module imports no server surface, so the browser bundle may carry it.
 */

export const WORKSPACE_NAME_RE = /^[a-z0-9][a-z0-9-]*$/;
export const WORKSPACE_NAME_MAX = 100;

/** Whether a string is a valid workspace address slug (charset + length). */
export function isWorkspaceNameShape(name: string): boolean {
  return WORKSPACE_NAME_RE.test(name) && name.length <= WORKSPACE_NAME_MAX;
}

/**
 * Derive an address slug from a free-text display name: lowercase, spaces/underscores become
 * hyphens, every other character is dropped, runs of hyphens collapse, and leading/trailing
 * hyphens are trimmed. The result is either empty or a valid slug — the form previews it live and
 * the person may still edit it.
 */
export function toWorkspaceSlug(input: string): string {
  return input
    .toLowerCase()
    .replace(/[\s_]+/g, "-")
    .replace(/[^a-z0-9-]/g, "")
    .replace(/-+/g, "-")
    .replace(/^-+|-+$/g, "");
}
