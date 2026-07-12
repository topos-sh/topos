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
import { notFound, requireMember, requireReviewer } from "@/lib/auth/guards.server";
import { insertProposalComment, proposalCommentsFor, skillIndexRow } from "@/lib/db/queries.server";
import { loadDiffContents } from "@/lib/diff/load.server";
import { type DiffFileMode, type FileDiffModel, MAX_HIGHLIGHT_BYTES } from "@/lib/diff/model";
import { computeDiffPlan, type PlanFile } from "@/lib/diff/plan";
import { diffChromeAssets, renderFileDiffHTML } from "@/lib/diff/render/pierre.server";
import { vaultFetch } from "@/lib/plane/client.server";
import { REVIEW_DENIED_REASONS } from "@/lib/plane/errors";
import {
  sessionBundleCapped,
  sessionCurrent,
  sessionProposalDetail,
  sessionVersionMeta,
  sessionVersionMetaFresh,
} from "@/lib/plane/reads.server";
import type { ReviewDecisionOutcome } from "@/lib/plane/wire";
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

/** One resolution row the terminal panel renders (older rows may carry partial facts). */
interface ResolutionFacts {
  resolved_by: string;
  reason: string | null;
  resolved_at: string | null;
}

/** One rendered diff card: the lean model (raw file text stripped) plus its pre-sanitized HTML. */
interface RenderedDiffFile {
  model: FileDiffModel;
  /** The sanitized unified diff, pre-rendered server-side; absent for non-text presentations. */
  html?: string;
  anchorId: string;
}

/**
 * The rendered review: what would change if this proposal became the team's current version — and,
 * for an owner|reviewer seat, the decision itself. Every vault read carries the actor's verified
 * email for the internal lane (no token is opened anywhere) and keys on the immutable `skillId`;
 * the decision is a vault-authorized write (seat, four-eyes, the bound generation), so this page
 * stays a member page — read-only is a legitimate state, not a miss. The page ALWAYS diffs against
 * the LIVE current pointer and mints the approve binding from the same render, so a conflict's
 * re-show costs nothing.
 *
 * Ordering is load-bearing: the proposal DETAIL + the live pointer come first and derive the page
 * state; only then is the candidate's meta consulted. A never-proposed candidate is the uniform 404
 * (the version itself stays viewable under `…/versions/[versionId]`) — but once a proposal row
 * exists, NO later read failure is fatal: the vault retains a candidate's bytes only while
 * trunk-reachable or an open proposal on the live base, so a rejected or staled candidate's meta
 * 404 is normal reclamation and the page renders the full state surface (banner, resolution,
 * comments) with an honest diff-less card in place of the files. The async diff render runs HERE so
 * every review component stays a plain synchronous component.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const versionId = params.versionId as string;
  const actor = await requireMember(request, ws);
  if (!VERSION_ID.test(versionId)) {
    notFound();
  }

  // The existence probe is the DB catalog: an unknown skill NAME is the uniform 404. (A known name
  // with no current pointer still resolves — the diff derivation below handles a bare candidate.)
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }
  const skillId = row.skillId;

  const detailPromise = sessionProposalDetail(actor.email, ws, skillId, versionId);
  const candidateMetaPromise = sessionVersionMeta(actor.email, ws, skillId, versionId);
  const detail = await detailPromise;
  if (!detail.ok && detail.kind === "not_found") {
    // No proposal row for this candidate — never proposed here, or not this workspace's to see.
    notFound();
  }
  const current = await sessionCurrent(actor.email, ws, skillId);
  if (!current.ok) {
    return {
      view: "empty" as const,
      heading: "The server couldn't be read",
      message: `Fetching this skill's current pointer failed: ${current.message}. Nothing below is known — reload to retry.`,
    };
  }
  const currentId = current.data.version_id;
  const currentMetaPromise = sessionVersionMeta(actor.email, ws, skillId, currentId);

  // A degraded (non-404) detail read still renders the page — the state is honestly unknown.
  const state = detail.ok
    ? deriveProposalPageState(
        {
          version_id: detail.data.version_id,
          status: detail.data.status,
          base_generation: { epoch: detail.data.base_epoch, seq: detail.data.base_seq },
        },
        { versionId: currentId, generation: { epoch: current.data.epoch, seq: current.data.seq } },
      )
    : "unknown";
  const isSelfProposal =
    detail.ok && detail.data.proposer === actor.email && detail.data.review_required;
  const canDecide = actor.role !== "member";

  // A stale or rejected candidate is one the vault RECLAIMS — readability is the fact being asked,
  // so those states bypass the in-process meta LRU (a warm cache must not dress a reclaimed
  // candidate up as a readable diff). Every other state uses the prefetch above.
  const candidateMeta =
    state === "stale" || state === "rejected"
      ? await sessionVersionMetaFresh(actor.email, ws, skillId, versionId)
      : await candidateMetaPromise;

  // Shared surface — the header facts, the terminal resolution, the comment thread, and the CLI /
  // member-note flags — computed the same way whether or not the diff renders.
  const { comments, truncated } = await proposalCommentsFor(actor, skillId, versionId);
  const resolvedState =
    detail.ok && (state === "accepted-live" || state === "superseded" || state === "rejected")
      ? state
      : null;
  const resolutionFacts: ResolutionFacts | null =
    detail.ok && detail.data.resolved_by !== null
      ? {
          resolved_by: detail.data.resolved_by,
          reason: detail.data.resolved_reason,
          resolved_at: detail.data.resolved_at,
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
    showCliDetails: state === "pending" || state === "stale",
    showUnknownHandoff: state === "unknown",
    memberNote: state === "pending" && !canDecide,
    createdAt: detail.ok ? detail.data.created_at : undefined,
    proposer: detail.ok ? detail.data.proposer : undefined,
  };

  if (!candidateMeta.ok) {
    // The proposal EXISTS (the detail read said so) but its candidate bytes don't — the full state
    // surface renders with a diff-less card. No decision forms here, deliberately: the approve
    // binding IS the rendered diff, and there is none.
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

  const currentMeta = await currentMetaPromise;
  if (!currentMeta.ok) {
    return {
      view: "empty" as const,
      heading: "The current version couldn't be read",
      message: `The candidate exists, but the team's current version couldn't be fetched to diff against (${currentMeta.message}). Reload to retry.`,
    };
  }

  // The wire carries `mode` as a plain string; the plan needs the two git regular-file modes. The
  // vault only ever emits "100644"/"100755", so the coercion is safe (and total set-logic anyway).
  const toPlanFiles = (
    files: readonly { path: string; mode: string; object_id: string }[],
  ): PlanFile[] =>
    files.map((f) => ({ path: f.path, mode: f.mode as DiffFileMode, object_id: f.object_id }));
  const plan = computeDiffPlan(
    toPlanFiles(currentMeta.data.files),
    toPlanFiles(candidateMeta.data.files),
  );
  const changed = plan.filter((e) => e.kind !== "unchanged");
  const unchangedCount = plan.length - changed.length;
  // Blob fetches are budget-capped (file count + page bytes); the blob fetcher closes over the
  // actor's email + skillId so the loader names no read scope itself. The chrome assets are
  // independent. Each text card's unified diff is pre-rendered + sanitized HERE, so DiffFileCard
  // renders it as a plain component; the raw file text is then stripped from the client payload.
  const [models, chrome] = await Promise.all([
    loadDiffContents(plan, {
      getBundleCapped: (objectId: string, maxBytes: number) =>
        sessionBundleCapped(actor.email, ws, skillId, objectId, maxBytes),
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

  const decision =
    state === "pending" && canDecide
      ? {
          approveRequestId: crypto.randomUUID(),
          rejectRequestId: crypto.randomUUID(),
          expectedEpoch: String(current.data.epoch),
          expectedSeq: String(current.data.seq),
          baseEpoch: detail.ok ? String(detail.data.base_epoch) : "0",
          baseSeq: detail.ok ? String(detail.data.base_seq) : "0",
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
 * One typed outcome per honest render: `conflict` = current moved (the revalidated page shows the
 * fresh diff); `self_approve` = four-eyes; `not_open` = the proposal is no longer open under this
 * base; `denied` = the seat gate or an unrecognized static reason; `reason_required` = a reject
 * without a usable reason. `submittedReason` echoes on a non-success so the dialog keeps the text.
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

/** Map a typed denied reason to its state (unknown reasons degrade to generic denied). */
function stateForDeniedReason(reason: string | undefined): ReviewFormState["status"] {
  if (reason === REVIEW_DENIED_REASONS.fourEyes) {
    return "self_approve";
  }
  if (
    reason === REVIEW_DENIED_REASONS.notOpen ||
    reason === REVIEW_DENIED_REASONS.alreadyAccepted
  ) {
    return "not_open";
  }
  return "denied";
}

/** POST an approve/reject decision on the internal lane (200-for-all-outcomes) and parse the body. */
async function postDecision(
  ws: string,
  skillId: string,
  versionId: string,
  actingEmail: string,
  verb: "approve" | "reject",
  body: Record<string, unknown>,
): Promise<ReviewDecisionOutcome | null> {
  try {
    const res = await vaultFetch({
      method: "POST",
      template: `/internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/${verb}`,
      params: { ws, skill: skillId, version_id: versionId },
      actingEmail,
      body,
    });
    return res.ok ? ((await res.json()) as ReviewDecisionOutcome) : null;
  } catch {
    return null;
  }
}

/**
 * The review decisions + the comment write, dispatched on the hidden `intent`. Every branch
 * RE-GUARDS itself (a page-level check never extends to its actions): approve/reject/withdraw need
 * a confirmed owner|reviewer seat (withdraw is the proposer's own reject — the same write, the same
 * gate: four-eyes withholds APPROVE alone), comment needs any confirmed member. The vault's
 * in-transaction gate re-checks all of it; the web only relays. React Router revalidates the loader
 * after the action, so the fresh diff / thread simply re-renders — no explicit path invalidation.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const versionId = params.versionId as string;
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");

  if (intent === "comment") {
    return commentAction(request, ws, skill, versionId, form);
  }
  if (intent === "approve") {
    return approveAction(request, ws, skill, versionId, form);
  }
  if (intent === "reject" || intent === "withdraw") {
    return rejectAction(request, ws, skill, versionId, form);
  }
  return data<ReviewFormState>({ status: "error" }, { status: 400 });
}

async function approveAction(
  request: Request,
  ws: string,
  skill: string,
  versionId: string,
  form: FormData,
) {
  const actor = await requireReviewer(request, ws);
  const requestId = String(form.get("request_id") ?? "").trim();
  const expectedEpoch = parseGeneration(String(form.get("expected_epoch") ?? "").trim());
  const expectedSeq = parseGeneration(String(form.get("expected_seq") ?? "").trim());
  if (
    !UUID_RE.test(requestId) ||
    !VERSION_ID.test(versionId) ||
    expectedEpoch === undefined ||
    expectedSeq === undefined
  ) {
    return data<ReviewFormState>({ status: "error" });
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<ReviewFormState>({ status: "error" });
  }
  const outcome = await postDecision(ws, row.skillId, versionId, actor.email, "approve", {
    request_id: requestId,
    expected_epoch: expectedEpoch,
    expected_seq: expectedSeq,
  });
  if (outcome === null) {
    return data<ReviewFormState>({ status: "error" });
  }
  if (outcome.outcome === "approved") {
    return data<ReviewFormState>({ status: "approved" });
  }
  if (outcome.outcome === "conflict") {
    return data<ReviewFormState>({ status: "conflict" });
  }
  if (outcome.outcome === "denied") {
    return data<ReviewFormState>({ status: stateForDeniedReason(outcome.reason) });
  }
  return data<ReviewFormState>({ status: "error" });
}

async function rejectAction(
  request: Request,
  ws: string,
  skill: string,
  versionId: string,
  form: FormData,
) {
  const actor = await requireReviewer(request, ws);
  const requestId = String(form.get("request_id") ?? "").trim();
  const expectedEpoch = parseGeneration(String(form.get("expected_epoch") ?? "").trim());
  const expectedSeq = parseGeneration(String(form.get("expected_seq") ?? "").trim());
  const reason = String(form.get("reason") ?? "").trim();
  if (
    !UUID_RE.test(requestId) ||
    !VERSION_ID.test(versionId) ||
    expectedEpoch === undefined ||
    expectedSeq === undefined
  ) {
    return data<ReviewFormState>({ status: "error", submittedReason: reason });
  }
  if (reason.length === 0 || reason.length > MAX_REASON_CHARS) {
    // The first belt — the vault's edge and the OSS op hold the same 1..=2000 line.
    return data<ReviewFormState>({ status: "reason_required", submittedReason: reason });
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<ReviewFormState>({ status: "error", submittedReason: reason });
  }
  const outcome = await postDecision(ws, row.skillId, versionId, actor.email, "reject", {
    request_id: requestId,
    expected_epoch: expectedEpoch,
    expected_seq: expectedSeq,
    reason,
  });
  if (outcome === null) {
    return data<ReviewFormState>({ status: "error", submittedReason: reason });
  }
  if (outcome.outcome === "rejected") {
    return data<ReviewFormState>({ status: "rejected", submittedReason: reason });
  }
  if (outcome.outcome === "denied") {
    return data<ReviewFormState>({
      status: stateForDeniedReason(outcome.reason),
      submittedReason: reason,
    });
  }
  if (outcome.outcome === "conflict") {
    // A reject binds the proposal's base, so a moved current surfaces as no-longer-open, not a
    // pointer conflict — treat it as the same honest not-open state.
    return data<ReviewFormState>({ status: "not_open", submittedReason: reason });
  }
  return data<ReviewFormState>({ status: "error", submittedReason: reason });
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
  // The bucket runs AFTER the shape belts (a mangled form burns no token) and BEFORE any DB write,
  // keyed by the guard-minted actor's email.
  if (!allowCommentWrite(actor.email)) {
    return data<CommentFormState>({ status: "slow_down", submittedBody: body });
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    return data<CommentFormState>({ status: "error", submittedBody: body });
  }
  try {
    const outcome = await insertProposalComment(actor, {
      id: id.toLowerCase(),
      skillId: row.skillId,
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
    showUnknownHandoff,
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
        <ApproveHandoff skill={skill} versionId={versionId} status={state as "pending" | "stale"} />
      </div>
    </details>
  ) : null;
  const unknownHandoff = showUnknownHandoff ? (
    <ApproveHandoff skill={skill} versionId={versionId} status="unknown" />
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
        {unknownHandoff}
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
            ws={ws}
            skill={skill}
            versionId={versionId}
            approveRequestId={body.decision.approveRequestId}
            rejectRequestId={body.decision.rejectRequestId}
            expectedEpoch={body.decision.expectedEpoch}
            expectedSeq={body.decision.expectedSeq}
            baseEpoch={body.decision.baseEpoch}
            baseSeq={body.decision.baseSeq}
            withholdApprove={body.decision.withholdApprove}
          />
        ) : (
          <MemberReadOnlyNote />
        )
      ) : null}
      {resolutionPanel}
      {cliDetails}
      {unknownHandoff}
      {commentsSection}
    </Shell>
  );
}

/**
 * Honest failure state for the review route (RR renders it for any error thrown by the loader,
 * action, or render). A route-error RESPONSE is the uniform miss — stated plainly, no access claim.
 * Anything else is a build fault: no detail leaks (an error message can carry internal values), just
 * the plain fact and a retry that re-runs the loader.
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
