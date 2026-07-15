/**
 * The lifecycle ceremonies' PURE half — the name-charset belt and the refused-outcome → copy
 * maps. Client-safe by design (no vault call, no env, no secret): the rename form uses the
 * charset constants in its own markup, so this module must stay importable from component
 * code — the ceremonies themselves live in app/lib/db/queries.lifecycle.server.ts. Role
 * refusals need no copy here: the OwnerActor brand gates every ceremony before it runs.
 */
/** The catalog-name charset the rename input validates against (client belt + a server re-check). */
export const SKILL_NAME_RE = /^[a-z0-9][a-z0-9-]*$/;
export const SKILL_NAME_MAX = 64;

/** Whether a proposed rename target is a syntactically valid catalog name (the web belt). */
export function isValidSkillName(name: string): boolean {
  return SKILL_NAME_RE.test(name) && name.length <= SKILL_NAME_MAX;
}

/** Inline copy for a refused RENAME — maps the ceremony's outcome code to plain words. */
export function renameDeniedCopy(outcome: string): string {
  switch (outcome) {
    case "name_taken":
      return "That name is already taken — choose another.";
    case "bad_name":
      return "Use lowercase letters, numbers, and hyphens (max 64 characters).";
    case "not_active":
      return "This skill can't be renamed — it isn't active.";
    case "unknown_skill":
      return "This skill no longer exists.";
    default:
      return "The server declined this rename.";
  }
}

/** Inline copy for a refused UNARCHIVE — the name-reuse case names the way out. */
export function unarchiveDeniedCopy(outcome: string): string {
  switch (outcome) {
    case "name_taken":
      return "The name was reused since — unarchive is refused; rename the newer skill first.";
    case "not_archived":
      return "This skill isn't archived.";
    case "unknown_skill":
      return "This skill no longer exists.";
    default:
      return "The server declined this unarchive.";
  }
}

/** Inline copy for a refused DELETE. */
export function deleteDeniedCopy(outcome: string): string {
  switch (outcome) {
    case "not_archived":
      return "Archive this skill before deleting it.";
    case "unknown_skill":
      return "This skill no longer exists.";
    default:
      return "The server declined this delete.";
  }
}

/** Inline copy for a refused PURGE (re-purging is idempotent, so no already-purged arm). */
export function purgeDeniedCopy(outcome: string): string {
  switch (outcome) {
    case "is_current":
      return "This is the current version — purge an older one instead.";
    case "unknown_version":
      return "The server has no version with this id for this skill.";
    default:
      return "The server declined this purge.";
  }
}
