import { vaultFetch } from "./client.server";
import type {
  ArchiveOutcome,
  DeleteOutcome,
  PurgeOutcome,
  RenameOutcome,
  UnarchiveOutcome,
} from "./wire";

/**
 * The skill LIFECYCLE write helpers — the vault side of the owner ceremonies (archive / unarchive /
 * delete / purge / rename). Every call rides the internal session lane keyed on the immutable
 * `skillId` (the catalog name is resolved to it in the route loader; a concurrent rename becomes a
 * harmless miss, never a wrong-target act), and the acting identity is the session-verified email —
 * threaded explicitly by the caller. The vault re-verifies the acting principal's confirmed OWNER
 * seat in-transaction, so this tier's guard is the matching lock, never the authority.
 *
 * Each returns the parsed typed OUTCOME (the lane answers writes 200-for-all-outcomes) or `null`
 * when the vault faulted or answered a non-2xx — the route maps `null` to the honest error state.
 * The pure `*DeniedCopy` helpers below turn a guarded-function outcome code into inline copy; they
 * hold NO fetch and are unit-tested directly.
 */

async function postLifecycle<T>(
  template: string,
  params: Record<string, string>,
  actingEmail: string,
  body: unknown,
): Promise<T | null> {
  try {
    const res = await vaultFetch({ method: "POST", template, params, actingEmail, body });
    return res.ok ? ((await res.json()) as T) : null;
  } catch {
    return null;
  }
}

export function archiveSkill(
  actingEmail: string,
  ws: string,
  skillId: string,
): Promise<ArchiveOutcome | null> {
  return postLifecycle(
    "/internal/v1/workspaces/{ws}/skills/{skill}/archive",
    { ws, skill: skillId },
    actingEmail,
    {},
  );
}

export function unarchiveSkill(
  actingEmail: string,
  ws: string,
  skillId: string,
): Promise<UnarchiveOutcome | null> {
  return postLifecycle(
    "/internal/v1/workspaces/{ws}/skills/{skill}/unarchive",
    { ws, skill: skillId },
    actingEmail,
    {},
  );
}

export function deleteSkill(
  actingEmail: string,
  ws: string,
  skillId: string,
): Promise<DeleteOutcome | null> {
  return postLifecycle(
    "/internal/v1/workspaces/{ws}/skills/{skill}/delete",
    { ws, skill: skillId },
    actingEmail,
    {},
  );
}

export function purgeVersion(
  actingEmail: string,
  ws: string,
  skillId: string,
  versionId: string,
): Promise<PurgeOutcome | null> {
  return postLifecycle(
    "/internal/v1/workspaces/{ws}/skills/{skill}/purge",
    { ws, skill: skillId },
    actingEmail,
    { version_id: versionId },
  );
}

export function renameSkill(
  actingEmail: string,
  ws: string,
  skillId: string,
  newName: string,
): Promise<RenameOutcome | null> {
  return postLifecycle(
    "/internal/v1/workspaces/{ws}/skills/{skill}/rename",
    { ws, skill: skillId },
    actingEmail,
    { new_name: newName },
  );
}
