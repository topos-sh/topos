import type { ReactNode } from "react";
import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import {
  data,
  isRouteErrorResponse,
  useLoaderData,
  useRevalidator,
  useRouteError,
} from "react-router";
import { ApproveHandoff } from "@/components/review/ApproveHandoff";
import { CommentsSection } from "@/components/review/CommentsSection";
import { DiffFileCard } from "@/components/review/DiffFileCard";
import { FileSummaryList } from "@/components/review/FileSummaryList";
import { ResolutionPanel } from "@/components/review/ResolutionPanel";
import { ReviewDecisionPanel } from "@/components/review/ReviewDecisionPanel";
import { ReviewHeader } from "@/components/review/ReviewHeader";
import { MemberReadOnlyNote } from "@/components/review/ReviewNotes";
import { TrustPanel } from "@/components/review/TrustPanel";
import { Card } from "@/components/ui";
import {
  notFound,
  requireMember,
  requireReviewer,
  workspaceInScope,
} from "@/lib/auth/guards.server";
import {
  inFinalTx,
  lockOpenProposalInTx,
  resolveProposalInTx,
} from "@/lib/db/queries.custody.server";
import { workspacePolicyOf } from "@/lib/db/queries.policy.server";
import {
  bundleById,
  insertProposalComment,
  proposalByCandidate,
  proposalCommentsFor,
  proposalExists,
  skillIndexRow,
} from "@/lib/db/queries.server";
import { loadDiffContents } from "@/lib/diff/load.server";
import { type DiffFileMode, type FileDiffModel, MAX_HIGHLIGHT_BYTES } from "@/lib/diff/model";
import { computeDiffPlan, type PlanFile } from "@/lib/diff/plan";
import { diffChromeAssets, renderFileDiffHTML } from "@/lib/diff/render/pierre.server";
import { movePointer, purgeVersionBytes } from "@/lib/plane/custody.server";
import {
  custodyCurrent,
  custodyObjectCapped,
  custodyVersionMeta,
  custodyVersionMetaFresh,
} from "@/lib/plane/reads.server";
import { allowCommentWrite } from "@/lib/rate-limit.server";
import { deriveDiffAvailability, deriveProposalPageState } from "@/lib/review/state";

const VERSION_ID = /^[0-9a-f]{64}$/;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const GEN_RE = /^(0|[1-9][0-9]{0,15})$/;
const MAX_REASON_CHARS = 2000;
const MAX_COMMENT_CHARS = 4000;

function parseGeneration(value: string): number | undefined {
  if (!GEN_RE.test(value)) {
    return undefined;
  }
  const n = Number(value);
  return n <= Number.MAX_SAFE_INTEGER ? n : undefined;
}

export function meta({ params }: { params: { skill?: string; versionId?: string } }) {
  const short = (params.versionId ?? "").slice(0, 12);
  return [{ title: `${params.skill ?? "skill"} @${short} · Topos` }];
}

// ── The loader ────────────────────────────────────────────────────────────────────────────────

/** One rendered diff card: the lean model (raw file text stripped) plus its pre-sanitized HTML. */
interface RenderedDiffFile {
  model: FileDiffModel;
  /** The sanitized unified diff, pre-rendered server-side; absent for non-text presentations. */
  html?: string;
  anchorId: string;
}

/**
 * The rendered review: what would change if this proposal became the team's current version —
 * and, for an owner|reviewer seat, the decision itself. The proposal is the app's OWN row
 * (proposalByCandidate — a never-proposed candidate is the uniform 404); the candidate's bytes
 * are the vault's, read on the internal custody lane keyed by the immutable `skillId`. The
 * decision is app-authorized orchestration (seat + four-eyes here, the CAS in the vault), so
 * this page stays a member page — read-only is a legitimate state, not a miss.
 *
 * The candidate's meta read BYPASSES the in-process LRU (`custodyVersionMetaFresh`): the vault
 * retains a candidate's bytes only while trunk-reachable or under an open proposal, so
 * readability itself is the fact being asked — a rejected candidate's 404 is normal
 * reclamation, and the page renders the full record (banner, resolution, comments) with an
 * honest diff-less card in place of the files. The page ALWAYS diffs against the LIVE current
 * pointer (the catalog row's DB mirror) and binds the approve to the same render, so a
 * conflict's re-show costs nothing. The async diff render runs HERE so every review component
 * stays a plain synchronous component.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const skill = params.skill as string;
  const versionId = params.versionId as string;
  const actor = await requireMember(request, ws);
  if (!VERSION_ID.test(versionId)) {
    notFound();
  }

  // The existence probe is the DB catalog: an unknown skill NAME is the uniform 404.
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }
  const skillId = row.skillId;

  const proposal = await proposalByCandidate(actor, skillId, versionId);
  if (proposal === undefined) {
    // No proposal row for this candidate — never proposed here, or not this workspace's to see.
    notFound();
  }

  // The live current pointer, from the same catalog read (the plane mirror the row joined).
  const liveCurrentId = row.versionId;
  const state = deriveProposalPageState(proposal.status, versionId, liveCurrentId);

  // Four-eyes is display-computed here and RE-CHECKED in the action: the proposer may not
  // approve their own proposal when the bundle's EFFECTIVE protection is 'reviewed'.
  const [bundleRow, policy] = await Promise.all([
    bundleById(actor, skillId),
    workspacePolicyOf(actor),
  ]);
  const effectiveProtection = bundleRow?.protection ?? policy.protectionDefault;
  const isSelfProposal =
    proposal.proposedBy !== null &&
    proposal.proposedBy === actor.userId &&
    effectiveProtection === "reviewed";
  const canDecide = actor.role !== "member";

  const candidateMeta = await custodyVersionMetaFresh(ws, skillId, versionId);
  const { comments, truncated } = await proposalCommentsFor(actor, skillId, versionId);

  const resolvedState =
    state === "accepted-live" ||
    state === "superseded" ||
    state === "rejected" ||
    state === "closed"
      ? state
      : null;
  const resolutionFacts =
    proposal.resolvedAt !== null
      ? {
          resolved_by: proposal.resolvedByDisplay ?? "a former member",
          reason: proposal.resolvedReason,
          resolved_at: proposal.resolvedAt.toISOString(),
        }
      : null;

  const shared = {
    view: "review" as const,
    ws,
    skill,
    versionId,
    state,
    resolution: { state: resolvedState, facts: resolutionFacts },
    comments: { commentId: crypto.randomUUID(), comments, truncated },
    showCliDetails: state === "pending",
    memberNote: state === "pending" && !canDecide,
    createdAt: proposal.createdAt.toISOString(),
    proposer: proposal.proposedByDisplay ?? undefined,
  };

  if (!candidateMeta.ok) {
    // The proposal EXISTS (the row said so) but its candidate bytes don't — the full record
    // renders with a diff-less card. No decision forms here, deliberately: the approve binding
    // IS the rendered diff, and there is none.
    const availability = deriveDiffAvailability(candidateMeta);
    return {
      ...shared,
      header: {
        skillName: skill,
        versionId,
        createdAt: shared.createdAt,
        proposer: shared.proposer,
        status: state,
      },
      body: {
        kind: "diffless" as const,
        availability,
        message: candidateMeta.message,
      },
    };
  }

  // The base of the diff: the live current version's files — or the empty tree when nothing is
  // published (a proposal can outlive a withdrawn pointer; everything reads as added).
  let currentFiles: readonly { path: string; mode: string; object_id: string }[] = [];
  if (liveCurrentId !== null) {
    const currentMeta = await custodyVersionMeta(ws, skillId, liveCurrentId);
    if (!currentMeta.ok) {
      return {
        view: "empty" as const,
        heading: "The current version couldn't be read",
        message: `The candidate exists, but the team's current version couldn't be fetched to diff against (${currentMeta.message}). Reload to retry.`,
      };
    }
    currentFiles = currentMeta.data.files;
  }

  // The wire carries `mode` as a plain string; the plan needs the two git regular-file modes. The
  // vault only ever emits "100644"/"100755", so the coercion is safe (and total set-logic anyway).
  const toPlanFiles = (
    files: readonly { path: string; mode: string; object_id: string }[],
  ): PlanFile[] =>
    files.map((f) => ({ path: f.path, mode: f.mode as DiffFileMode, object_id: f.object_id }));
  const plan = computeDiffPlan(toPlanFiles(currentFiles), toPlanFiles(candidateMeta.data.files));
  const changed = plan.filter((e) => e.kind !== "unchanged");
  const unchangedCount = plan.length - changed.length;
  // Blob fetches are budget-capped (file count + page bytes); the blob fetcher closes over the
  // workspace + skillId so the loader names no read scope itself. The chrome assets are
  // independent. Each text card's unified diff is pre-rendered + sanitized HERE, so DiffFileCard
  // renders it as a plain component; the raw file text is then stripped from the client payload.
  const [models, chrome] = await Promise.all([
    loadDiffContents(plan, {
      getBundleCapped: (objectId: string, maxBytes: number) =>
        custodyObjectCapped(ws, skillId, objectId, maxBytes),
    }),
    diffChromeAssets(),
  ]);
  const files: RenderedDiffFile[] = await Promise.all(
    models.map(async (model: FileDiffModel, i: number) => {
      const { entry } = model;
      let html: string | undefined;
      if (model.presentation === "text" && entry.kind !== "moved" && entry.kind !== "mode-only") {
        const plain =
          (model.sizes.old ?? 0) > MAX_HIGHLIGHT_BYTES ||
          (model.sizes.new ?? 0) > MAX_HIGHLIGHT_BYTES;
        html = await renderFileDiffHTML({
          path: entry.path,
          prevPath: entry.prevPath,
          oldText: model.oldText,
          newText: model.newText,
          plain,
        });
      }
      // Ship the classification, not the bytes — the sanitized html already carries what renders.
      const leanModel: FileDiffModel = { ...model, oldText: undefined, newText: undefined };
      return { model: leanModel, html, anchorId: `file-${i}` };
    }),
  );

  // The decision binds THIS render's current generation: the approve action re-reads the live
  // pointer and refuses when it no longer matches what the diff above was computed against.
  const decision =
    state === "pending" && canDecide && row.generation !== null
      ? {
          expectedGeneration: String(row.generation),
          withholdApprove: isSelfProposal,
        }
      : null;

  return {
    ...shared,
    header: {
      skillName: skill,
      versionId,
      author: candidateMeta.data.author,
      message: candidateMeta.data.message,
      createdAt: shared.createdAt,
      proposer: shared.proposer,
      status: state,
    },
    body: {
      kind: "full" as const,
      bundleDigest: candidateMeta.data.bundle_digest,
      changed,
      unchangedCount,
      chrome,
      files,
      decision,
    },
  };
}

// ── The action (intent-dispatched) ──────────────────────────────────────────────────────────────

/**
 * One typed outcome per honest render: `conflict` = current moved (the revalidated page shows
 * the fresh diff); `self_approve` = four-eyes; `not_open` = the proposal is no longer open;
 * `denied` = a typed refusal with its copy; `reason_required` = a reject without a usable
 * reason. `submittedReason` echoes on a non-success so the dialog keeps the text.
 */
export interface ReviewFormState {
  status:
    | "idle"
    | "approved"
    | "rejected"
    | "conflict"
    | "self_approve"
    | "not_open"
    | "denied"
    | "reason_required"
    | "error";
  submittedReason?: string;
}

/**
 * `empty` / `too_long` / `slow_down` / `thread_full` keep the typed text via `submittedBody`;
 * `slow_down` is the per-actor bucket refusing a burst; `thread_full` is the hard cap; `error` is a
 * mangled form or a storage fault (a retry is safe — same id, same row).
 */
export interface CommentFormState {
  status: "idle" | "posted" | "empty" | "too_long" | "slow_down" | "thread_full" | "error";
  submittedBody?: string;
}

/**
 * The review decisions + the comment write, dispatched on the hidden `intent`. Every branch
 * RE-GUARDS itself (a page-level check never extends to its actions): approve/reject/withdraw
 * need an owner|reviewer seat (withdraw is the proposer retracting their own — the same
 * resolve, verdict `withdrawn`), comment needs any member. The ORCHESTRATION is inline and
 * app-authorized: the seat gate and four-eyes run here against the app's own rows; the vault
 * only CAS-moves the pointer. React Router revalidates the loader after the action, so the
 * fresh diff / thread simply re-renders.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const skill = params.skill as string;
  const versionId = params.versionId as string;
  // The membership FLOOR, hoisted above the intent dispatch: every intent below requires at
  // least a member (most re-check owner/reviewer themselves), and the unmatched-intent 400 must
  // never answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a
  // 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  await requireMember(request, workspace.id);
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");

  if (intent === "comment") {
    return commentAction(request, ws, skill, versionId, form);
  }
  if (intent === "approve") {
    return approveAction(request, ws, skill, versionId, form);
  }
  if (intent === "reject" || intent === "withdraw") {
    return rejectAction(request, ws, skill, versionId, form, intent);
  }
  return data<ReviewFormState>({ status: "error" }, { status: 400 });
}

/**
 * APPROVE — the promote: gate, then CAS, then the row. The four-eyes check runs against the
 * bundle's EFFECTIVE protection (its pin, else the workspace default); the CAS binds the
 * generation the REVIEWER's render diffed against — a fresh pointer read that disagrees means
 * the diff they approved is stale, and the honest answer is a conflict, never a silent approve
 * of something unseen. On a landed move the proposal row resolves (FOR UPDATE-locked) in one
 * final transaction; a concurrently-resolved row changes nothing the pointer already says.
 */
async function approveAction(
  request: Request,
  ws: string,
  skill: string,
  versionId: string,
  form: FormData,
) {
  const actor = await requireReviewer(request, ws);
  const expected = parseGeneration(String(form.get("expected_generation") ?? "").trim());
  if (!VERSION_ID.test(versionId) || expected === undefined) {
    return data<ReviewFormState>({ status: "error" });
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<ReviewFormState>({ status: "error" });
  }
  const proposal = await proposalByCandidate(actor, row.skillId, versionId);
  if (proposal === undefined || proposal.status !== "open") {
    return data<ReviewFormState>({ status: "not_open" });
  }
  const [bundleRow, policy] = await Promise.all([
    bundleById(actor, row.skillId),
    workspacePolicyOf(actor),
  ]);
  const effectiveProtection = bundleRow?.protection ?? policy.protectionDefault;
  if (effectiveProtection === "reviewed" && proposal.proposedBy === actor.userId) {
    return data<ReviewFormState>({ status: "self_approve" });
  }

  const current = await custodyCurrent(ws, row.skillId);
  if (!current.ok) {
    return data<ReviewFormState>({ status: "error" });
  }
  if (current.data.generation !== expected) {
    // The pointer moved since the reviewer's render — their diff no longer shows the change.
    return data<ReviewFormState>({ status: "conflict" });
  }
  const moved = await movePointer(ws, row.skillId, {
    version_id: versionId,
    expected_generation: current.data.generation,
    attribution: actor.display,
  });
  if (moved.kind === "fault" || moved.kind === "rejected") {
    return data<ReviewFormState>({ status: "error" });
  }
  if (moved.kind === "conflict") {
    return data<ReviewFormState>({ status: "conflict" });
  }
  if (moved.kind !== "ok") {
    // not_found / target_purged — the candidate's bytes are gone from custody (a purge or
    // reclamation raced the decision); nothing moved.
    return data<ReviewFormState>({ status: "denied" });
  }
  await inFinalTx(async (tx) => {
    const locked = await lockOpenProposalInTx(tx, ws, row.skillId, versionId);
    if (locked !== undefined) {
      await resolveProposalInTx(
        tx,
        { userId: actor.userId, display: actor.display, workspaceId: ws },
        locked,
        "approved",
        null,
      );
    }
  });
  return data<ReviewFormState>({ status: "approved" });
}

/**
 * REJECT / WITHDRAW — a status flip, no pointer move: lock the open row, resolve it with the
 * verdict, notify the author (a reject only — withdrawing is the author telling themselves).
 * Withdraw is the PROPOSER's own retraction and refuses anyone else, four-eyes or not. A
 * rejected candidate's bytes are reclaimed best-effort after the row commits — the record (the
 * row + the notice) already stands; a custody fault changes nothing the reviewer decided.
 */
async function rejectAction(
  request: Request,
  ws: string,
  skill: string,
  versionId: string,
  form: FormData,
  intent: "reject" | "withdraw",
) {
  const actor = await requireReviewer(request, ws);
  const reason = String(form.get("reason") ?? "").trim();
  if (!VERSION_ID.test(versionId)) {
    return data<ReviewFormState>({ status: "error", submittedReason: reason });
  }
  if (reason.length === 0 || reason.length > MAX_REASON_CHARS) {
    return data<ReviewFormState>({ status: "reason_required", submittedReason: reason });
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<ReviewFormState>({ status: "error", submittedReason: reason });
  }

  const outcome = await inFinalTx(async (tx) => {
    const locked = await lockOpenProposalInTx(tx, ws, row.skillId, versionId);
    if (locked === undefined) {
      return "not_open" as const;
    }
    if (intent === "withdraw" && locked.proposedBy !== actor.userId) {
      return "denied" as const;
    }
    await resolveProposalInTx(
      tx,
      { userId: actor.userId, display: actor.display, workspaceId: ws },
      locked,
      intent === "withdraw" ? "withdrawn" : "rejected",
      reason,
    );
    return "resolved" as const;
  });
  if (outcome === "not_open") {
    return data<ReviewFormState>({ status: "not_open", submittedReason: reason });
  }
  if (outcome === "denied") {
    return data<ReviewFormState>({ status: "denied", submittedReason: reason });
  }
  if (intent === "reject") {
    // Best-effort byte reclaim of the rejected candidate — the record (the row + the notice)
    // already stands; a custody refusal or fault changes nothing the reviewer decided.
    void purgeVersionBytes(ws, row.skillId, versionId, actor.display);
  }
  return data<ReviewFormState>({ status: "rejected", submittedReason: reason });
}

async function commentAction(
  request: Request,
  ws: string,
  skill: string,
  versionId: string,
  form: FormData,
) {
  const actor = await requireMember(request, ws);
  const id = String(form.get("comment_id") ?? "").trim();
  const body = String(form.get("body") ?? "").trim();
  if (!UUID_RE.test(id) || !VERSION_ID.test(versionId)) {
    return data<CommentFormState>({ status: "error", submittedBody: body });
  }
  if (body.length === 0) {
    return data<CommentFormState>({ status: "empty" });
  }
  if (body.length > MAX_COMMENT_CHARS) {
    return data<CommentFormState>({ status: "too_long", submittedBody: body });
  }
  // The bucket runs AFTER the shape belts (a mangled form burns no token) and BEFORE any DB
  // write, keyed by the guard-minted actor's user id.
  if (!allowCommentWrite(actor.userId)) {
    return data<CommentFormState>({ status: "slow_down", submittedBody: body });
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<CommentFormState>({ status: "error", submittedBody: body });
  }
  // A thread exists only under a REAL proposal — never a free write lane keyed by an arbitrary
  // hex id (the loader 404s never-proposed candidates; the action holds the same line).
  if (!(await proposalExists(actor, row.skillId, versionId))) {
    return data<CommentFormState>({ status: "error", submittedBody: body });
  }
  try {
    const outcome = await insertProposalComment(actor, {
      id: id.toLowerCase(),
      bundleId: row.skillId,
      versionId,
      body,
    });
    if (outcome === "thread_full") {
      return data<CommentFormState>({ status: "thread_full", submittedBody: body });
    }
  } catch {
    return data<CommentFormState>({ status: "error", submittedBody: body });
  }
  return data<CommentFormState>({ status: "posted" });
}

// ── The page ──────────────────────────────────────────────────────────────────────────────────

function Shell({ children }: { children: ReactNode }) {
  return <main className="mx-auto flex w-full max-w-4xl flex-col gap-6 px-4 py-8">{children}</main>;
}

function EmptyState({ heading, message }: { heading: string; message: string }) {
  return (
    <Card className="flex flex-col gap-2 p-6">
      <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">{heading}</h1>
      <p className="text-sm text-dim">{message}</p>
    </Card>
  );
}

export default function ProposalReviewPage() {
  const data = useLoaderData<typeof loader>();
  if (data.view === "empty") {
    return (
      <Shell>
        <EmptyState heading={data.heading} message={data.message} />
      </Shell>
    );
  }

  const {
    ws,
    skill,
    versionId,
    state,
    resolution,
    comments,
    showCliDetails,
    memberNote,
    header,
    body,
  } = data;

  const resolutionPanel =
    resolution.state !== null ? (
      <ResolutionPanel state={resolution.state} resolution={resolution.facts} />
    ) : null;
  const cliDetails = showCliDetails ? (
    <details>
      <summary className="cursor-pointer select-none font-mono text-[13px] text-dim hover:text-ink">
        Prefer the CLI?
      </summary>
      <div className="mt-2">
        <ApproveHandoff skill={skill} versionId={versionId} />
      </div>
    </details>
  ) : null;
  const commentsSection = (
    <CommentsSection
      ws={ws}
      skill={skill}
      versionId={versionId}
      commentId={comments.commentId}
      comments={comments.comments}
      truncated={comments.truncated}
    />
  );

  if (body.kind === "diffless") {
    return (
      <Shell>
        <ReviewHeader
          skillName={header.skillName}
          versionId={header.versionId}
          createdAt={header.createdAt}
          proposer={header.proposer}
          status={header.status}
        />
        <Card className="p-4">
          <p className="text-sm text-dim">
            {body.availability === "reclaimed"
              ? "This candidate's file contents are no longer readable — the server retains only current versions and open proposals."
              : `This candidate's file contents couldn't be read (${body.message}). Reload to retry.`}
          </p>
        </Card>
        {resolutionPanel}
        {memberNote ? <MemberReadOnlyNote /> : null}
        {cliDetails}
        {commentsSection}
      </Shell>
    );
  }

  return (
    <Shell>
      <a
        href="#diff"
        className="sr-only rounded-md bg-panel px-3 py-2 text-sm focus:not-sr-only focus-visible:outline-2 focus-visible:outline-accent"
      >
        Skip to changed files
      </a>
      <ReviewHeader
        skillName={header.skillName}
        versionId={header.versionId}
        author={header.author}
        message={header.message}
        createdAt={header.createdAt}
        proposer={header.proposer}
        status={header.status}
      />
      <TrustPanel bundleDigest={body.bundleDigest ?? ""} />
      {body.changed.length === 0 ? (
        <Card className="p-4">
          <p className="text-sm text-dim">
            No file changes between current and this candidate — the two versions carry
            byte-identical files.
          </p>
        </Card>
      ) : (
        <>
          <FileSummaryList entries={body.changed} unchangedCount={body.unchangedCount} />
          {/* biome-ignore lint/security/noDangerouslySetInnerHtml: static renderer stylesheet + sprite from a constant render — no user byte reaches it (lib/diff/render/pierre.ts) */}
          <div dangerouslySetInnerHTML={{ __html: body.chrome }} />
          <div id="diff" className="flex flex-col gap-4">
            {body.files.map((file) => (
              <DiffFileCard
                key={`${file.model.entry.path}:${file.model.entry.prevPath ?? ""}`}
                model={file.model}
                html={file.html}
                anchorId={file.anchorId}
                skill={skill}
                versionId={versionId}
              />
            ))}
          </div>
        </>
      )}
      {state === "pending" ? (
        body.decision !== null ? (
          <ReviewDecisionPanel
            versionId={versionId}
            expectedGeneration={body.decision.expectedGeneration}
            withholdApprove={body.decision.withholdApprove}
          />
        ) : (
          <MemberReadOnlyNote />
        )
      ) : null}
      {resolutionPanel}
      {cliDetails}
      {commentsSection}
    </Shell>
  );
}

/**
 * Honest failure state for the review route (RR renders it for any error thrown by the loader,
 * action, or render). A route-error RESPONSE is the uniform miss — stated plainly, no access
 * claim. Anything else is a build fault: no detail leaks (an error message can carry internal
 * values), just the plain fact and a retry that re-runs the loader.
 */
export function ErrorBoundary() {
  const error = useRouteError();
  const revalidator = useRevalidator();
  if (isRouteErrorResponse(error)) {
    return (
      <main className="mx-auto flex w-full max-w-4xl flex-col gap-3 px-4 py-8">
        <div className="rounded-lg border border-line-soft bg-panel p-6">
          <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">
            Not found
          </h1>
          <p className="mt-1 text-sm text-dim">
            There&apos;s nothing here, or it isn&apos;t yours to see.
          </p>
        </div>
      </main>
    );
  }
  return (
    <main className="mx-auto flex w-full max-w-4xl flex-col gap-3 px-4 py-8">
      <div className="rounded-lg border border-line-soft bg-panel p-6">
        <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">
          This review couldn&apos;t render
        </h1>
        <p className="mt-1 text-sm text-dim">
          Something failed while building the page. Nothing was approved or changed.
        </p>
        <button
          type="button"
          onClick={() => revalidator.revalidate()}
          className="mt-4 inline-flex min-h-11 items-center justify-center rounded-md bg-accent px-4 font-mono text-[13px] text-on-accent hover:bg-accent-deep focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          Try again
        </button>
      </div>
    </main>
  );
}
