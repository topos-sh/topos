import { useState } from "react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, redirect, useFetcher, useLoaderData } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { requireStepUp } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  archiveBundle,
  renameBundle,
  setBundleProtection,
} from "@/lib/db/queries.lifecycle.server";
import { workspacePolicyOf } from "@/lib/db/queries.policy.server";
import { bundleById, skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { isValidSkillName, renameDeniedCopy, SKILL_NAME_MAX } from "@/lib/plane/lifecycle-copy";

/** The verbatim boundary the archive ceremony must state — what archiving costs and what it keeps. */
const ARCHIVE_BOUNDARY =
  "Archiving retires it for the whole team — devices keep their sidecar copies.";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `Settings · ${params.skill ?? "skill"}` }];
}

/** How the skill's protection resolves for the render: the pin, or the inherited default. */
type ProtectionChoice = "inherit" | "open" | "reviewed";

/**
 * The skill settings page — OWNER-ONLY (requireWorkspaceOwner; a plain member gets the uniform
 * 404, no existence claim). It hosts the identity ceremonies — RENAME (records the old name as
 * a resolving hint) and ARCHIVE (retires the skill, freeing its base name) — plus the
 * PROTECTION pin: `open`/`reviewed` pinned per skill, or inheriting the workspace default. A
 * miss on the catalog name follows the rename hint: an old address that a rename left behind
 * redirects to the live name's settings, so a bookmark keeps working; anything else is the
 * house 404.
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
  const [bundleRow, policy] = await Promise.all([
    bundleById(owner, row.skillId),
    workspacePolicyOf(owner),
  ]);
  const pinned = bundleRow?.protection ?? null;
  return {
    ws,
    skill: row.name,
    protection: (pinned ?? "inherit") as ProtectionChoice,
    protectionDefault: policy.protectionDefault,
  };
}

/** One typed inline error per ceremony — a landed rename/archive redirects away. */
interface SettingsFormError {
  form: "rename" | "archive" | "protection";
  message: string;
}

/**
 * The ceremonies, dispatched on the hidden `intent`. Each RE-GUARDS as owner, re-proves the
 * password (step-up), then runs the app-tier ceremony keyed on the immutable skill id. The
 * ceremonies land their own audit rows in-transaction; the route records the attempts they
 * never see — refused step-ups, typed refusals, faults.
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
  if (intent === "set-protection") {
    return protectionIntent(request, ws, skill, formData);
  }
  return data<SettingsFormError>({ form: "rename", message: "Unknown action." }, { status: 400 });
}

async function renameIntent(request: Request, ws: string, skill: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "skill_renamed",
      subject: skill,
      detail: "step_up",
      outcome: "denied",
    });
    return data<SettingsFormError>({ form: "rename", message: stepUp.error });
  }
  const newName = String(formData.get("new_name") ?? "").trim();
  if (!isValidSkillName(newName)) {
    await recordAdminEvent(owner, {
      kind: "skill_renamed",
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
  let outcome: Awaited<ReturnType<typeof renameBundle>>;
  try {
    outcome = await renameBundle(owner, row.skillId, newName);
  } catch {
    await recordAdminEvent(owner, {
      kind: "skill_renamed",
      subject: row.skillId,
      detail: newName,
      outcome: "error",
    });
    return data<SettingsFormError>({
      form: "rename",
      message: "That didn't go through — nothing was renamed. A retry is safe.",
    });
  }
  if (outcome.outcome === "renamed") {
    throw redirect(`/workspaces/${ws}/skills/${outcome.name}/settings`);
  }
  await recordAdminEvent(owner, {
    kind: "skill_renamed",
    subject: row.skillId,
    detail: `${newName} ${outcome.outcome}`,
    outcome: "denied",
  });
  return data<SettingsFormError>({ form: "rename", message: renameDeniedCopy(outcome.outcome) });
}

async function archiveIntent(request: Request, ws: string, skill: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "skill_archived",
      subject: skill,
      detail: "step_up",
      outcome: "denied",
    });
    return data<SettingsFormError>({ form: "archive", message: stepUp.error });
  }
  const row = await skillIndexRow(owner, skill);
  if (row === undefined) {
    return data<SettingsFormError>({ form: "archive", message: "This skill no longer exists." });
  }
  let outcome: Awaited<ReturnType<typeof archiveBundle>>;
  try {
    outcome = await archiveBundle(owner, row.skillId);
  } catch {
    await recordAdminEvent(owner, {
      kind: "skill_archived",
      subject: row.skillId,
      outcome: "error",
    });
    return data<SettingsFormError>({
      form: "archive",
      message: "That didn't go through — nothing was archived. A retry is safe.",
    });
  }
  if (outcome.outcome === "archived") {
    throw redirect(`/workspaces/${ws}/archive`);
  }
  await recordAdminEvent(owner, {
    kind: "skill_archived",
    subject: row.skillId,
    detail: outcome.outcome,
    outcome: "denied",
  });
  return data<SettingsFormError>({
    form: "archive",
    message:
      outcome.outcome === "not_active"
        ? "This skill isn't active — only an active skill archives."
        : "This skill no longer exists.",
  });
}

/** The protection pin — owner + step-up; 'inherit' clears the pin back to the workspace default. */
async function protectionIntent(request: Request, ws: string, skill: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const choice = String(formData.get("protection") ?? "");
  if (choice !== "inherit" && choice !== "open" && choice !== "reviewed") {
    return data<SettingsFormError>({ form: "protection", message: "Unknown protection value." });
  }
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "skill_protection",
      subject: skill,
      detail: "step_up",
      outcome: "denied",
    });
    return data<SettingsFormError>({ form: "protection", message: stepUp.error });
  }
  const row = await skillIndexRow(owner, skill);
  if (row === undefined) {
    return data<SettingsFormError>({ form: "protection", message: "This skill no longer exists." });
  }
  let outcome: Awaited<ReturnType<typeof setBundleProtection>>;
  try {
    outcome = await setBundleProtection(owner, row.skillId, choice === "inherit" ? null : choice);
  } catch {
    await recordAdminEvent(owner, {
      kind: "skill_protection",
      subject: row.skillId,
      detail: choice,
      outcome: "error",
    });
    return data<SettingsFormError>({
      form: "protection",
      message: "That didn't go through — nothing changed. A retry is safe.",
    });
  }
  if (outcome.outcome === "set") {
    return data<SettingsFormError>({ form: "protection", message: "" });
  }
  return data<SettingsFormError>({ form: "protection", message: "This skill no longer exists." });
}

export default function SkillSettings() {
  const { ws, skill, protection, protectionDefault } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader title={`${skill} settings`} meta={<code className="font-mono">{ws}</code>} />
      <ProtectionCeremony
        skill={skill}
        protection={protection}
        protectionDefault={protectionDefault}
      />
      <RenameCeremony skill={skill} />
      <ArchiveCeremony skill={skill} />
    </div>
  );
}

const PROTECTION_LABEL: Record<ProtectionChoice, string> = {
  inherit: "Inherit the workspace default",
  open: "Open — any member's publish lands directly",
  reviewed: "Reviewed — a member's publish becomes a proposal",
};

/**
 * The protection pin. `checked` is the stored pin ('inherit' = no pin); the staged choice
 * reveals the step-up confirm only when it differs, so the knob at rest shows no password
 * prompt. The copy names the cascade honestly: the pin overrides the workspace default, and
 * clearing it returns to inheriting.
 */
function ProtectionCeremony({
  skill,
  protection,
  protectionDefault,
}: {
  skill: string;
  protection: ProtectionChoice;
  protectionDefault: "open" | "reviewed";
}) {
  const fetcher = useFetcher<SettingsFormError>();
  const pending = fetcher.state !== "idle";
  const [staged, setStaged] = useState<ProtectionChoice>(protection);
  const dirty = staged !== protection;
  const error =
    fetcher.data?.form === "protection" && fetcher.data.message.length > 0
      ? fetcher.data.message
      : undefined;
  return (
    <section aria-labelledby="protection-heading" className="space-y-3">
      <SectionHeading>
        <span id="protection-heading">Protection</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-4">
        <p className="text-dim text-sm">
          Whether a member&apos;s publish to <code className="font-mono text-ink">{skill}</code>{" "}
          lands directly or reroutes into a proposal a reviewer approves. The workspace default is{" "}
          <span className="font-medium text-ink">{protectionDefault}</span>; a pin here overrides it
          for this skill alone.
        </p>
        <fetcher.Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="set-protection" />
          <fieldset className="space-y-2">
            <legend className="sr-only">Protection</legend>
            {(["inherit", "open", "reviewed"] as const).map((option) => (
              <label key={option} className="flex items-center gap-2 text-ink text-sm">
                <input
                  type="radio"
                  name="protection"
                  value={option}
                  checked={staged === option}
                  disabled={pending}
                  onChange={() => setStaged(option)}
                  className="accent-accent"
                />
                {PROTECTION_LABEL[option]}
                {option === "inherit" && (
                  <span className="text-faint text-xs">(currently {protectionDefault})</span>
                )}
              </label>
            ))}
          </fieldset>
          {dirty && (
            <div className="space-y-3 border-line-soft border-t pt-3">
              <StepUpFields idPrefix="protection" />
              {error !== undefined && (
                <p className="text-red-600 text-sm" role="alert">
                  {error}
                </p>
              )}
              <div className="flex items-center gap-2">
                <button type="submit" disabled={pending} className={buttonClasses("primary")}>
                  {pending ? "Saving…" : "Save protection"}
                </button>
                <button
                  type="button"
                  disabled={pending}
                  onClick={() => setStaged(protection)}
                  className={buttonClasses("quiet")}
                >
                  Cancel
                </button>
              </div>
            </div>
          )}
        </fetcher.Form>
      </Card>
    </section>
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
