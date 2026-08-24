//! `revert <skill> --to <good> [--yes]` — undo a release for the TEAM.
//!
//! A **forward** pointer-move: the server builds a new 1-parent commit `{tree: good.tree, parents:
//! [current]}` that restores the GOOD version's bytes on top of `current` — nothing is deleted (the bad
//! version stays fetchable), so it is invertible. `--to <hash>` is the sole source of the GOOD version (the
//! destination, NOT the bad one). The client computes the byte-identical forward `commit_id` the plane
//! reconstructs (over the FRESH current parent — a stale parent would be a DENIED, not a clean CONFLICT).
//! Team-only — the local go-back is `pull <skill>@<hash>`.
//!
//! The verb acts on the scope you stand in, like every other: the bundle resolves in the cwd
//! chain's project stores first, then the machine's (`-g` names the machine outright).

use topos_core::digest::to_hex;
use topos_core::identity::{self, Commit};
use topos_types::persisted::{OpKind, OpRecord};
use topos_types::results::{RevertData, RevertDescribeData};
use topos_types::{PERSISTED_SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect, REVERT_MESSAGE};
use super::reconcile::SessionConnect;
use super::{
    StoreScope, VersionRef, resolve_followed_skill_in_scope, resolve_version_ref, workspace_of,
};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::plane::WriteReceipt;
use crate::{op_wal, sessions::Session, sidecar};

/// The seams `revert` needs: the contribute lane it POSTs the forward move on, and the per-session
/// transports it reads the workspace's version history through — the same history `topos log`
/// prints, which is where a short id a person copies off that listing comes from.
pub(crate) struct RevertConnectors<'a> {
    /// The contribute-write lane (the forward move's POST).
    pub contribute: &'a ContributeConnect<'a>,
    /// The per-session transports (each read rides its session's own credential).
    pub session: &'a SessionConnect<'a>,
}

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
    connectors: &RevertConnectors<'_>,
    skill_name: &str,
    to: &str,
    yes: bool,
    workspace: Option<&str>,
    scope: StoreScope,
) -> Result<RevertOutcome, ClientError> {
    // Argv is validated FIRST (a malformed hash is a usage error however un-enrolled the machine is).
    // `--to` is the sole source of the good destination — it accepts the full 64-hex id OR a short prefix
    // (resolved below against the skill's recorded history, once the skill is known).
    let to_ref = VersionRef::parse_arg(
        to,
        "`--to` must be a 64-char lowercase hex version id (or a unique prefix of at least 8 chars)",
    )?;

    // The `--workspace` filter disambiguates a name shared across workspaces; the SIGNED scope is the
    // skill's OWN follow-entry workspace (the forward-revert commit is built against that workspace's live
    // current). You only ever revert a skill you FOLLOW (the fresh-current read needs its read creds), so
    // this is the STRICT resolve — a candidate with NO follow entry (a local-only skill that merely shares
    // the name) is dropped before the ambiguity count, and a non-followed target fails locally as "not a
    // followed skill" rather than an opaque plane rejection.
    //
    // Resolved WHERE YOU STAND (or on the machine, with `-g`): the copy a project checkout
    // delivers lives in that checkout's own store, and every per-skill read and write below rides
    // the OWNING store's layout. The machine-level seams — the sessions, the plane, the follow
    // state, the cwd roots — stay the outer ctx's.
    let (layout, id, lock) = resolve_followed_skill_in_scope(ctx, skill_name, workspace, scope)?;
    let outer_ctx = ctx;
    let store_ctx = super::pull::ctx_with_layout(ctx, &layout);
    let ctx = &store_ctx;
    // The KIND decides whether this verb applies at all, asked before any version is resolved.
    let placements: Vec<String> = crate::doc::read_map(ctx.fs, &ctx.layout.published(&id).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    if let Some(refusal) = crate::bundle_kind::refuse_version_verb(
        crate::bundle_kind::VersionVerb::Revert,
        &lock.name,
        crate::bundle_kind::classify(ctx, id.as_str(), &placements).or_skill(),
    ) {
        return Err(refusal);
    }
    // The resolved display NAME leads the describe + the success line (never the opaque id).
    let name = lock.name;
    let workspace_id = workspace_of(ctx, id.as_str())?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // The SESSION lane: the skill's own workspace names the session whose credential signs — and,
    // for a short `--to`, the session whose credential reads the workspace's history. Sessions
    // are a machine fact, read from the machine store whatever scope holds the copy.
    let all = crate::sessions::read_sessions(outer_ctx.fs, &outer_ctx.layout)?;
    let lane_session = all
        .sessions
        .iter()
        .find(|s| s.workspace_id == workspace_id)
        .ok_or_else(|| ClientError::SessionRequired {
            address: "<workspace-address>".to_owned(),
            message: "no session for this workspace — run `topos login <workspace-address>` \
                          first"
                .into(),
        })?;

    // Resolve the ref against WHAT THE WORKSPACE HAS PUBLISHED — the same versions `topos log`
    // prints — not merely the ones this machine happens to have applied. A short id is something a
    // person copies off that listing, and a machine that never took a version still reverts the
    // team to it; refusing with "matches no locally recorded version" over an id the listing had
    // just shown made the documented prefix form unusable. The resolved FULL hex is what every
    // downstream surface carries (`good`, the WAL, `reverted_to` — whose schema pins 64 hex), so a
    // prefix never leaks into a document or the wire.
    let mut known = super::local_version_ids(ctx, &sp)?;
    if let VersionRef::Prefix(_) = &to_ref {
        known.extend(workspace_version_ids(
            connectors,
            lane_session,
            &workspace_id,
            id.as_str(),
        ));
    }
    let good_commit = resolve_version_ref(&known, &to_ref)?.ok_or_else(|| {
        ClientError::InvalidArgument(format!(
            "--to '{}' matches no version of '{skill_name}' in this workspace's history — \
             `topos log {skill_name}` lists them",
            to_ref.shown()
        ))
    })?;
    let good_hex = to_hex(&good_commit);

    let transport = (connectors.contribute)(&lane_session.base_url, Some(&lane_session.credential));

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
                // The SHORT id, the way every listing prints it and every person copies it. A
                // 64-char hex in a paste-ready line is a line nobody reads, and `--to` resolves a
                // prefix against this workspace's whole history — the same set the id came from.
                let mut yes_argv = vec!["topos".to_owned(), "revert".to_owned()];
                // The scope flag rides where a person writes it: right after the verb.
                if scope == StoreScope::Machine {
                    yes_argv.push("-g".to_owned());
                }
                yes_argv.extend([
                    skill_name.to_owned(),
                    "--to".to_owned(),
                    crate::render::short(&good_hex).to_owned(),
                ]);
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
                bundle_kind: None,
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
                base_version: None,
                republished_version: None,
                last_receipt: None,
            }
        }
    };

    let receipt = contribute::run_write(ctx, &*transport, &sp, &rec, None)?;
    let workspace = crate::ops::session_workspace_ref(lane_session);
    map_outcome(ctx, &sp, &rec, &receipt, &name, &good_hex, &workspace).map(RevertOutcome::Applied)
}

/// The 64-hex version ids the WORKSPACE holds for this bundle — the same list `topos log` prints
/// under the plane half. Best-effort: a transport fault, an unreadable answer, or a workspace that
/// has never heard of the bundle contributes nothing, and the local history still answers alone.
fn workspace_version_ids(
    connectors: &RevertConnectors<'_>,
    session: &Session,
    workspace_id: &str,
    skill_id: &str,
) -> Vec<String> {
    (connectors.session)(session)
        .directory
        .skill_log(workspace_id, skill_id)
        .map(|log| log.versions.into_iter().map(|v| v.version_id).collect())
        .unwrap_or_default()
}

fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    name: &str,
    good_hex: &str,
    workspace: &topos_types::results::WorkspaceRef,
) -> Result<RevertData, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            let record = receipt.wire_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK revert carried no current pointer".into())
            })?;
            let new_gen = contribute::apply_light_advance(ctx, sp, rec, record)?;
            Ok(RevertData {
                skill_id: rec.skill_id.clone(),
                workspace: Some(workspace.clone()),
                name: name.to_owned(),
                reverted_to: good_hex.to_owned(),
                new_version_id: rec.candidate_commit.clone(),
                current_generation: new_gen,
                // The cwd project's lock moves with the revert: the forward commit is the new
                // current, and the checkout the command stood in runs it at its next update.
                project_lock: crate::ops::advance_project_lock(
                    ctx,
                    name,
                    &rec.candidate_commit,
                    &workspace.host,
                    &workspace.name,
                ),
            })
        }
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: name.to_owned(),
            current: receipt.error.as_ref().and_then(|e| e.current_generation),
            // `revert` acts on the store this ctx names — no cross-scope resolution — so the
            // rebase it offers is spelled for that same store.
            global: !ctx.layout.is_project_scope(),
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
