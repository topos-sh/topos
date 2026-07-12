import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, useLoaderData } from "react-router";
import {
  HistorySection,
  type HistorySectionData,
  type HistoryStepView,
} from "@/components/skill/history-section";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { notFound, requireMember, requireReviewer } from "@/lib/auth/guards.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { vaultFetch } from "@/lib/plane/client.server";
import { REVERT_DENIED_REASONS } from "@/lib/plane/errors";
import { walkHistory } from "@/lib/plane/history.server";
import { sessionVersionMeta } from "@/lib/plane/reads.server";
import type { RevertOutcome } from "@/lib/plane/wire";
import { allowRevertWrite } from "@/lib/rate-limit.server";

const DEPTH = 10;
const HEX64 = /^[0-9a-f]{64}$/;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * A generation half is a canonical base-10 integer in 0..=Number.MAX_SAFE_INTEGER — the vault's
 * counters live under 2^53−1 (16 digits), so BOTH belts run: the canonical-form regex and the
 * numeric ceiling.
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
  /** On `denied`, the display copy the control renders (role-gate substitution or a verbatim reason). */
  reason?: string;
}

/**
 * The skill's History tab — the first-parent version walk as its own shareable route (a sibling of
 * Current and Proposals), carrying the `?from=` cursor for Show-older / second-parent paging. Same
 * guard-then-probe order as every skill page: requireMember before any data, then the DB catalog
 * probe as the uniform 404 (an unknown NAME), with an honest empty body when the name exists but
 * nothing is published yet.
 *
 * The walk runs HERE in the loader (the pure walk from history.server.ts, its metadata fetcher on
 * the member-session lane over the actor + skillId) so HistorySection renders as a plain component.
 * The catalog row IS the current pointer, so its version id is the walk's head and its `(epoch,
 * seq)` is the live generation every roll-back binds — no second pointer read. The `from` cursor is
 * HEX64-gated here; roll-back request ids are minted per row on the server so hydration never
 * re-mints them.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const actor = await requireMember(request, ws);
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }

  const rawFrom = new URL(request.url).searchParams.get("from") ?? undefined;
  const resume = rawFrom !== undefined && HEX64.test(rawFrom) ? rawFrom : undefined;

  let history: HistorySectionData;
  if (row.versionId === null || row.epoch === null || row.seq === null) {
    // The name exists but nothing is published — no head to walk from.
    history = { published: false };
  } else {
    // The roll-back affordance is owner|reviewer-only (the vault's in-transaction gate is the
    // authority — this is the matching web lock), and it binds the LIVE current generation: a
    // revert is a CAS on `current`, so every non-head row shares the same (epoch, seq) target.
    const page = await walkHistory(
      async (versionId) => {
        const m = await sessionVersionMeta(actor.email, ws, row.skillId, versionId);
        return m.ok ? { ok: true, data: m.data } : { ok: false };
      },
      row.versionId,
      { depth: DEPTH, from: resume },
    );
    const steps: HistoryStepView[] = page.steps.map((step) => ({
      versionId: step.versionId,
      author: step.author,
      message: step.message,
      parents: step.parents,
      fileCount: step.fileCount,
      revertRequestId: crypto.randomUUID(),
    }));
    history = {
      published: true,
      head: row.versionId,
      canRevert: actor.role !== "member",
      expectedEpoch: String(row.epoch),
      expectedSeq: String(row.seq),
      steps,
      cursor: page.cursor,
      truncated: page.truncated,
    };
  }

  return {
    ws,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    openProposals: row.openProposals,
    history,
  };
}

/**
 * The team-revert decision — roll a skill's `current` back to a known-good version. The per-row
 * RevertControl on a non-head history row posts here (the same route it renders under). The guard
 * (`requireReviewer`, a confirmed owner|reviewer seat) runs FIRST and the vault's in-transaction
 * gate re-checks the seat — the web never decides, it relays a decision the vault authorizes.
 * Revert is a FORWARD move (it restores already-consented bytes on top of `current`; nothing is
 * deleted, and a team can roll forward again). The form binds what the reviewer saw: a loader-minted
 * UUID `request_id` (a retried form replays the same idempotent outcome) plus the bound `(epoch,
 * seq)` the page rendered against, so a moved pointer refuses `conflict` and the reloaded page mints
 * fresh values. React Router revalidates the loader after this action — no explicit invalidation.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const actor = await requireReviewer(request, ws);

  const form = await request.formData();
  if (String(form.get("intent") ?? "") !== "revert") {
    return data<RevertActionData>({ status: "error" }, { status: 400 });
  }

  // The belt runs after the guard (a stranger burns no token) and before the vault call, keyed by
  // the guard-minted actor's email. A refusal is the honest error state — nothing was sent.
  if (!allowRevertWrite(actor.email)) {
    return data<RevertActionData>({ status: "error" });
  }

  const requestId = String(form.get("request_id") ?? "").trim();
  const good = String(form.get("good_version_id") ?? "").trim();
  const expectedEpoch = parseGeneration(String(form.get("expected_epoch") ?? "").trim());
  const expectedSeq = parseGeneration(String(form.get("expected_seq") ?? "").trim());
  if (
    !UUID_RE.test(requestId) ||
    !HEX64.test(good) ||
    expectedEpoch === undefined ||
    expectedSeq === undefined
  ) {
    return data<RevertActionData>({ status: "error" });
  }

  // The vault keys on the immutable skill id, never the catalog name.
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<RevertActionData>({ status: "error" });
  }

  let outcome: RevertOutcome | null;
  try {
    const res = await vaultFetch({
      method: "POST",
      template: "/internal/v1/workspaces/{ws}/skills/{skill}/reverts",
      params: { ws, skill: row.skillId },
      actingEmail: actor.email,
      body: {
        request_id: requestId,
        good_version_id: good,
        expected_epoch: expectedEpoch,
        expected_seq: expectedSeq,
      },
    });
    outcome = res.ok ? ((await res.json()) as RevertOutcome) : null;
  } catch {
    outcome = null;
  }
  if (outcome === null) {
    return data<RevertActionData>({ status: "error" });
  }

  if (outcome.outcome === "reverted") {
    return data<RevertActionData>({ status: "reverted" });
  }
  if (outcome.outcome === "conflict") {
    return data<RevertActionData>({ status: "conflict" });
  }
  if (outcome.outcome === "denied") {
    const reason = outcome.reason ?? "";
    // A reused request id: the vault refused because THIS id was already used for a DIFFERENT revert
    // (a genuine lost-ack retry of the SAME revert replays as `reverted` and never reaches here).
    // NEVER report it as success — the divergent revert did not move `current`.
    if (reason === REVERT_DENIED_REASONS.opIdReused) {
      return data<RevertActionData>({
        status: "denied",
        reason: "This roll back couldn't be confirmed — reload to see the team's current version.",
      });
    }
    // The role gate is the vault's SHARED approve/reject string (it will NOT say "roll back"), so
    // the web substitutes verb-appropriate copy; any other static reason relays verbatim, and an
    // unrecognized/absent one degrades to the generic declined copy.
    const copy =
      reason === REVERT_DENIED_REASONS.roleGate
        ? "You need reviewer or owner access to roll back."
        : reason.length > 0
          ? reason
          : "The server declined this roll back.";
    return data<RevertActionData>({ status: "denied", reason: copy });
  }
  // `not_found` and anything else: the uniform miss — nothing was rolled back, a retry is safe.
  return data<RevertActionData>({ status: "error" });
}

export default function SkillHistoryPage() {
  const { ws, skill, currentShort, displayName, openProposals, history } =
    useLoaderData<typeof loader>();
  return (
    <div className="space-y-6">
      <SkillHeader ws={ws} skill={skill} currentShort={currentShort} displayName={displayName} />
      <SkillTabs
        basePath={`/workspaces/${ws}/skills/${skill}`}
        active="history"
        openProposals={openProposals}
      />
      <HistorySection ws={ws} skill={skill} data={history} />
    </div>
  );
}
