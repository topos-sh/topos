import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, redirect, useFetcher, useLoaderData } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { requireStepUp } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { archiveSkill, renameSkill } from "@/lib/plane/lifecycle.server";
import { isValidSkillName, renameDeniedCopy, SKILL_NAME_MAX } from "@/lib/plane/lifecycle-copy";

/** The verbatim boundary the archive ceremony must state — what archiving costs and what it keeps. */
const ARCHIVE_BOUNDARY =
  "Archiving retires it for the whole team — devices keep their sidecar copies.";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `Settings · ${params.skill ?? "skill"}` }];
}

/**
 * The skill settings page — OWNER-ONLY (requireWorkspaceOwner; a plain member gets the uniform
 * 404, no existence claim). It hosts the two identity ceremonies: RENAME (records the old name as
 * a resolving hint) and ARCHIVE (retires the skill, freeing its base name). A miss on the catalog
 * name follows the rename hint: an old address that a rename left behind redirects to the live
 * name's settings, so a bookmark keeps working; anything else is the house 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  const skill = params.skill;
  if (!ws || !skill) {
    notFound();
  }
  const owner = await requireWorkspaceOwner(request, ws);
  const row = await skillIndexRow(owner, skill);
  if (row === undefined) {
    const resolved = await resolveSkillName(owner, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(`/workspaces/${ws}/skills/${resolved.name}/settings`);
    }
    notFound();
  }
  return { ws, skill: row.name };
}

/** One typed inline error per ceremony — success is a redirect (rename → new name, archive → archive). */
interface SettingsFormError {
  form: "rename" | "archive";
  message: string;
}

/**
 * The two ceremonies, dispatched on the hidden `intent`. Each RE-GUARDS as owner, re-proves the
 * password (step-up), then rides the vault's internal lane keyed on the immutable skill id — and
 * records ONE admin_event whatever the outcome (a refused step-up is a fact too). The vault's own
 * owner gate stays the authority; this action relays.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  const skill = params.skill;
  if (!ws || !skill) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "rename") {
    return renameIntent(request, ws, skill, formData);
  }
  if (intent === "archive") {
    return archiveIntent(request, ws, skill, formData);
  }
  return data<SettingsFormError>({ form: "rename", message: "Unknown action." }, { status: 400 });
}

async function renameIntent(request: Request, ws: string, skill: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, { kind: "rename", subject: skill, outcome: "denied" });
    return data<SettingsFormError>({ form: "rename", message: stepUp.error });
  }
  const newName = String(formData.get("new_name") ?? "").trim();
  if (!isValidSkillName(newName)) {
    await recordAdminEvent(owner, {
      kind: "rename",
      subject: skill,
      detail: newName,
      outcome: "denied",
    });
    return data<SettingsFormError>({ form: "rename", message: renameDeniedCopy("bad_name") });
  }
  const row = await skillIndexRow(owner, skill);
  if (row === undefined) {
    return data<SettingsFormError>({ form: "rename", message: "This skill no longer exists." });
  }
  const outcome = await renameSkill(owner.email, ws, row.skillId, newName);
  if (outcome === null) {
    await recordAdminEvent(owner, {
      kind: "rename",
      subject: skill,
      detail: newName,
      outcome: "error",
    });
    return data<SettingsFormError>({
      form: "rename",
      message: "That didn't go through — nothing was renamed. A retry is safe.",
    });
  }
  if (outcome.outcome === "renamed") {
    const live = outcome.name ?? newName;
    await recordAdminEvent(owner, { kind: "rename", subject: skill, detail: live, outcome: "ok" });
    throw redirect(`/workspaces/${ws}/skills/${live}/settings`);
  }
  await recordAdminEvent(owner, {
    kind: "rename",
    subject: skill,
    detail: newName,
    outcome: "denied",
  });
  return data<SettingsFormError>({ form: "rename", message: renameDeniedCopy(outcome.reason) });
}

async function archiveIntent(request: Request, ws: string, skill: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, { kind: "archive", subject: skill, outcome: "denied" });
    return data<SettingsFormError>({ form: "archive", message: stepUp.error });
  }
  const row = await skillIndexRow(owner, skill);
  if (row === undefined) {
    return data<SettingsFormError>({ form: "archive", message: "This skill no longer exists." });
  }
  const outcome = await archiveSkill(owner.email, ws, row.skillId);
  if (outcome === null) {
    await recordAdminEvent(owner, { kind: "archive", subject: skill, outcome: "error" });
    return data<SettingsFormError>({
      form: "archive",
      message: "That didn't go through — nothing was archived. A retry is safe.",
    });
  }
  if (outcome.outcome === "archived") {
    await recordAdminEvent(owner, {
      kind: "archive",
      subject: skill,
      detail: outcome.archived_name,
      outcome: "ok",
    });
    throw redirect(`/workspaces/${ws}/archive`);
  }
  await recordAdminEvent(owner, { kind: "archive", subject: skill, outcome: "denied" });
  return data<SettingsFormError>({ form: "archive", message: "The server declined this archive." });
}

export default function SkillSettings() {
  const { ws, skill } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader title={`${skill} settings`} meta={<code className="font-mono">{ws}</code>} />
      <RenameCeremony skill={skill} />
      <ArchiveCeremony skill={skill} />
    </div>
  );
}

function RenameCeremony({ skill }: { skill: string }) {
  const fetcher = useFetcher<SettingsFormError>();
  const pending = fetcher.state !== "idle";
  const error = fetcher.data?.form === "rename" ? fetcher.data.message : undefined;
  return (
    <section aria-labelledby="rename-heading" className="space-y-3">
      <SectionHeading>
        <span id="rename-heading">Rename</span>
      </SectionHeading>
      <Card className="space-y-4 px-4 py-4">
        <p className="text-dim text-sm">
          Renaming <code className="font-mono text-ink">{skill}</code> keeps the old name resolving
          as a redirect — a bookmark or a running command that used it keeps working until someone
          claims the name for a new skill.
        </p>
        <fetcher.Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="rename" />
          <label className="block" htmlFor="rename-new-name">
            <span className="mb-1 block font-medium text-dim text-sm">New name</span>
            <input
              id="rename-new-name"
              type="text"
              name="new_name"
              required
              pattern="[a-z0-9][a-z0-9-]*"
              maxLength={SKILL_NAME_MAX}
              autoComplete="off"
              spellCheck={false}
              placeholder="lowercase-with-hyphens"
              className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
            />
          </label>
          <StepUpFields idPrefix="rename" />
          {error !== undefined && (
            <p className="text-red-600 text-sm" role="alert">
              {error}
            </p>
          )}
          <div>
            <button type="submit" disabled={pending} className={buttonClasses("primary")}>
              {pending ? "Renaming…" : "Rename skill"}
            </button>
          </div>
        </fetcher.Form>
      </Card>
    </section>
  );
}

function ArchiveCeremony({ skill }: { skill: string }) {
  const fetcher = useFetcher<SettingsFormError>();
  const pending = fetcher.state !== "idle";
  const error = fetcher.data?.form === "archive" ? fetcher.data.message : undefined;
  return (
    <section aria-labelledby="archive-heading" className="space-y-3">
      <SectionHeading>
        <span id="archive-heading">Archive</span>
      </SectionHeading>
      <Card className="space-y-4 px-4 py-4">
        <p className="text-dim text-sm">{ARCHIVE_BOUNDARY}</p>
        <p className="text-faint text-sm">
          The base name is freed for reuse and the skill drops off every device&apos;s next update;
          you can unarchive it from the archive page.
        </p>
        <fetcher.Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="archive" />
          <StepUpFields idPrefix="archive" />
          {error !== undefined && (
            <p className="text-red-600 text-sm" role="alert">
              {error}
            </p>
          )}
          <div>
            <button type="submit" disabled={pending} className={buttonClasses("danger")}>
              {pending ? "Archiving…" : `Archive ${skill}`}
            </button>
          </div>
        </fetcher.Form>
      </Card>
    </section>
  );
}
