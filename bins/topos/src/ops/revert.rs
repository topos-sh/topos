//! `revert <skill> --to <good> --approve <skill>@<hash> [--confirm]` — undo a release for the TEAM.
//!
//! A **forward** pointer-move: the server builds a new 1-parent commit `{tree: good.tree, parents:
//! [current]}` that restores the GOOD version's bytes on top of `current` — nothing is deleted (the bad
//! version stays fetchable), so it is invertible. `--to <hash>` is the GOOD version (the destination, NOT
//! the bad one); `--approve <skill>@<hash>` binds that same good version id (the disclosed consent). The
//! client computes the byte-identical forward `commit_id` the plane reconstructs (over the FRESH current
//! parent — a stale parent would be a DENIED, not a clean CONFLICT). Team-only — the local go-back is
//! `pull <skill>@<hash>`.

use topos_core::digest::to_hex;
use topos_core::sign::{self, Commit};
use topos_types::persisted::{OpKind, OpRecord};
use topos_types::results::RevertData;
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect, REVERT_MESSAGE};
use super::{
    VersionRef, resolve_skill_in_workspace, resolve_version_ref, workspace_of,
};
use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::WriteReceipt;
use crate::{op_wal, sidecar};

/// Move `current` forward to the GOOD version named by `--to`.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::ApprovalMismatch`] if `--approve` does not
/// name the same good version as `--to`; [`ClientError::ConfirmRequired`] for a no-op revert without
/// `--confirm`; [`ClientError::Conflict`] / [`ClientError::Denied`] on the plane's verdict; an integrity
/// error if the good version does not reproduce its id; a transport failure otherwise.
pub(crate) fn revert(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    skill_arg: Option<&str>,
    to: &str,
    approve: &str,
    confirm: bool,
    workspace: Option<&str>,
) -> Result<RevertData, ClientError> {
    // Argv is validated FIRST (a malformed hash or token is a usage error however un-enrolled the
    // machine is). Both `--to` and the `--approve` hash accept the full 64-hex id OR a short prefix
    // (resolved below against the skill's recorded history, once the skill is known); they must name the
    // SAME good version, and `--approve` the same skill as any positional.
    let to_ref = VersionRef::parse_arg(
        to,
        "`--to` must be a 64-char lowercase hex version id (or a unique prefix of at least 8 chars)",
    )?;
    let (approve_skill, approve_hash) = split_skill_at(approve)?;
    let approve_ref = VersionRef::parse_arg(
        &approve_hash,
        "`--approve` must be `<skill>@<hash>` naming the good version — a 64-char lowercase hex id \
         (or a unique prefix of at least 8 chars)",
    )?;

    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or_else(|| {
        ClientError::Enrollment("not enrolled; run `topos follow <link>` first".into())
    })?;
    let skill_name = match skill_arg {
        Some(s) if s != approve_skill => {
            return Err(ClientError::ApprovalMismatch {
                skill: s.to_owned(),
                expected: format!("skill '{approve_skill}' (from --approve)"),
                got: format!("skill '{s}' (positional)"),
            });
        }
        Some(s) => s,
        None => &approve_skill,
    };

    // The `--workspace` filter disambiguates a name shared across workspaces; the SIGNED scope is the
    // skill's OWN follow-entry workspace (the forward-revert commit is built against that workspace's live
    // current). You only ever revert a skill you FOLLOW (the fresh-current read needs its read creds), so
    // this is the STRICT resolve — a non-followed target fails locally as "not a followed skill" rather
    // than an opaque plane rejection.
    let (id, _lock) = resolve_skill_in_workspace(ctx, skill_name, workspace)?;
    let workspace_id = workspace_of(ctx, id.as_str())?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Resolve both refs against the skill's recorded pointer history (a revert target is a version that
    // was `current` at some point this client verified — exactly what `recorded` holds). The resolved
    // FULL hex is what every downstream surface carries (`good`, the WAL, `reverted_to` — whose schema
    // pins 64 hex), so a prefix never leaks into a document or the wire.
    let recorded = super::recorded_history(ctx, &sp)?;
    let good_commit = resolve_version_ref(&recorded, &to_ref)?.ok_or_else(|| {
        ClientError::InvalidArgument(format!(
            "--to '{}' matches no locally recorded version of '{skill_name}'; use the full \
             64-char version id",
            to_ref.shown()
        ))
    })?;
    let good_hex = to_hex(&good_commit);
    let approve_commit = resolve_version_ref(&recorded, &approve_ref)?;
    if approve_commit != Some(good_commit) {
        return Err(ClientError::ApprovalMismatch {
            skill: approve_skill,
            expected: format!("good version {good_hex} (from --to)"),
            got: format!("approved version {}", approve_ref.shown()),
        });
    }

    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let transport = connect(&instance.base_url);

    let kinds = [OpKind::Revert];
    let rec = match op_wal::find_pending_for_skill(
        ctx.fs,
        &ctx.layout,
        &workspace_id,
        id.as_str(),
        &kinds,
    )? {
        // Replay a crashed prior revert ONLY if it targets the SAME good version as this command; a
        // different `--to` must settle the in-flight revert first.
        Some(pending) => {
            if pending.good.as_deref() != Some(good_hex.as_str()) {
                return Err(ClientError::PendingOp {
                    skill: skill_name.to_owned(),
                    detail: format!(
                        "a revert to {} is in flight — settle it (re-run that revert), then retry",
                        pending.good.as_deref().unwrap_or("<unknown>")
                    ),
                });
            }
            pending
        }
        None => {
            // The forward commit parents on (and `expected` targets) the FRESH live current — the server
            // builds + signature-checks the forward commit against its live parent before the CAS.
            let (current_commit, expected) =
                contribute::fresh_current(ctx, id.as_str(), &workspace_id)?;
            if good_commit == current_commit && !confirm {
                return Err(ClientError::ConfirmRequired {
                    reason: "the --to version is already current; reverting is a no-op".to_owned(),
                });
            }
            // The good version's tree digest = the forward commit's tree (re-derived from bytes + verified).
            let good_digest = contribute::verified_version_digest(ctx, id.as_str(), good_commit)?;
            let forward = sign::commit_id(&Commit {
                parents: &[current_commit],
                tree: good_digest,
                author: &ctx.device_id,
                message: REVERT_MESSAGE,
            })
            .map_err(|_| ClientError::Corrupt("forward-revert commit id preimage".to_owned()))?;
            OpRecord {
                schema_version: PERSISTED_SCHEMA_VERSION,
                op_id: contribute::new_op_id(ctx),
                workspace_id: workspace_id.clone(),
                skill_id: id.to_string(),
                op: OpKind::Revert,
                candidate_commit: to_hex(&forward),
                bundle_digest: to_hex(&good_digest),
                expected_generation: expected,
                good: Some(good_hex.clone()),
                // A revert renames nothing — carry no name so the plane preserves the stored one.
                display_name: None,
                last_receipt: None,
            }
        }
    };

    let receipt = contribute::run_write(ctx, &*transport, &signer, &sp, &rec)?;
    map_outcome(ctx, &sp, &rec, &receipt, skill_name, &good_hex)
}

fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    skill_name: &str,
    good_hex: &str,
) -> Result<RevertData, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            let signed = receipt.signed_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK revert carried no signed pointer".into())
            })?;
            let new_gen = contribute::apply_light_advance(ctx, sp, rec, signed)?;
            Ok(RevertData {
                skill_id: rec.skill_id.clone(),
                reverted_to: good_hex.to_owned(),
                new_version_id: rec.candidate_commit.clone(),
                current_generation: new_gen,
            })
        }
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: skill_name.to_owned(),
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

/// Split a `<skill>@<hash>` token on its first `@`. A malformed shape is a usage error
/// (`INVALID_ARGUMENT` — the token is the user's own argv, echoed back as clap would).
fn split_skill_at(token: &str) -> Result<(String, String), ClientError> {
    match token.split_once('@') {
        Some((skill, rest)) if !skill.is_empty() && !rest.is_empty() => {
            Ok((skill.to_owned(), rest.to_owned()))
        }
        _ => Err(ClientError::InvalidArgument(format!(
            "--approve must be `<skill>@<hash>`, got `{token}`"
        ))),
    }
}
