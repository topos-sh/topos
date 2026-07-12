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
use crate::plane::WriteReceipt;
use crate::{op_wal, sidecar};

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
) -> Result<ReviewOutcome, ClientError> {
    match (target, verdict) {
        (None, None) => review_inbox(ctx, connectors, workspace).map(ReviewOutcome::Inbox),
        (Some(t), None) => review_describe(ctx, connectors, t, workspace),
        (Some(t), Some(v)) => {
            review(ctx, connectors.contribute, t, v, workspace).map(ReviewOutcome::Applied)
        }
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
    let base_url = base_url.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    // Fold the caller's principal to the canonical form so the outbox match is address-shape-agnostic.
    let me_principal = enroll::read_user(ctx.fs, &ctx.layout)?
        .and_then(|u| u.principal)
        .map(|p| topos_core::identity::canonical_principal(&p));
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
            let mine = me_principal
                .as_deref()
                .is_some_and(|me| me == topos_core::identity::canonical_principal(&p.proposer));
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
) -> Result<ReviewOutcome, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
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
    let (id, _lock) = resolve_followed_skill_in_workspace(ctx, &skill_name, workspace)?;
    let workspace_id = workspace_of(ctx, id.as_str())?;
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
    // The diff against current — the same plane-diff machinery `diff` runs (`current..<proposal>`).
    let diff = super::diff(
        ctx,
        &skill_name,
        Some(&format!("current..{}", proposal.version_id)),
    )?
    .diff;
    let handle = format!("{}@{}", skill_name, proposal.version_id);
    let next_argvs = vec![
        vec![
            "topos".to_owned(),
            "review".to_owned(),
            handle.clone(),
            "--approve".to_owned(),
        ],
        vec![
            "topos".to_owned(),
            "review".to_owned(),
            handle.clone(),
            "--reject".to_owned(),
            "-m".to_owned(),
            "<reason>".to_owned(),
        ],
    ];
    Ok(ReviewOutcome::Describe {
        data: Box::new(ReviewDescribeData {
            proposal: handle,
            skill: skill_name,
            proposer: proposal.proposer,
            message: proposal.message,
            base_version_id: proposal.base_version_id,
            stale: proposal.stale,
            diff,
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
    connect: &ContributeConnect<'_>,
    target: &str,
    verdict: ReviewVerdict,
    workspace: Option<&str>,
) -> Result<ReviewData, ClientError> {
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

    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    // Resolve the proposal's skill (a `--workspace` filter disambiguates a name shared across
    // workspaces), then bind the SIGNED scope to that skill's OWN follow-entry workspace. You only ever
    // review a proposal for a skill you FOLLOW (the fresh-current read + candidate fetch need its read
    // creds), so this is the STRICT resolve — a candidate with NO follow entry (a local-only skill that
    // merely shares the name) is dropped before the ambiguity count, and a non-followed target fails with
    // a clean local "not a followed skill" here rather than an opaque plane rejection after a wasted trip.
    let (id, _lock) = resolve_followed_skill_in_workspace(ctx, &skill_name, workspace)?;
    let workspace_id = workspace_of(ctx, id.as_str())?;
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
            // Bind the proposal's RECORDED bundle digest — re-derived from the fetched bytes + asserted to
            // reproduce the named `@hash` (consent re-derivation).
            let bundle_digest =
                contribute::verified_version_digest(ctx, id.as_str(), proposal_commit)?;
            OpRecord {
                schema_version: PERSISTED_SCHEMA_VERSION,
                op_id: contribute::new_op_id(ctx),
                workspace_id: workspace_id.clone(),
                skill_id: id.to_string(),
                op: this_kind,
                candidate_commit: proposal_hex.clone(),
                bundle_digest: to_hex(&bundle_digest),
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
        TerminalOutcome::Denied => Err(ClientError::Denied(
            receipt
                .error
                .as_ref()
                .map(|e| e.code.clone())
                .unwrap_or_else(|| "DENIED".to_owned()),
        )),
        _ => Err(contribute::plane_terminal(receipt)),
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

#[cfg(test)]
mod tests {
    use super::split_target;

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
