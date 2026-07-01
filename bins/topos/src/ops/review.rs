//! `review <skill>@<hash> --approve | --reject` — resolve a proposal (the `gh pr review` model).
//!
//! `--approve` moves `current` to the candidate (a compare-and-set on the proposal's base; a stale base
//! re-dos); `--reject` declines a proposal (reviewer) or withdraws your own (proposer). The reviewer binds
//! the proposal's RECORDED identity (its `commit_id` = the `@hash`, its `bundle_digest` re-derived from the
//! fetched bytes and asserted to reproduce the hash — so a tampered plane can't get the reviewer to sign
//! over forged bytes) at `expected` = the FRESH `current` generation (which equals a reviewable proposal's
//! base, so a reviewer who has not pulled is still correct). Viewing the change is `diff`; this verb decides.

use topos_core::digest::to_hex;
use topos_types::persisted::{OpKind, OpRecord};
use topos_types::results::{ReviewData, ReviewDecision};
use topos_types::{SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect};
use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::WriteReceipt;
use crate::{op_wal, sidecar};

/// Approve or reject the proposal named `<skill>@<hash>`.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::Conflict`] on a stale base (approve);
/// [`ClientError::Denied`] on four-eyes self-approve / not-a-reviewer / an already-resolved proposal; an
/// integrity error if the fetched proposal does not reproduce its `@hash`; a transport failure otherwise.
pub(crate) fn review(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    target: &str,
    approve: bool,
) -> Result<ReviewData, ClientError> {
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    let workspace_id = enroll::read_user(ctx.fs, &ctx.layout)?
        .map(|u| u.workspace_id)
        .ok_or_else(|| {
            ClientError::Enrollment(
                "could not determine your workspace; complete enrollment with `topos follow` first"
                    .into(),
            )
        })?;

    // `<skill>@<hash>` — the proposal's skill + its candidate commit id.
    let (skill_name, proposal_hex) = split_target(target)?;
    let proposal_commit = parse_hex32(&proposal_hex)?;
    let (id, _lock) = resolve_skill(ctx, &skill_name)?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let transport = connect(&instance.base_url);

    // Resume a crashed prior review for this skill before minting a new op.
    let kinds = [OpKind::ReviewApprove, OpKind::ReviewReject];
    let rec = match op_wal::find_pending_for_skill(
        ctx.fs,
        &ctx.layout,
        &workspace_id,
        id.as_str(),
        &kinds,
    )? {
        // Replay a crashed prior review ONLY if it matches THIS command (same proposal + same
        // approve/reject verdict); a different intent must settle the in-flight op first.
        Some(pending) => {
            let pending_approve = matches!(pending.op, OpKind::ReviewApprove);
            if pending.candidate_commit != proposal_hex || pending_approve != approve {
                return Err(ClientError::PendingOp {
                    skill: skill_name.clone(),
                    detail: format!(
                        "a {} of {skill_name}@{} is in flight — settle it (re-run that review), then retry",
                        if pending_approve {
                            "review --approve"
                        } else {
                            "review --reject"
                        },
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
                schema_version: SCHEMA_VERSION,
                op_id: contribute::new_op_id(ctx),
                workspace_id: workspace_id.clone(),
                skill_id: id.to_string(),
                op: if approve {
                    OpKind::ReviewApprove
                } else {
                    OpKind::ReviewReject
                },
                candidate_commit: proposal_hex.clone(),
                bundle_digest: to_hex(&bundle_digest),
                expected_generation: expected,
                good: None,
                last_receipt: None,
            }
        }
    };

    let receipt = contribute::run_write(ctx, &*transport, &signer, &sp, &rec)?;
    map_outcome(ctx, &sp, &rec, &receipt, target)
}

fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    target: &str,
) -> Result<ReviewData, ClientError> {
    let decision = if matches!(rec.op, OpKind::ReviewApprove) {
        ReviewDecision::Approve
    } else {
        ReviewDecision::Reject
    };
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            // Approve moved `current` to the proposal — advance the reviewer's floor (the bytes land at the
            // next pull). Reject moves nothing (the plane returns OK with no signed pointer).
            let current_generation = if matches!(decision, ReviewDecision::Approve) {
                let signed = receipt.signed_record.as_ref().ok_or_else(|| {
                    ClientError::Corrupt("an approved proposal carried no signed pointer".into())
                })?;
                Some(contribute::apply_light_advance(ctx, sp, rec, signed)?)
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

/// Split a `<skill>@<hash>` review target on its first `@`.
fn split_target(target: &str) -> Result<(String, String), ClientError> {
    match target.split_once('@') {
        Some((skill, hash)) if !skill.is_empty() && !hash.is_empty() => {
            Ok((skill.to_owned(), hash.to_owned()))
        }
        _ => Err(ClientError::Corrupt(format!(
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
