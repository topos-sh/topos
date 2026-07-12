import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, useFetcher, useLoaderData } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, PageHeader } from "@/components/ui";
import { notFound, requireMember, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { requireStepUp, requireTypedName } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { archivedSkillById, archivedSkillsOf } from "@/lib/db/queries.lifecycle.server";
import { deleteSkill, unarchiveSkill } from "@/lib/plane/lifecycle.server";
import { deleteDeniedCopy, unarchiveDeniedCopy } from "@/lib/plane/lifecycle-copy";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Archived skills · ${params.ws ?? "Workspace"}` }];
}

/**
 * The workspace's archived skills — a MEMBER-visible list (requireMember; the archive is honest
 * history, not a secret), with OWNER-only per-row actions: UNARCHIVE (restore the base name) and
 * DELETE (drop the server-side bytes — a step further, and gated by typing the archived name). The
 * two writes ride the vault's internal lane keyed on the immutable skill id; the loader hands the
 * page the owner flag so a plain member sees the list without the action controls.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const archived = await archivedSkillsOf(actor);
  return {
    ws,
    isOwner: actor.role === "owner",
    archived: archived.map((row) => ({
      skillId: row.skillId,
      name: row.name,
      baseName: row.baseName,
      archivedAt: row.archivedAtMs !== null ? new Date(row.archivedAtMs).toISOString() : null,
    })),
  };
}

/** One typed inline error per row action (success revalidates the loader; the row re-renders). */
interface ArchiveFormError {
  op: "unarchive" | "delete";
  skillId: string;
  message: string;
}

/**
 * The two owner ceremonies, dispatched on the hidden `intent`. Each RE-GUARDS as owner, re-proves
 * the password (step-up), and — for DELETE — requires typing the ARCHIVED name (re-read from the
 * database, never trusted from the form) before the byte drop. The vault keys on the immutable
 * skill id; one admin_event lands whatever the outcome.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "unarchive") {
    return unarchiveIntent(request, ws, formData);
  }
  if (intent === "delete") {
    return deleteIntent(request, ws, formData);
  }
  return data<ArchiveFormError>(
    { op: "unarchive", skillId: "", message: "Unknown action." },
    { status: 400 },
  );
}

async function unarchiveIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const skillId = String(formData.get("skill_id") ?? "");
  const row = await archivedSkillById(owner, skillId);
  const subject = row?.name ?? skillId;
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, { kind: "unarchive", subject, outcome: "denied" });
    return data<ArchiveFormError>({ op: "unarchive", skillId, message: stepUp.error });
  }
  if (row === undefined) {
    return data<ArchiveFormError>({
      op: "unarchive",
      skillId,
      message: "This skill is no longer archived.",
    });
  }
  const outcome = await unarchiveSkill(owner.email, ws, skillId);
  if (outcome === null) {
    await recordAdminEvent(owner, { kind: "unarchive", subject, outcome: "error" });
    return data<ArchiveFormError>({
      op: "unarchive",
      skillId,
      message: "That didn't go through — nothing was unarchived. A retry is safe.",
    });
  }
  if (outcome.outcome === "unarchived") {
    await recordAdminEvent(owner, {
      kind: "unarchive",
      subject,
      detail: outcome.name,
      outcome: "ok",
    });
    return data<ArchiveFormError>({ op: "unarchive", skillId, message: "" });
  }
  await recordAdminEvent(owner, { kind: "unarchive", subject, outcome: "denied" });
  return data<ArchiveFormError>({
    op: "unarchive",
    skillId,
    message: unarchiveDeniedCopy(outcome.reason),
  });
}

async function deleteIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const skillId = String(formData.get("skill_id") ?? "");
  const row = await archivedSkillById(owner, skillId);
  const subject = row?.name ?? skillId;
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, { kind: "delete", subject, outcome: "denied" });
    return data<ArchiveFormError>({ op: "delete", skillId, message: stepUp.error });
  }
  if (row === undefined) {
    return data<ArchiveFormError>({
      op: "delete",
      skillId,
      message: "This skill is no longer archived.",
    });
  }
  // The typed-name second factor: it must equal the ARCHIVED name exactly (anchored to the row
  // the server re-read, never a form-supplied expected value).
  const typed = requireTypedName(formData, row.name);
  if (!typed.ok) {
    await recordAdminEvent(owner, { kind: "delete", subject, outcome: "denied" });
    return data<ArchiveFormError>({ op: "delete", skillId, message: typed.error });
  }
  const outcome = await deleteSkill(owner.email, ws, skillId);
  if (outcome === null) {
    await recordAdminEvent(owner, { kind: "delete", subject, outcome: "error" });
    return data<ArchiveFormError>({
      op: "delete",
      skillId,
      message: "That didn't go through — nothing was deleted. A retry is safe.",
    });
  }
  if (outcome.outcome === "deleted") {
    await recordAdminEvent(owner, { kind: "delete", subject, outcome: "ok" });
    return data<ArchiveFormError>({ op: "delete", skillId, message: "" });
  }
  await recordAdminEvent(owner, { kind: "delete", subject, outcome: "denied" });
  return data<ArchiveFormError>({
    op: "delete",
    skillId,
    message: deleteDeniedCopy(outcome.reason),
  });
}

export default function WorkspaceArchive() {
  const { ws, isOwner, archived } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-6">
      <PageHeader title="Archived skills" meta={<code className="font-mono">{ws}</code>} />
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
  const fetcher = useFetcher<ArchiveFormError>();
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
        <StepUpFields idPrefix={`unarchive-${skillId}`} />
        {message !== undefined && message.length > 0 && (
          <p className="text-red-600 text-sm" role="alert">
            {message}
          </p>
        )}
        <div>
          <button type="submit" disabled={pending} className={buttonClasses("quiet")}>
            {pending ? "Unarchiving…" : "Unarchive"}
          </button>
        </div>
      </fetcher.Form>
    </details>
  );
}

function DeleteControl({ skillId, archivedName }: { skillId: string; archivedName: string }) {
  const fetcher = useFetcher<ArchiveFormError>();
  const pending = fetcher.state !== "idle";
  const message =
    fetcher.data?.op === "delete" && fetcher.data.skillId === skillId
      ? fetcher.data.message
      : undefined;
  return (
    <details className="flex-1">
      <summary className="cursor-pointer select-none font-mono text-red-700 text-xs hover:text-red-800">
        Delete permanently…
      </summary>
      <fetcher.Form method="post" className="mt-2 space-y-3">
        <input type="hidden" name="intent" value="delete" />
        <input type="hidden" name="skill_id" value={skillId} />
        <p className="text-dim text-xs">
          Deletion drops this skill&apos;s bytes from the server. It cannot recall the copies
          devices already hold — the fleet page shows who still holds them.
        </p>
        <StepUpFields idPrefix={`delete-${skillId}`} typedName={archivedName} />
        {message !== undefined && message.length > 0 && (
          <p className="text-red-600 text-sm" role="alert">
            {message}
          </p>
        )}
        <div>
          <button type="submit" disabled={pending} className={buttonClasses("danger")}>
            {pending ? "Deleting…" : "Delete permanently"}
          </button>
        </div>
      </fetcher.Form>
    </details>
  );
}
