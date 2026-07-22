import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Form, useActionData, useFetcher, useLoaderData, useNavigation } from "react-router";
import { ConfirmButton, ConfirmNameField } from "@/components/confirm";
import { SettingsTabs } from "@/components/settings-tabs";
import { buttonClasses, Card, PageHeader } from "@/components/ui";
import { requireTypedName } from "@/lib/auth/ceremony.server";
import { requireMemberInScope, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  archivedSkillById,
  archivedSkillsOf,
  deleteBundle,
  unarchiveBundle,
} from "@/lib/db/queries.lifecycle.server";
import { deleteDeniedCopy, unarchiveDeniedCopy } from "@/lib/plane/lifecycle-copy";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Archived skills · ${params.ws ?? "Workspace"}` }];
}

/**
 * The workspace's archived skills — a MEMBER-visible list (requireMember; the archive is honest
 * history, not a secret), with OWNER-only per-row actions: UNARCHIVE (restore the base name) and
 * DELETE (tombstone the row and drop the server-side bytes — a step further, and gated by typing
 * the archived name). Both are app-tier ceremonies keyed on the immutable skill id; the loader
 * hands the page the owner flag so a plain member sees the list without the action controls.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const archived = await archivedSkillsOf(actor);
  return {
    wsName: workspace.name,
    isOwner: actor.role === "owner",
    archived: archived.map((row) => ({
      skillId: row.skillId,
      name: row.name,
      baseName: row.baseName,
      archivedAt: row.archivedAtMs !== null ? new Date(row.archivedAtMs).toISOString() : null,
    })),
  };
}

/** The two ceremonies' typed replies — unarchive rides a fetcher; delete a full-page post. */
type ArchiveActionData =
  | { op: "unarchive"; skillId: string; message: string }
  | { op: "delete"; skillId: string; status: "deleted"; name: string; bytesDropped: boolean }
  | { op: "delete"; skillId: string; status: "refused"; message: string };

/**
 * The two owner ceremonies, dispatched on the hidden `intent`. Each RE-GUARDS as owner — the
 * owner guard is the whole gate (no re-authentication) — and DELETE additionally requires typing
 * the ARCHIVED name (re-read from the database, never trusted from the form); unarchive wears a
 * client-side in-place confirm. The ceremonies land their own audit rows in-transaction; the
 * route records the attempts they never see — typed names that miss, faults.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The membership FLOOR, hoisted above the intent dispatch: every intent below requires at
  // least a member (most re-check owner/reviewer themselves), and the unmatched-intent 400 must
  // never answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a
  // 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  const { workspace } = await requireMemberInScope(request, params);
  const ws = workspace.id;
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "unarchive") {
    return unarchiveIntent(request, ws, formData);
  }
  if (intent === "delete") {
    return deleteIntent(request, ws, formData);
  }
  return data<ArchiveActionData>(
    { op: "unarchive", skillId: "", message: "Unknown action." },
    { status: 400 },
  );
}

async function unarchiveIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const skillId = String(formData.get("skill_id") ?? "");
  const row = await archivedSkillById(owner, skillId);
  const subject = row?.name ?? skillId;
  if (row === undefined) {
    return data<ArchiveActionData>({
      op: "unarchive",
      skillId,
      message: "This skill is no longer archived.",
    });
  }
  let outcome: Awaited<ReturnType<typeof unarchiveBundle>>;
  try {
    outcome = await unarchiveBundle(owner, skillId);
  } catch {
    await recordAdminEvent(owner, { kind: "skill_unarchived", subject, outcome: "error" });
    return data<ArchiveActionData>({
      op: "unarchive",
      skillId,
      message: "That didn't go through — nothing was unarchived. A retry is safe.",
    });
  }
  if (outcome.outcome === "unarchived") {
    return data<ArchiveActionData>({ op: "unarchive", skillId, message: "" });
  }
  await recordAdminEvent(owner, {
    kind: "skill_unarchived",
    subject,
    detail: outcome.outcome,
    outcome: "denied",
  });
  return data<ArchiveActionData>({
    op: "unarchive",
    skillId,
    message: unarchiveDeniedCopy(outcome.outcome),
  });
}

async function deleteIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const skillId = String(formData.get("skill_id") ?? "");
  const row = await archivedSkillById(owner, skillId);
  const subject = row?.name ?? skillId;
  if (row === undefined) {
    return data<ArchiveActionData>({
      op: "delete",
      skillId,
      status: "refused",
      message: "This skill is no longer archived.",
    });
  }
  // The typed-name second factor: it must equal the ARCHIVED name exactly (anchored to the row
  // the server re-read, never a form-supplied expected value).
  const typed = requireTypedName(formData, row.name);
  if (!typed.ok) {
    await recordAdminEvent(owner, {
      kind: "skill_deleted",
      subject,
      detail: "confirm_name",
      outcome: "denied",
    });
    return data<ArchiveActionData>({
      op: "delete",
      skillId,
      status: "refused",
      message: typed.error,
    });
  }
  let outcome: Awaited<ReturnType<typeof deleteBundle>>;
  try {
    outcome = await deleteBundle(owner, skillId);
  } catch {
    await recordAdminEvent(owner, { kind: "skill_deleted", subject, outcome: "error" });
    return data<ArchiveActionData>({
      op: "delete",
      skillId,
      status: "refused",
      message: "That didn't go through — nothing was deleted. A retry is safe.",
    });
  }
  if (outcome.outcome === "deleted") {
    return data<ArchiveActionData>({
      op: "delete",
      skillId,
      status: "deleted",
      name: row.name,
      bytesDropped: outcome.bytesDropped,
    });
  }
  await recordAdminEvent(owner, {
    kind: "skill_deleted",
    subject,
    detail: outcome.outcome,
    outcome: "denied",
  });
  return data<ArchiveActionData>({
    op: "delete",
    skillId,
    status: "refused",
    message: deleteDeniedCopy(outcome.outcome),
  });
}

export default function WorkspaceArchive() {
  const { wsName, isOwner, archived } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  const deleted =
    actionData?.op === "delete" && actionData.status === "deleted" ? actionData : null;
  return (
    <div className="space-y-6">
      <PageHeader title="Archive" meta={<code className="font-mono">{wsName}</code>} />
      <SettingsTabs active="archive" />
      {deleted !== null && (
        <Card className="px-4 py-3">
          <p className="text-dim text-sm" role="status">
            Deleted <span className="font-mono text-ink">{deleted.name}</span>.{" "}
            {deleted.bytesDropped
              ? "Its bytes are reclaimed from the server; the row stays as a tombstone so history survives."
              : "The row is tombstoned; the byte reclaim faulted and will be retried — running the delete again is safe."}
          </p>
        </Card>
      )}
      {archived.length === 0 ? (
        <Card className="px-4 py-3">
          <p className="text-dim text-sm">
            Nothing archived. Retiring a skill from its settings page moves it here.
          </p>
        </Card>
      ) : (
        <ul className="space-y-3">
          {archived.map((row) => (
            <ArchivedRow key={row.skillId} row={row} isOwner={isOwner} />
          ))}
        </ul>
      )}
    </div>
  );
}

function ArchivedRow({
  row,
  isOwner,
}: {
  row: { skillId: string; name: string; baseName: string | null; archivedAt: string | null };
  isOwner: boolean;
}) {
  return (
    <li>
      <Card className="space-y-3 px-4 py-4">
        <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
          <span className="font-mono text-ink text-sm">{row.name}</span>
          {row.baseName !== null && (
            <span className="text-faint text-xs">
              was <span className="font-mono">{row.baseName}</span>
            </span>
          )}
          {row.archivedAt !== null && (
            <span className="text-faint text-xs">archived {row.archivedAt.slice(0, 10)}</span>
          )}
        </div>
        {isOwner && (
          <div className="flex flex-col gap-4 border-line-soft border-t pt-3 sm:flex-row sm:items-start">
            <UnarchiveControl skillId={row.skillId} />
            <DeleteControl skillId={row.skillId} archivedName={row.name} />
          </div>
        )}
      </Card>
    </li>
  );
}

function UnarchiveControl({ skillId }: { skillId: string }) {
  const fetcher = useFetcher<Extract<ArchiveActionData, { op: "unarchive" }>>();
  const pending = fetcher.state !== "idle";
  const message =
    fetcher.data?.op === "unarchive" && fetcher.data.skillId === skillId
      ? fetcher.data.message
      : undefined;
  return (
    <details className="flex-1">
      <summary className="cursor-pointer select-none font-mono text-dim text-xs hover:text-ink">
        Unarchive…
      </summary>
      <fetcher.Form method="post" className="mt-2 space-y-3">
        <input type="hidden" name="intent" value="unarchive" />
        <input type="hidden" name="skill_id" value={skillId} />
        <p className="text-dim text-xs">
          Restores the base name and puts the skill back in the catalog. If the name was reused
          since, this is refused — rename after unarchiving.
        </p>
        {message !== undefined && message.length > 0 && (
          <p className="text-red-600 text-sm" role="alert">
            {message}
          </p>
        )}
        <div>
          <ConfirmButton
            label="Unarchive"
            confirmLabel="Unarchive — confirm?"
            tone="quiet"
            pendingLabel="Unarchiving…"
            pending={pending}
          />
        </div>
      </fetcher.Form>
    </details>
  );
}

/**
 * The delete ceremony posts as a FULL-PAGE form (not a fetcher): a landed delete revalidates the
 * row away, and the outcome — including whether the bytes actually dropped — must survive that
 * unmount, so the page-level banner above renders it. Refusals leave the row standing and render
 * inline here.
 */
function DeleteControl({ skillId, archivedName }: { skillId: string; archivedName: string }) {
  const navigation = useNavigation();
  const actionData = useActionData<typeof action>();
  const pending =
    navigation.state !== "idle" &&
    navigation.formData?.get("intent") === "delete" &&
    navigation.formData?.get("skill_id") === skillId;
  const message =
    actionData?.op === "delete" && actionData.status === "refused" && actionData.skillId === skillId
      ? actionData.message
      : undefined;
  return (
    <details className="flex-1">
      <summary className="cursor-pointer select-none font-mono text-red-700 text-xs hover:text-red-800">
        Delete permanently…
      </summary>
      <Form method="post" className="mt-2 space-y-3">
        <input type="hidden" name="intent" value="delete" />
        <input type="hidden" name="skill_id" value={skillId} />
        <p className="text-dim text-xs">
          Deletion drops this skill&apos;s bytes from the server. It cannot recall the copies
          devices already hold — the fleet page shows who still holds them.
        </p>
        <ConfirmNameField typedName={archivedName} idPrefix={`delete-${skillId}`} />
        {message !== undefined && (
          <p className="text-red-600 text-sm" role="alert">
            {message}
          </p>
        )}
        <div>
          <button type="submit" disabled={pending} className={buttonClasses("danger")}>
            {pending ? "Deleting…" : "Delete permanently"}
          </button>
        </div>
      </Form>
    </details>
  );
}
