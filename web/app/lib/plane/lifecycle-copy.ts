/**
 * The lifecycle ceremonies' PURE half — the name-charset belt and the denied-outcome → copy
 * maps. Client-safe by design (no vault call, no env, no secret): the rename form uses the
 * charset constants in its own markup, so this module must stay importable from component
 * code — the vault write helpers live in lifecycle.server.ts.
 */
/** The catalog-name charset the rename input validates against (client belt + a server re-check). */
export const SKILL_NAME_RE = /^[a-z0-9][a-z0-9-]*$/;
export const SKILL_NAME_MAX = 64;

/** Whether a proposed rename target is a syntactically valid catalog name (the web belt). */
export function isValidSkillName(name: string): boolean {
  return SKILL_NAME_RE.test(name) && name.length <= SKILL_NAME_MAX;
}

/** Inline copy for a refused RENAME — maps the guarded function's outcome code to plain words. */
export function renameDeniedCopy(reason: string | undefined): string {
  switch (reason) {
    case "name_taken":
      return "That name is already taken — choose another.";
    case "bad_name":
      return "Use lowercase letters, numbers, and hyphens (max 64 characters).";
    case "not_active":
      return "This skill can't be renamed — it isn't active.";
    case "owner_role_required":
      return "Only a workspace owner can rename a skill.";
    default:
      return "The server declined this rename.";
  }
}

/** Inline copy for a refused UNARCHIVE — the name-reuse case names the way out. */
export function unarchiveDeniedCopy(reason: string | undefined): string {
  switch (reason) {
    case "name_taken":
      return "the name was reused — rename after unarchiving";
    case "not_archived":
      return "This skill isn't archived.";
    case "owner_role_required":
      return "Only a workspace owner can unarchive a skill.";
    default:
      return "The server declined this unarchive.";
  }
}

/** Inline copy for a refused DELETE. */
export function deleteDeniedCopy(reason: string | undefined): string {
  switch (reason) {
    case "not_archived":
      return "Archive this skill before deleting it.";
    case "owner_role_required":
      return "Only a workspace owner can delete a skill.";
    default:
      return "The server declined this delete.";
  }
}

/** Inline copy for a refused PURGE. */
export function purgeDeniedCopy(reason: string | undefined): string {
  switch (reason) {
    case "is_current":
      return "This is the current version — purge an older one instead.";
    case "already_purged":
      return "This version's bytes are already purged.";
    case "owner_role_required":
      return "Only a workspace owner can purge a version.";
    default:
      return "The server declined this purge.";
  }
}
