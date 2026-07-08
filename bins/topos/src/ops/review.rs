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
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect};
use super::{parse_hex32_arg, resolve_skill_in_workspace, workspace_of};
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
    workspace: Option<&str>,
) -> Result<ReviewData, ClientError> {
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
    // creds), so this is the STRICT resolve — a non-followed target fails with a clean local
    // "not a followed skill" here rather than an opaque plane rejection after a wasted round-trip.
    let (id, _lock) = resolve_skill_in_workspace(ctx, &skill_name, workspace)?;
    let workspace_id = workspace_of(ctx, id.as_str())?;
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
                schema_version: PERSISTED_SCHEMA_VERSION,
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
                // A review renames nothing — carry no name so the plane preserves the stored one.
                display_name: None,
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
