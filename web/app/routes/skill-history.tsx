import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, redirect, useLoaderData } from "react-router";
import { HistorySection, type HistorySectionData } from "@/components/skill/history-section";
import { type PurgeActionData, PurgeSection } from "@/components/skill/purge-section";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { requireTypedName } from "@/lib/auth/ceremony.server";
import {
  notFound,
  requireMemberInScope,
  requireReviewer,
  requireWorkspaceOwner,
} from "@/lib/auth/guards.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { purgeVersion } from "@/lib/db/queries.lifecycle.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { revertPointer } from "@/lib/plane/custody.server";
import { walkHistory } from "@/lib/plane/history.server";
import { purgeDeniedCopy } from "@/lib/plane/lifecycle-copy";
import { custodyVersionMeta } from "@/lib/plane/reads.server";
import { allowRevertWrite } from "@/lib/rate-limit.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

const DEPTH = 10;
const HEX64 = /^[0-9a-f]{64}$/;

/**
 * A generation is ONE canonical base-10 integer in 0..=Number.MAX_SAFE_INTEGER — the pointer's
 * CAS counter lives under 2^53−1 (16 digits), so BOTH belts run: the canonical-form regex and
 * the numeric ceiling.
 */
const GEN_RE = /^(0|[1-9][0-9]{0,15})$/;

function parseGeneration(value: string): number | undefined {
  if (!GEN_RE.test(value)) {
    return undefined;
  }
  const n = Number(value);
  return n <= Number.MAX_SAFE_INTEGER ? n : undefined;
}

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `${params.skill ?? "skill"} · history · Topos` }];
}

/** The typed reply the per-row RevertControl fetcher reads back (intent=revert). */
interface RevertActionData {
  status: "reverted" | "conflict" | "denied" | "error";
  /** On `denied`, the display copy the control renders. */
  reason?: string;
}

/**
 * The skill's History tab — the first-parent version walk as its own shareable route (a sibling
 * of Current and Proposals), carrying the `?from=` cursor for Show-older / second-parent paging.
 * Same guard-then-probe order as every skill page: requireMember before any data, then the DB
 * catalog probe as the uniform 404 (an unknown NAME), with an honest empty body when the name
 * exists but nothing is published yet.
 *
 * The walk runs HERE in the loader (the pure walk from history.server.ts, its metadata fetcher
 * on the internal custody lane over the immutable skillId) so HistorySection renders as a plain
 * component. The catalog row IS the current pointer, so its version id is the walk's head and
 * its generation — ONE number — is the CAS binding every roll-back carries. The `from` cursor is
 * HEX64-gated here.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const ws = workspace.id;
  const skill = params.skill as string;
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    // A rename left an old name behind: follow the resolving hint to the live name's History tab
    // (preserving the paging cursor); anything else is the house 404.
    const resolved = await resolveSkillName(actor, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(
        wsPathServer(workspace.name, `skills/${resolved.name}/history`) +
          new URL(request.url).search,
      );
    }
    notFound();
  }

  const rawFrom = new URL(request.url).searchParams.get("from") ?? undefined;
  const resume = rawFrom !== undefined && HEX64.test(rawFrom) ? rawFrom : undefined;

  let history: HistorySectionData;
  if (row.versionId === null || row.generation === null) {
    // The name exists but nothing is published — no head to walk from.
    history = { published: false };
  } else {
    // The roll-back affordance is owner|reviewer-only (the action's requireReviewer is the
    // authority — this is the matching render lock), and it binds the LIVE current generation:
    // a revert is a CAS on `current`, so every non-head row shares the same target.
    const page = await walkHistory(
      async (versionId) => {
        const m = await custodyVersionMeta(ws, row.skillId, versionId);
        return m.ok ? { ok: true, data: m.data } : { ok: false };
      },
      row.versionId,
      { depth: DEPTH, from: resume },
    );
    history = {
      published: true,
      head: row.versionId,
      canRevert: actor.role !== "member",
      expectedGeneration: String(row.generation),
      steps: page.steps,
      cursor: page.cursor,
      truncated: page.truncated,
    };
  }

  return {
    isOwner: actor.role === "owner",
    wsName: workspace.name,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    kind: row.kind,
    openProposals: row.openProposals,
    history,
    // The purge affordance is a workspace-OWNER ceremony; a plain member/reviewer never sees it.
    canPurge: actor.role === "owner",
  };
}

/**
 * The History tab's writes, dispatched on the hidden `intent`: REVERT (owner|reviewer roll-back
 * to a known-good version — a forward move the vault expresses as a new commit carrying the good
 * tree; worn as a client-side in-place confirm) and PURGE (an owner-only ceremony that drops one
 * past version's bytes, gated by typing the skill name). Each branch RE-GUARDS itself; React
 * Router revalidates the loader after either — no explicit invalidation.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  // The membership FLOOR, hoisted above the intent dispatch: every intent below requires at
  // least a member (most re-check owner/reviewer themselves), and the unmatched-intent 400 must
  // never answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a
  // 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  const { workspace } = await requireMemberInScope(request, params);
  const ws = workspace.id;
  const skill = params.skill as string;
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");
  if (intent === "revert") {
    return revertAction(request, ws, skill, form);
  }
  if (intent === "purge") {
    return purgeAction(request, ws, skill, form);
  }
  return data<RevertActionData>({ status: "error" }, { status: 400 });
}

/**
 * The team-revert decision — the per-row RevertControl posts here. Guard (owner|reviewer) FIRST;
 * the belt runs after the guard (a stranger burns no token) and before the vault call, keyed by
 * the guard-minted actor's user id. The CAS binding is the generation the page rendered against —
 * the vault refuses a moved pointer instead of rolling back over something the reviewer didn't
 * see. One admin_event lands per attempt (the vault records only the pass-through display).
 */
async function revertAction(request: Request, ws: string, skill: string, form: FormData) {
  const actor = await requireReviewer(request, ws);
  if (!allowRevertWrite(actor.userId)) {
    return data<RevertActionData>({ status: "error" });
  }

  const good = String(form.get("good_version_id") ?? "").trim();
  const expected = parseGeneration(String(form.get("expected_generation") ?? "").trim());
  if (!HEX64.test(good) || expected === undefined) {
    return data<RevertActionData>({ status: "error" });
  }

  // The vault keys on the immutable skill id, never the catalog name.
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<RevertActionData>({ status: "error" });
  }
  const short = good.slice(0, 12);

  const outcome = await revertPointer(ws, row.skillId, {
    to_version_id: good,
    expected_generation: expected,
    attribution: actor.display,
    // The browser ceremony composes its own frame (no device pre-derives this id) — a
    // deterministic message keeps a double-submit's retry converging on the same commit.
    message: `Revert to ${good}`,
  });
  if (outcome.kind === "fault") {
    await recordAdminEvent(actor, {
      kind: "revert",
      subject: row.skillId,
      detail: short,
      outcome: "error",
    });
    return data<RevertActionData>({ status: "error" });
  }
  await recordAdminEvent(actor, {
    kind: "revert",
    subject: row.skillId,
    detail: short,
    outcome: outcome.kind === "ok" ? "ok" : "denied",
  });
  if (outcome.kind === "ok") {
    return data<RevertActionData>({ status: "reverted" });
  }
  if (outcome.kind === "conflict") {
    return data<RevertActionData>({ status: "conflict" });
  }
  if (outcome.kind === "target_purged") {
    return data<RevertActionData>({
      status: "denied",
      reason: "That version's bytes were purged — there is nothing to roll back to.",
    });
  }
  // not_found / rejected — the id names nothing this skill's custody can roll back to.
  return data<RevertActionData>({
    status: "denied",
    reason: "The server has no version with this id for this skill.",
  });
}

/**
 * The per-version PURGE ceremony — OWNER-only (requireWorkspaceOwner), type-the-skill-name gated.
 * It drops ONE past version's bytes server-side; the hash stays as a tombstone. The ceremony
 * refuses the CURRENT version (`is_current`) — the UI also hides the control on the head row —
 * and re-purging is idempotent. The ceremony lands its own audit row; the route records the
 * attempts it never sees (typed-name misses, refusals). Keys on the immutable skill id, never the
 * catalog name.
 */
async function purgeAction(request: Request, ws: string, skill: string, form: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const versionId = String(form.get("version_id") ?? "")
    .trim()
    .toLowerCase();
  const short = versionId.slice(0, 12);

  const row = await skillIndexRow(owner, skill);
  if (row === undefined) {
    return data<PurgeActionData>({ intent: "purge", status: "error", versionId });
  }
  // The typed second factor: the skill's CURRENT catalog name, re-read from the server (never a
  // form-supplied expected value).
  const typed = requireTypedName(form, row.name);
  if (!typed.ok) {
    await recordAdminEvent(owner, {
      kind: "version_purged",
      subject: row.skillId,
      detail: "confirm_name",
      outcome: "denied",
    });
    return data<PurgeActionData>({
      intent: "purge",
      status: "denied",
      versionId,
      message: typed.error,
    });
  }
  if (!HEX64.test(versionId)) {
    return data<PurgeActionData>({ intent: "purge", status: "error", versionId });
  }
  let outcome: Awaited<ReturnType<typeof purgeVersion>>;
  try {
    outcome = await purgeVersion(owner, row.skillId, versionId);
  } catch {
    outcome = { outcome: "fault" };
  }
  if (outcome.outcome === "fault") {
    await recordAdminEvent(owner, {
      kind: "version_purged",
      subject: row.skillId,
      detail: short,
      outcome: "error",
    });
    return data<PurgeActionData>({ intent: "purge", status: "error", versionId });
  }
  if (outcome.outcome === "purged") {
    return data<PurgeActionData>({ intent: "purge", status: "purged", versionId });
  }
  await recordAdminEvent(owner, {
    kind: "version_purged",
    subject: row.skillId,
    detail: `${short} ${outcome.outcome}`,
    outcome: "denied",
  });
  return data<PurgeActionData>({
    intent: "purge",
    status: "denied",
    versionId,
    message: purgeDeniedCopy(outcome.outcome),
  });
}

export default function SkillHistoryPage() {
  const {
    wsName,
    skill,
    currentShort,
    displayName,
    kind,
    isOwner,
    openProposals,
    history,
    canPurge,
  } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-6">
      <SkillHeader
        ws={wsName}
        skill={skill}
        currentShort={currentShort}
        displayName={displayName}
        kind={kind}
      />
      <SkillTabs
        basePath={wsPath(`skills/${skill}`)}
        active="history"
        openProposals={openProposals}
        showSettings={isOwner}
      />
      <HistorySection skill={skill} data={history} />
      <PurgeSection skill={skill} data={history} canPurge={canPurge} />
    </div>
  );
}
