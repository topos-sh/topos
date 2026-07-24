//! `revert <skill> --to <good> [--yes]` — undo a release for the TEAM.
//!
//! A **forward** pointer-move: the server builds a new 1-parent commit `{tree: good.tree, parents:
//! [current]}` that restores the GOOD version's bytes on top of `current` — nothing is deleted (the bad
//! version stays fetchable), so it is invertible. `--to <hash>` is the sole source of the GOOD version (the
//! destination, NOT the bad one). The client computes the byte-identical forward `commit_id` the plane
//! reconstructs (over the FRESH current parent — a stale parent would be a DENIED, not a clean CONFLICT).
//! Team-only — the local go-back is `pull <skill>@<hash>`.

use topos_core::digest::to_hex;
use topos_core::identity::{self, Commit};
use topos_types::persisted::{OpKind, OpRecord};
use topos_types::results::{RevertData, RevertDescribeData};
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect, REVERT_MESSAGE};
use super::{VersionRef, resolve_followed_skill_in_workspace, resolve_version_ref, workspace_of};
use crate::ctx::Ctx;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::WriteReceipt;
use crate::{op_wal, sidecar};

/// The verb's outcome — the two-phase pair plus the byte-level no-op.
#[derive(Debug)]
pub(crate) enum RevertOutcome {
    /// Bare `revert --to` (bytes differ) — the forward-move DESCRIBE; `--yes` applies it.
    Describe {
        data: RevertDescribeData,
        yes_argv: Vec<String>,
    },
    /// good's bytes ALREADY equal current's — nothing to move. Bare says "nothing would change";
    /// `--yes` acknowledges it (a typed success). No forward commit, no POST either way.
    NoOp(RevertDescribeData),
    /// The forward move landed (`--yes`, or a WAL replay resuming an in-flight revert).
    Applied(RevertData),
}

/// Move `current` forward to the GOOD version named by `--to`, two-phase. A bare invocation DESCRIBES
/// the forward move (nothing written); `--yes` applies it. A byte-level no-op — good's bytes already
/// equal current's, detected by comparing verified bundle DIGESTS, not commit ids (a forward revert mints
/// a NEW id over IDENTICAL bytes, so an id compare would mint generation after generation) — moves
/// nothing on either path. A pending revert WAL always RESUMES to apply (re-invoking IS the resume),
/// regardless of `--yes`.
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::PendingOp`] if a DIFFERENT revert is
/// in flight; [`ClientError::Conflict`] / [`ClientError::Denied`] on the plane's verdict; an integrity
/// error if the good version does not reproduce its id; a transport failure otherwise.
pub(crate) fn revert(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    skill_name: &str,
    to: &str,
    yes: bool,
    workspace: Option<&str>,
) -> Result<RevertOutcome, ClientError> {
    // Argv is validated FIRST (a malformed hash is a usage error however un-enrolled the machine is).
    // `--to` is the sole source of the good destination — it accepts the full 64-hex id OR a short prefix
    // (resolved below against the skill's recorded history, once the skill is known).
    let to_ref = VersionRef::parse_arg(
        to,
        "`--to` must be a 64-char lowercase hex version id (or a unique prefix of at least 8 chars)",
    )?;

    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?.ok_or(ClientError::NotEnrolled)?;

    // The `--workspace` filter disambiguates a name shared across workspaces; the SIGNED scope is the
    // skill's OWN follow-entry workspace (the forward-revert commit is built against that workspace's live
    // current). You only ever revert a skill you FOLLOW (the fresh-current read needs its read creds), so
    // this is the STRICT resolve — a candidate with NO follow entry (a local-only skill that merely shares
    // the name) is dropped before the ambiguity count, and a non-followed target fails locally as "not a
    // followed skill" rather than an opaque plane rejection.
    let (id, lock) = resolve_followed_skill_in_workspace(ctx, skill_name, workspace)?;
    // The resolved display NAME leads the describe + the success line (never the opaque id).
    let name = lock.name;
    let workspace_id = workspace_of(ctx, id.as_str())?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Resolve the ref against the versions this client holds locally (a revert target is a version this
    // client has previously fetched). The resolved FULL hex is what every downstream surface carries
    // (`good`, the WAL, `reverted_to` — whose schema pins 64 hex), so a prefix never leaks into a document
    // or the wire.
    let known = super::local_version_ids(ctx, &sp)?;
    let good_commit = resolve_version_ref(&known, &to_ref)?.ok_or_else(|| {
        ClientError::InvalidArgument(format!(
            "--to '{}' matches no locally recorded version of '{skill_name}'; use the full \
             64-char version id",
            to_ref.shown()
        ))
    })?;
    let good_hex = to_hex(&good_commit);

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
        // different `--to` must settle the in-flight revert first. A matching pending revert always
        // RESUMES to apply — re-invoking IS the resume, regardless of `--yes`.
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
            // No-op detection by TREE digest, NOT commit id: after one revert, `current` is a forward
            // commit with a NEW id over IDENTICAL bytes, so an id compare would mint generation after
            // generation. Compare the good and current versions' VERIFIED bundle digests instead — the
            // good version's tree is also the forward commit's tree, so it is computed here either way.
            let good_digest = contribute::verified_version_digest(ctx, id.as_str(), good_commit)?;
            let current_digest =
                contribute::verified_version_digest(ctx, id.as_str(), current_commit)?;
            let describe = RevertDescribeData {
                skill: name.clone(),
                skill_id: id.to_string(),
                current_version_id: to_hex(&current_commit),
                reverted_to: good_hex.clone(),
                current_generation: expected,
                is_noop: good_digest == current_digest,
            };
            if describe.is_noop {
                // A byte-level no-op — good's bytes already ARE current. No forward commit is minted and
                // no write is POSTed on either path (bare says "nothing would change"; `--yes`
                // acknowledges it as a typed success).
                return Ok(RevertOutcome::NoOp(describe));
            }
            if !yes {
                // Bare = DESCRIBE: nothing is written (no op-WAL, no POST). The `next_actions` carry the
                // paste-ready `--yes` apply. A `--workspace` disambiguation is PRESERVED on it (as the
                // canonical id), so the suggested apply re-resolves to exactly the skill described —
                // never ambiguously against whatever local state the re-run finds.
                let mut yes_argv = vec![
                    "topos".to_owned(),
                    "revert".to_owned(),
                    skill_name.to_owned(),
                    "--to".to_owned(),
                    good_hex.clone(),
                ];
                if workspace.is_some() {
                    yes_argv.push("--workspace".to_owned());
                    yes_argv.push(workspace_id.clone());
                }
                yes_argv.push("--yes".to_owned());
                return Ok(RevertOutcome::Describe {
                    data: describe,
                    yes_argv,
                });
            }
            // `--yes` = build the forward commit + op record (the apply path). `good_digest` is the
            // forward commit's tree (re-derived from the verified bytes above).
            let forward = identity::commit_id(&Commit {
                parents: &[current_commit],
                tree: good_digest,
                author: &ctx.device_id,
                message: REVERT_MESSAGE,
            })
            .map_err(|_| ClientError::Corrupt("forward-revert commit id preimage".to_owned()))?;
            OpRecord {
                schema_version: PERSISTED_SCHEMA_VERSION,
                upstream: None,
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
                channel: None,
                last_receipt: None,
            }
        }
    };

    let receipt = contribute::run_write(ctx, &*transport, &sp, &rec, None)?;
    map_outcome(ctx, &sp, &rec, &receipt, &name, &good_hex).map(RevertOutcome::Applied)
}

fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    name: &str,
    good_hex: &str,
) -> Result<RevertData, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            let record = receipt.wire_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK revert carried no current pointer".into())
            })?;
            let new_gen = contribute::apply_light_advance(ctx, sp, rec, record)?;
            Ok(RevertData {
                skill_id: rec.skill_id.clone(),
                name: name.to_owned(),
                reverted_to: good_hex.to_owned(),
                new_version_id: rec.candidate_commit.clone(),
                current_generation: new_gen,
            })
        }
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: name.to_owned(),
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
