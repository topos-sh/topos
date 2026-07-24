//! `review [TARGET] [--approve | --reject | --withdraw] [-m <message>]` — resolve a proposal (the
//! `gh pr review` model).
//!
//! `--approve` moves `current` to the candidate (a compare-and-set on the proposal's base; a stale base
//! re-dos); `--reject` declines a proposal (reviewer, `-m <reason>` required); `--withdraw` retracts your
//! own open proposal (proposer). The reviewer binds the proposal's RECORDED identity (its `commit_id` = the
//! `@hash`, its `bundle_digest` re-derived from the fetched bytes and asserted to reproduce the hash — so a
//! tampered plane can't get the reviewer to sign over forged bytes) at `expected` = the FRESH `current`
//! generation (which equals a reviewable proposal's base, so a reviewer who has not pulled is still
//! correct). Viewing the change is `diff`; this verb decides. A bare `review` (no target / no verdict) is
//! the two-phase describe — a MARKED SEAM until that leg lands.

use topos_core::digest::to_hex;
use topos_types::persisted::{OpKind, OpRecord};
use topos_types::results::{
    ReviewData, ReviewDecision, ReviewDescribeData, ReviewIndexData, ReviewIndexEntry,
};
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect, ReviewSend};
use super::follow::{DirectoryConnect, build_universe_via};
use super::{parse_hex32_arg, resolve_followed_skill_in_workspace, workspace_of};
use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::plane::WriteReceipt;
use crate::{op_wal, sidecar};

/// The all-zero digest a reject/withdraw records in its WAL — those verdicts flip a status and touch no
/// bytes, so no digest is fetched or sent (the wire review request carries none).
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The seams `review` needs — the directory connector (the inbox / describe reads) and the contribute
/// connector (the approve/reject/withdraw write).
pub(crate) struct ReviewConnectors<'a> {
    pub directory: &'a DirectoryConnect<'a>,
    pub contribute: &'a ContributeConnect<'a>,
}

/// The verb's outcome — the inbox (bare), a target describe (target, no verdict), or an applied verdict.
#[derive(Debug)]
pub(crate) enum ReviewOutcome {
    /// A bare `review` — the review inbox/outbox across every enrolled workspace.
    Inbox(ReviewIndexData),
    /// A bare target (`review <skill>[@<hash>]`) — the proposal describe + the verdict next-actions.
    Describe {
        data: Box<ReviewDescribeData>,
        next_argvs: Vec<Vec<String>>,
    },
    /// A target + a verdict flag — the write landed (the verdict IS the consent).
    Applied(ReviewData),
}

/// Dispatch `review`: a bare invocation is the inbox; a bare target is the describe; a target with a
/// verdict flag applies directly (the verdict is the consent — `--yes` is accepted there as a no-op).
///
/// # Errors
/// As the individual arms below.
pub(crate) fn review_dispatch(
    ctx: &Ctx<'_>,
    connectors: &ReviewConnectors<'_>,
    target: Option<&str>,
    verdict: Option<ReviewVerdict>,
    workspace: Option<&str>,
    budget: super::DiffBudget,
) -> Result<ReviewOutcome, ClientError> {
    match (target, verdict) {
        (None, None) => review_inbox(ctx, connectors, workspace).map(ReviewOutcome::Inbox),
        (Some(t), None) => review_describe(ctx, connectors, t, workspace, budget),
        (Some(t), Some(v)) => review(ctx, connectors, t, v, workspace).map(ReviewOutcome::Applied),
        (None, Some(_)) => Err(ClientError::InvalidArgument(
            "review needs a <skill>@<hash> target for a verdict — a bare `review` is the inbox"
                .into(),
        )),
    }
}

/// The review inbox/outbox: every OPEN proposal across the enrolled workspaces, split by author
/// (yours = the outbox, everyone else's = the inbox). Author-message first (the renderer leads with it).
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; a transport failure on a proposals read.
fn review_inbox(
    ctx: &Ctx<'_>,
    connectors: &ReviewConnectors<'_>,
    workspace: Option<&str>,
) -> Result<ReviewIndexData, ClientError> {
    let (base_url, universe) = build_universe_via(ctx, connectors.directory)?;
    let base_url = base_url.ok_or(ClientError::NotEnrolled)?;
    // Fold the caller's principal to the canonical form so the outbox match is address-shape-agnostic.
    let me_principal = enroll::read_user(ctx.fs, &ctx.layout)?
        .and_then(|u| u.principal)
        .map(|p| enroll::canonical_principal(&p));
    let directory = (connectors.directory)(&base_url);
    let mut inbox = Vec::new();
    let mut outbox = Vec::new();
    for ws in &universe {
        // `--workspace` narrows the inbox to one workspace when the install joined several.
        if workspace.is_some_and(|w| w != ws.workspace_id) {
            continue;
        }
        let index = directory.proposals_index(&ws.workspace_id)?;
        for p in index.proposals {
            let entry = ReviewIndexEntry {
                workspace_id: ws.workspace_id.clone(),
                workspace_name: ws.name.clone(),
                skill: p.skill_name.clone(),
                proposal: format!("{}@{}", p.skill_name, p.version_id),
                proposer: p.proposer.clone(),
                message: p.message,
                base_version_id: p.base_version_id,
                created_at: p.created_at,
                stale: p.stale,
            };
            // The server-computed `yours` (from the resolved user id) is authoritative in BOTH
            // directions — a served `false` is never overridden by the principal comparison (emails
            // are mutable and re-registrable, so a stale principal could mislabel someone else's
            // proposal as yours). The comparison is the COMPAT fallback ONLY when the server predates
            // the field (labeling only, never authorization — the plane re-decides every verdict).
            let mine = p.yours.unwrap_or_else(|| {
                me_principal
                    .as_deref()
                    .is_some_and(|me| me == enroll::canonical_principal(&p.proposer))
            });
            if mine {
                outbox.push(entry);
            } else {
                inbox.push(entry);
            }
        }
    }
    Ok(ReviewIndexData { inbox, outbox })
}

/// A bare target's describe: the author, message, base, staleness, and the DIFF against current
/// (`current..<proposal>` through the same plane-diff machinery `diff` uses). Nothing mutates.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::TargetNotFound`] for an unknown proposal;
/// name-resolution / transport / integrity errors.
fn review_describe(
    ctx: &Ctx<'_>,
    connectors: &ReviewConnectors<'_>,
    target: &str,
    workspace: Option<&str>,
    budget: super::DiffBudget,
) -> Result<ReviewOutcome, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or(ClientError::NotEnrolled)?;
    // A target is `<skill>` (its one open proposal) or `<skill>@<hash>` (a specific one).
    let (skill_name, wanted_hash) = match target.split_once('@') {
        Some((s, h)) if !s.is_empty() && !h.is_empty() => (s.to_owned(), Some(h.to_owned())),
        Some(_) => {
            return Err(ClientError::InvalidArgument(format!(
                "a review target is `<skill>` or `<skill>@<hash>`, got `{target}`"
            )));
        }
        None => (target.to_owned(), None),
    };
    let (id, workspace_id) =
        resolve_review_skill(ctx, connectors.directory, &skill_name, workspace)?;
    let directory = (connectors.directory)(&instance.base_url);
    let index = directory.proposals_index(&workspace_id)?;
    // The proposal on this skill matching the wanted hash, or its SOLE open proposal for a bare skill.
    let mut candidates: Vec<_> = index
        .proposals
        .into_iter()
        .filter(|p| p.skill_id == id.as_str())
        .filter(|p| wanted_hash.as_deref().is_none_or(|h| p.version_id == h))
        .collect();
    let proposal = match candidates.len() {
        0 => return Err(crate::resolve::not_found(target)),
        1 => candidates.remove(0),
        _ => {
            return Err(ClientError::InvalidArgument(format!(
                "'{skill_name}' has {} open proposals — name one as `{skill_name}@<hash>` (run \
                 `topos review` for the inbox)",
                candidates.len()
            )));
        }
    };
    // Whose proposal is this? The server-computed `yours` is authoritative in BOTH directions
    // (resolved user id, never email equality) — a served `false` is never overridden; the principal
    // comparison is the COMPAT fallback ONLY for a server predating the field.
    let me_principal = enroll::read_user(ctx.fs, &ctx.layout)?
        .and_then(|u| u.principal)
        .map(|p| enroll::canonical_principal(&p));
    let yours = proposal.yours.unwrap_or_else(|| {
        me_principal
            .as_deref()
            .is_some_and(|me| me == enroll::canonical_principal(&proposal.proposer))
    });

    // The diff against current — the same plane-diff machinery `diff` runs (`current..<proposal>`),
    // under the caller's byte budget (the `--json` default cap / `--max-bytes`); a truncated body
    // is flagged and the finisher adds the FETCH_FULL_DIFF next action (`topos diff … --max-bytes
    // 0`), so a huge proposal never floods an agent's context yet stays one command away in full.
    let diffed = super::diff(
        ctx,
        &skill_name,
        Some(&format!("current..{}", proposal.version_id)),
        budget,
    )?;
    let handle = format!("{}@{}", skill_name, proposal.version_id);
    let next_argvs = verdict_next_argvs(&handle, yours);
    Ok(ReviewOutcome::Describe {
        data: Box::new(ReviewDescribeData {
            proposal: handle,
            skill: skill_name,
            proposer: proposal.proposer,
            message: proposal.message,
            base_version_id: proposal.base_version_id,
            stale: proposal.stale,
            yours,
            diff: diffed.diff,
            diff_truncated: diffed.truncated,
        }),
        next_argvs,
    })
}

/// A review verdict, parsed from the CLI's `--approve` / `--reject` / `--withdraw` group. `Reject` carries
/// its (required) reason; `Withdraw` is the author retracting their own open proposal.
#[derive(Debug, Clone)]
pub(crate) enum ReviewVerdict {
    Approve,
    Reject { reason: Option<String> },
    Withdraw,
}

impl ReviewVerdict {
    /// The wire verdict this maps to.
    fn decision(&self) -> ReviewDecision {
        match self {
            ReviewVerdict::Approve => ReviewDecision::Approve,
            ReviewVerdict::Reject { .. } => ReviewDecision::Reject,
            ReviewVerdict::Withdraw => ReviewDecision::Withdraw,
        }
    }
}

/// Resolve the proposal named `<skill>@<hash>` with `verdict`.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a `--reject` with no `-m <reason>`; [`ClientError::Enrollment`] if
/// not enrolled; [`ClientError::Conflict`] on a stale base (approve); [`ClientError::Denied`] on four-eyes
/// self-approve / not-a-reviewer / an already-resolved proposal; an integrity error if the fetched proposal
/// does not reproduce its `@hash`; a transport failure otherwise.
pub(crate) fn review(
    ctx: &Ctx<'_>,
    connectors: &ReviewConnectors<'_>,
    target: &str,
    verdict: ReviewVerdict,
    workspace: Option<&str>,
) -> Result<ReviewData, ClientError> {
    let connect = connectors.contribute;
    // A reject must carry its reason (the plane requires it, and the author is owed one). Refused at the
    // argv boundary, before any resolution or network.
    if let ReviewVerdict::Reject { reason: None } = &verdict {
        return Err(ClientError::InvalidArgument(
            "`review --reject` needs a reason — pass `-m <message>`".into(),
        ));
    }
    // `<skill>@<hash>` — the proposal's skill + its candidate commit id. Argv is validated FIRST
    // (a malformed target is a usage error however un-enrolled the machine is). Unlike the go-back /
    // revert / diff refs, this hash stays FULL-64 only: an open proposal's candidate id exists on the
    // plane, never in the local recorded history short prefixes resolve against, and fetching the
    // proposals listing at parse time would put a network read inside argv validation. The full hash is
    // what the flow already hands the reviewer — `publish --propose` prints the ready-to-run command and
    // `list <skill>` prints each open proposal as `<skill>@<full hash>`.
    let (skill_name, proposal_hex) = split_target(target)?;
    let proposal_commit = parse_hex32_arg(
        &proposal_hex,
        "the review target's `@<hash>` must be a 64-char lowercase hex version id (copy it from \
         `publish --propose` output or `topos list <skill>`)",
    )?;

    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or(ClientError::NotEnrolled)?;
    // Resolve the proposal's skill (a `--workspace` filter disambiguates a name shared across
    // workspaces). Prefer the STRICT local resolve — a followed skill binds to its OWN follow-entry
    // workspace, and the fresh-current read + candidate fetch use its read creds. When the name matches no
    // locally FOLLOWED skill, fall back to the workspace CATALOG over the wire (the same credentialed reads
    // the inbox uses): a device that published a skill via genesis `add`+`publish` but has not yet run the
    // `update` sweep has NO follow entry for it, so the exact command `topos review` printed would
    // otherwise fail "no tracked skill" until that sweep. If the catalog read yields nothing (or fails) and
    // the skill is not local either, the "no tracked skill" error stands — the offline behavior is kept.
    let (id, workspace_id) =
        resolve_review_skill(ctx, connectors.directory, &skill_name, workspace)?;
    // Teach the READ transport this skill's workspace credential before the downstream
    // `fresh_current` / candidate `fetch_version` reads. A locally FOLLOWED skill is already in the
    // `follows.json`-derived cred map (this is a no-op), but a CATALOG-resolved target (the genesis
    // publisher, pre-`update`) is not — without the bind those reads answer the transport-shaped
    // "not served here" instead of authenticating under the workspace credential membership provides.
    ctx.plane.bind_skill(&workspace_id, id.as_str());
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    let transport = connect(&instance.base_url);

    // This command's WAL kind — the FULL 3-way verdict (approve / reject / withdraw), each a distinct
    // op kind, so a crashed op of a DIFFERENT verdict can never replay this one's stored receipt.
    let this_kind = match &verdict {
        ReviewVerdict::Approve => OpKind::ReviewApprove,
        ReviewVerdict::Reject { .. } => OpKind::ReviewReject,
        ReviewVerdict::Withdraw => OpKind::ReviewWithdraw,
    };
    // Resume a crashed prior review for this skill before minting a new op.
    let kinds = [
        OpKind::ReviewApprove,
        OpKind::ReviewReject,
        OpKind::ReviewWithdraw,
    ];
    let rec = match op_wal::find_pending_for_skill(
        ctx.fs,
        &ctx.layout,
        &workspace_id,
        id.as_str(),
        &kinds,
    )? {
        // Replay a crashed prior review ONLY if it matches THIS command (same proposal + same exact
        // verdict); a different verdict (approve vs reject vs withdraw) must settle the in-flight op
        // first — reusing its op id under a new verdict would replay the OLD outcome and silently drop
        // the new intent.
        Some(pending) => {
            if pending.candidate_commit != proposal_hex || pending.op != this_kind {
                let pending_verb = match pending.op {
                    OpKind::ReviewApprove => "review --approve",
                    OpKind::ReviewWithdraw => "review --withdraw",
                    _ => "review --reject",
                };
                return Err(ClientError::PendingOp {
                    skill: skill_name.clone(),
                    detail: format!(
                        "a {pending_verb} of {skill_name}@{} is in flight — settle it (re-run that review), then retry",
                        pending.candidate_commit
                    ),
                });
            }
            pending
        }
        None => {
            // The proposal's base == `current` (it is reviewable only while open ∧ base == current), so the
            // FRESH current generation is the correct `expected` even for a reviewer who has not pulled.
            let (_current, expected) = contribute::fresh_current(ctx, id.as_str(), &workspace_id)?;
            // Only an APPROVE moves the pointer to the candidate's bytes, so only an approve re-derives
            // the digest from the fetched bytes as its consent check (asserting they reproduce the named
            // `@hash`). A reject / withdraw is a status flip that touches no bytes — fetching them would
            // wedge the verb the instant a publish stales the proposal and the GC reclaims its unique
            // objects (a 404 that must never block a retraction). Its digest is unused on the wire (the
            // review request carries the proposal id + verdict, never a digest).
            let bundle_digest = if matches!(verdict, ReviewVerdict::Approve) {
                to_hex(&contribute::verified_version_digest(
                    ctx,
                    id.as_str(),
                    proposal_commit,
                )?)
            } else {
                ZERO_HEX.to_owned()
            };
            OpRecord {
                schema_version: PERSISTED_SCHEMA_VERSION,
                upstream: None,
                op_id: contribute::new_op_id(ctx),
                workspace_id: workspace_id.clone(),
                skill_id: id.to_string(),
                op: this_kind,
                candidate_commit: proposal_hex.clone(),
                bundle_digest,
                expected_generation: expected,
                good: None,
                // A review renames nothing — carry no name so the plane preserves the stored one.
                display_name: None,
                channel: None,
                last_receipt: None,
            }
        }
    };

    // The verdict + reason ride the current invocation into the POST (the durable `OpRecord` records
    // the verdict as its op KIND but not the reject reason, so a replay re-supplies the reason from
    // this same argv — and a resume of a differing verdict was already refused above).
    let review_send = ReviewSend {
        decision: verdict.decision(),
        reason: match &verdict {
            ReviewVerdict::Reject { reason } => reason.clone(),
            _ => None,
        },
    };
    let receipt = contribute::run_write(ctx, &*transport, &sp, &rec, Some(&review_send))?;
    map_outcome(ctx, &sp, &rec, &receipt, target, verdict.decision())
}

fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    target: &str,
    decision: ReviewDecision,
) -> Result<ReviewData, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            // Approve moved `current` to the proposal — advance the reviewer's floor (the bytes land at the
            // next pull). Reject moves nothing (the plane returns OK with no signed pointer).
            let current_generation = if matches!(decision, ReviewDecision::Approve) {
                let record = receipt.wire_record.as_ref().ok_or_else(|| {
                    ClientError::Corrupt("an approved proposal carried no current pointer".into())
                })?;
                Some(contribute::apply_light_advance(ctx, sp, rec, record)?)
            } else {
                None
            };
            Ok(ReviewData {
                proposal: target.to_owned(),
                decision,
                current_generation,
            })
        }
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: skill_of(target),
            current: receipt.error.as_ref().and_then(|e| e.current_generation),
        }),
        TerminalOutcome::Denied => Err(denied_review_error(receipt, target)),
        // A terminal PERMANENT_FAILURE on a review verdict is USUALLY the target proposal no longer being
        // OPEN at the live `current` — but OP_ID_REUSED shares the outcome and must NOT be folded into it,
        // so classify by the receipt's distinguishing code first.
        TerminalOutcome::PermanentFailure => Err(permanent_failure_error(receipt, target)),
        _ => Err(contribute::plane_terminal(receipt)),
    }
}

/// Classify a terminal PERMANENT_FAILURE on a review verdict — two very different failures share the
/// outcome and must render differently:
/// - the plane's PRE-TRANSACTION "no proposal for this candidate and base": an already-resolved
///   (approved/rejected/withdrawn) proposal moved `current` past its base, so the fresh-current
///   `expected` matches no open proposal. This case carries NO distinguishing machine code and never
///   names who resolved it → the HONEST, hedged domain refusal ([`review_not_open`]);
/// - `OP_ID_REUSED`: a same-`op_id` retry whose bound identity diverged from the recorded op. The plane
///   stamps it with a distinguishing `details.code` (`OP_ID_REUSED`; see plane-store's
///   `permanent_key_reuse`), so it is NOT a "not open" verdict — keep its transport-class rendering
///   ([`contribute::plane_terminal`], the pre-fix behaviour), whose code names the real cause.
fn permanent_failure_error(receipt: &WriteReceipt, target: &str) -> ClientError {
    if terminal_code(receipt).as_deref() == Some("OP_ID_REUSED") {
        contribute::plane_terminal(receipt)
    } else {
        review_not_open(target)
    }
}

/// Classify a terminal DENIED on a review verdict — the plane's fine code selects an HONEST refusal:
/// - `FOUR_EYES_REQUIRED`: the caller proposed this version and cannot also approve it — a typed
///   [`ClientError::Denied`] whose message NAMES four-eyes (it surfaces inside `render::safe_message`'s
///   `Denied` arm), so the human reads why, not a bare code. The wire code stays `DENIED`.
/// - `NO_OPEN_PROPOSAL`: a decided/closed proposal is a terminal outcome — the SAME honest
///   [`review_not_open`] refusal the undistinguished `PERMANENT_FAILURE` arm renders.
/// - anything else: the raw wire code, wrapped in [`ClientError::Denied`] for the agent to branch on.
fn denied_review_error(receipt: &WriteReceipt, target: &str) -> ClientError {
    match terminal_code(receipt).as_deref() {
        Some("FOUR_EYES_REQUIRED") => ClientError::Denied(
            "four-eyes review: you proposed this version — a second reviewer must approve it"
                .to_owned(),
        ),
        Some("NO_OPEN_PROPOSAL") => review_not_open(target),
        _ => ClientError::Denied(
            receipt
                .error
                .as_ref()
                .map(|e| e.code.clone())
                .unwrap_or_else(|| "DENIED".to_owned()),
        ),
    }
}

/// The distinguishing machine code a terminal receipt carries, if any: the plane stamps it into the
/// receipt's `details.code` and mirrors it onto the flat wire error's `code`. `None`/an undistinguished
/// outcome-default code (e.g. `PERMANENT_FAILURE`) means no richer code was set. Reads the flat error's
/// code when no receipt was attached (a receipt-less DENIED — an old server / a wedged stored receipt).
fn terminal_code(receipt: &WriteReceipt) -> Option<String> {
    receipt
        .receipt
        .as_ref()
        .and_then(|r| r.details.as_ref())
        .and_then(|d| d.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_owned)
        .or_else(|| receipt.error.as_ref().map(|e| e.code.clone()))
}

/// The honest domain refusal for a `review` verdict the plane answered with an UNDISTINGUISHED terminal
/// PERMANENT_FAILURE — the named proposal is no longer OPEN for review (already resolved, or `current`
/// moved on). Hedged deliberately: in THAT case the wire carries no distinguishing code and never names
/// who resolved it (a distinguished code like `OP_ID_REUSED` is routed elsewhere by
/// [`permanent_failure_error`], never here).
fn review_not_open(target: &str) -> ClientError {
    ClientError::ReviewNotOpen(format!(
        "'{target}' is not an open proposal for review — it may already be resolved (approved, rejected, \
         or withdrawn), or `current` has since moved. Run `topos review` to see the open proposals."
    ))
}

/// Resolve a `<skill>` review target to its `(skill id, workspace id)`. Prefers a LOCALLY followed skill
/// (the strict resolver + its follow-entry workspace); when the name matches no locally tracked+followed
/// skill, falls back to the workspace CATALOG over the wire so an enrolled device can review a proposal it
/// has not yet swept into local follow state (e.g. the genesis publisher, pre-`update`). If the catalog
/// read yields nothing (or fails) AND the skill is not local, the original "no tracked skill" error stands.
///
/// # Errors
/// [`ClientError::AmbiguousName`] when the name is ambiguous locally or across the enrolled catalogs;
/// [`ClientError::NoSuchSkill`] when it resolves nowhere; any other local-resolution error verbatim.
fn resolve_review_skill(
    ctx: &Ctx<'_>,
    directory_connect: &DirectoryConnect<'_>,
    skill_name: &str,
    workspace: Option<&str>,
) -> Result<(SkillId, String), ClientError> {
    match resolve_followed_skill_in_workspace(ctx, skill_name, workspace) {
        Ok((id, _lock)) => {
            let ws = workspace_of(ctx, id.as_str())?;
            Ok((id, ws))
        }
        Err(ClientError::NoSuchSkill { name }) => {
            match resolve_catalog_skill(ctx, directory_connect, &name, workspace)? {
                Some(found) => Ok(found),
                None => Err(ClientError::NoSuchSkill { name }),
            }
        }
        Err(e) => Err(e),
    }
}

/// Resolve a bare skill NAME against the enrolled workspace catalog(s) over the wire — the same
/// credentialed directory reads the review inbox uses (the catalog carries the name → custody-id map). A
/// `--workspace` filter narrows it; a name unique across the reachable catalogs resolves, a name in several
/// is [`ClientError::AmbiguousName`], a name in none is `Ok(None)`. A transport fault (or no enrollment)
/// also yields `Ok(None)` so the caller keeps the local "no tracked skill" error (the offline behavior).
fn resolve_catalog_skill(
    ctx: &Ctx<'_>,
    directory_connect: &DirectoryConnect<'_>,
    skill_name: &str,
    workspace: Option<&str>,
) -> Result<Option<(SkillId, String)>, ClientError> {
    // A transport fault or an un-enrolled install means we cannot resolve over the wire — fall back to the
    // local not-found (a revoked/removed workspace is already skipped by `build_universe_via`).
    let Ok((_base, universe)) = build_universe_via(ctx, directory_connect) else {
        return Ok(None);
    };
    let mut matches: Vec<(SkillId, String)> = Vec::new();
    for ws in &universe {
        if workspace.is_some_and(|w| w != ws.workspace_id) {
            continue;
        }
        for (name, skill_id) in &ws.skills {
            // Parse-validate the plane-minted id before it keys any local path; a malformed one is skipped.
            if name == skill_name
                && let Ok(id) = SkillId::parse(skill_id)
            {
                matches.push((id, ws.workspace_id.clone()));
            }
        }
    }
    matches.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    matches.dedup_by(|a, b| a.0.as_str() == b.0.as_str());
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches.into_iter().next().expect("len == 1"))),
        count => Err(ClientError::AmbiguousName {
            name: skill_name.to_owned(),
            count,
        }),
    }
}

/// Split a `<skill>@<hash>` review target on its first `@`. A malformed shape is a usage error
/// (`INVALID_ARGUMENT` — the target is the user's own argv token, echoed back as clap would).
fn split_target(target: &str) -> Result<(String, String), ClientError> {
    match target.split_once('@') {
        Some((skill, hash)) if !skill.is_empty() && !hash.is_empty() => {
            Ok((skill.to_owned(), hash.to_owned()))
        }
        _ => Err(ClientError::InvalidArgument(format!(
            "a review target must be `<skill>@<hash>`, got `{target}`"
        ))),
    }
}

fn skill_of(target: &str) -> String {
    target
        .split_once('@')
        .map(|(s, _)| s.to_owned())
        .unwrap_or_else(|| target.to_owned())
}

/// The paste-ready verdict next-actions a describe offers for `handle`. A four-eyes author cannot
/// approve their OWN version, so a `yours` proposal offers ONLY `--withdraw`; anyone else's offers the
/// reviewer's `--approve` and `--reject -m <reason>`.
fn verdict_next_argvs(handle: &str, yours: bool) -> Vec<Vec<String>> {
    let argv = |verb: &[&str]| -> Vec<String> {
        std::iter::once("topos")
            .chain(std::iter::once("review"))
            .chain(std::iter::once(handle))
            .chain(verb.iter().copied())
            .map(str::to_owned)
            .collect()
    };
    if yours {
        vec![argv(&["--withdraw"])]
    } else {
        vec![argv(&["--approve"]), argv(&["--reject", "-m", "<reason>"])]
    }
}

#[cfg(test)]
mod tests {
    use topos_types::{Affected, Receipt, TerminalOutcome, WireError};

    use super::{denied_review_error, permanent_failure_error, split_target, verdict_next_argvs};
    use crate::plane::WriteReceipt;

    #[test]
    fn a_yours_describe_offers_withdraw_and_never_approve() {
        // A four-eyes author can only withdraw their OWN proposal — the describe offers `--withdraw`,
        // never `--approve`; anyone else's offers approve / reject-with-reason.
        let handle = format!("deploy@{}", "a".repeat(64));
        let yours = verdict_next_argvs(&handle, true);
        let flat: Vec<&str> = yours.iter().flatten().map(String::as_str).collect();
        assert!(flat.contains(&"--withdraw"), "{flat:?}");
        assert!(!flat.contains(&"--approve"), "{flat:?}");
        assert!(!flat.contains(&"--reject"), "{flat:?}");

        let theirs = verdict_next_argvs(&handle, false);
        let flat: Vec<&str> = theirs.iter().flatten().map(String::as_str).collect();
        assert!(flat.contains(&"--approve"), "{flat:?}");
        assert!(flat.contains(&"--reject"), "{flat:?}");
        assert!(!flat.contains(&"--withdraw"), "{flat:?}");
    }

    /// A synthesized terminal PERMANENT_FAILURE write receipt as the plane's wire mapper builds one:
    /// `details_code` present ⇒ the distinguishing `details.code` (mirrored onto the flat error), else
    /// the undistinguished outcome-default code.
    fn permanent_receipt(details_code: Option<&str>) -> WriteReceipt {
        let code = details_code.unwrap_or("PERMANENT_FAILURE").to_owned();
        WriteReceipt {
            receipt: Some(Receipt {
                schema_version: 1,
                op_id: "op-1".into(),
                command: "review".into(),
                outcome: TerminalOutcome::PermanentFailure,
                workspace_id: "w_acme".into(),
                skill_id: Some("s_release".into()),
                version_id: None,
                bundle_digest: None,
                expected_generation: None,
                current_generation: None,
                created_at: "2026-07-13T00:00:00Z".into(),
                details: details_code.map(|c| serde_json::json!({ "code": c })),
            }),
            error: Some(WireError {
                code,
                outcome: TerminalOutcome::PermanentFailure,
                retryable: false,
                affected: Affected::default(),
                expected_generation: None,
                current_generation: None,
                context: serde_json::json!({}),
                next_actions: Vec::new(),
            }),
            wire_record: None,
        }
    }

    /// A terminal DENIED write receipt carrying the given wire code — the shape `denied_review_error`
    /// classifies. `receipt: None` models the receipt-LESS DENIED an old server (or a wedged stored
    /// receipt) serves, whose only signal is the flat error's code.
    fn denied_receipt(code: &str) -> WriteReceipt {
        WriteReceipt {
            receipt: None,
            error: Some(WireError {
                code: code.to_owned(),
                outcome: TerminalOutcome::Denied,
                retryable: false,
                affected: Affected::default(),
                expected_generation: None,
                current_generation: None,
                context: serde_json::json!({}),
                next_actions: Vec::new(),
            }),
            wire_record: None,
        }
    }

    #[test]
    fn a_denied_no_open_proposal_is_the_review_not_open_domain_refusal() {
        // The server DENIED a verdict on a no-longer-open proposal with the distinguishing NO_OPEN_PROPOSAL
        // code — a decided/closed proposal is a terminal outcome, rendered as the SAME honest refusal the
        // PermanentFailure "not open" arm uses (never a bare "the plane denied this operation (…)").
        let err = denied_review_error(
            &denied_receipt("NO_OPEN_PROPOSAL"),
            "release-notes@abc123def456",
        );
        assert_eq!(err.code(), "REVIEW_NOT_OPEN");
        let msg = crate::render::safe_message(&err);
        assert!(msg.contains("release-notes@abc123def456"), "{msg}");
        assert!(msg.contains("already be resolved"), "{msg}");
        assert!(msg.contains("topos review"), "{msg}");
    }

    #[test]
    fn a_denied_four_eyes_names_four_eyes_and_stays_denied() {
        // A four-eyes self-approve DENIED renders an HONEST sentence naming four-eyes (surfacing through
        // safe_message's Denied arm), and keeps the DENIED wire code for the agent to branch on.
        let err = denied_review_error(
            &denied_receipt("FOUR_EYES_REQUIRED"),
            "release-notes@abc123def456",
        );
        assert_eq!(err.code(), "DENIED");
        assert_eq!(err.outcome(), TerminalOutcome::Denied);
        let msg = crate::render::safe_message(&err);
        assert!(msg.contains("four-eyes"), "{msg}");
        assert!(msg.contains("second reviewer"), "{msg}");
        // The raw code is not what surfaces on the four-eyes case — the human reads the sentence.
        assert!(!msg.contains("FOUR_EYES_REQUIRED"), "{msg}");
    }

    #[test]
    fn a_review_verdict_on_a_resolved_proposal_is_an_honest_domain_refusal() {
        // The plane answers a verdict on an already-resolved proposal with a terminal PERMANENT_FAILURE
        // ("no proposal for this candidate and base") — no distinguishing code, no resolver named. Driven
        // through the SAME classification `map_outcome` runs (`permanent_failure_error`), the client must
        // render an HONEST domain refusal, not the transport-fault-shaped "the plane returned
        // PERMANENT_FAILURE (PermanentFailure)".
        let receipt = permanent_receipt(None);
        let err = permanent_failure_error(&receipt, "release-notes@abc123def456");
        assert_eq!(err.code(), "REVIEW_NOT_OPEN");
        assert_eq!(err.outcome(), TerminalOutcome::PermanentFailure);
        let msg = crate::render::safe_message(&err);
        assert!(msg.contains("release-notes@abc123def456"), "{msg}");
        assert!(msg.contains("already be resolved"), "{msg}");
        assert!(msg.contains("topos review"), "{msg}");
        // Never the opaque transport-shaped enum on the user surface.
        assert!(!msg.contains("PERMANENT_FAILURE"), "{msg}");
        assert!(!msg.contains("the plane returned"), "{msg}");
    }

    #[test]
    fn a_review_op_id_reuse_permanent_failure_is_not_folded_into_review_not_open() {
        // OP_ID_REUSED shares the PERMANENT_FAILURE outcome but carries a distinguishing `details.code`.
        // The wiring must keep its transport-class rendering (the code names the real cause), never the
        // "not open" domain refusal — a regression that overfolds this into REVIEW_NOT_OPEN fails here.
        let receipt = permanent_receipt(Some("OP_ID_REUSED"));
        let err = permanent_failure_error(&receipt, "release-notes@abc123def456");
        assert_ne!(err.code(), "REVIEW_NOT_OPEN");
        assert_eq!(err.code(), "PLANE_TERMINAL");
        // The distinguishing code reaches the surface (via the transport-class message), never hidden.
        let msg = crate::render::safe_message(&err);
        assert!(msg.contains("OP_ID_REUSED"), "{msg}");
    }

    #[test]
    fn a_malformed_review_target_is_a_usage_error_not_corruption() {
        for bad in ["docs", "@abc12", "docs@", ""] {
            let err = split_target(bad).unwrap_err();
            assert_eq!(err.code(), "INVALID_ARGUMENT", "{bad:?}");
            // The written guidance reaches the surface verbatim (safe_message passes it through).
            assert!(
                crate::render::safe_message(&err).contains("`<skill>@<hash>`"),
                "{bad:?}"
            );
        }
        assert!(split_target("docs@abc12").is_ok());
    }
}
