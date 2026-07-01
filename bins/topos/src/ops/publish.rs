//! `publish [skill] [--propose] --approve <skill>@<digest>` — ship a draft to the team.
//!
//! `publish` moves `current` to the freshly-scanned draft (a direct publish, or a genesis create for a
//! never-published skill); `--propose` opens a PR without moving `current`. The client computes the
//! byte-identical `commit_id`/`bundle_digest` the plane re-derives (I-COMMIT-PARITY), gates the outward
//! ship behind `--approve <skill>@<digest>` matching the scanned bytes (refusing on mismatch — never a
//! silent mode-flip), persists an op-WAL before the first send (so an uncertain retry replays the same
//! `op_id`), and maps the plane's typed outcome. Requires prior enrollment (the workspace + the pinned
//! plane come from what `follow` wrote).

use topos_core::digest::to_hex;
use topos_core::sign::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_types::persisted::{ConflictState, Lock, OpKind, OpRecord, PlacementMap, SyncState};
use topos_types::results::{ProposeData, PublishData};
use topos_types::{Generation, SCHEMA_VERSION, TerminalOutcome};

use super::contribute::{self, ContributeConnect, PUBLISH_MESSAGE};
use super::invite::{GovernanceConnect, invite as mint_invite};
use super::sync_engine;
use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll;
use crate::error::ClientError;
use crate::plane::WriteReceipt;
use crate::{doc, op_wal, scan, sidecar};

/// The result of `publish`: either `current` moved (a direct publish) or a proposal opened (`--propose`).
pub(crate) enum PublishOutcome {
    /// A direct publish moved `current` to the draft.
    Published(PublishData),
    /// `--propose` opened a proposal (NEEDS_REVIEW); `current` did NOT move.
    Proposed(ProposeData),
}

/// The genesis base — a skill whose `current` does not exist yet is published as a zero-parent commit at
/// `(0,0)` (the plane's genesis branch creates `current` at `(1,1)`).
const GENESIS: Generation = Generation { epoch: 0, seq: 0 };

/// Ship `skill`'s draft (or, with `propose`, open a proposal).
///
/// # Errors
/// [`ClientError::Enrollment`] if not enrolled; [`ClientError::ApprovalMismatch`] if `--approve` does not
/// match the scanned bytes; [`ClientError::PublishBlocked`] if an unresolved merge conflict is present;
/// [`ClientError::Conflict`] / [`ClientError::ApprovalRequired`] / [`ClientError::Denied`] on the plane's
/// typed verdict; a signing / transport / store failure otherwise.
pub(crate) fn publish(
    ctx: &Ctx<'_>,
    connect: &ContributeConnect<'_>,
    gov_connect: &GovernanceConnect<'_>,
    skill_arg: Option<&str>,
    propose: bool,
    approve: &str,
) -> Result<PublishOutcome, ClientError> {
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

    // `--approve <skill>@<digest>` names the skill + the consent digest. A positional skill must agree.
    let (approve_skill, approved_digest) = parse_skill_at(approve)?;
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

    let (id, lock) = resolve_skill(ctx, skill_name)?;
    let sp = ctx.layout.published(&id);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &id)?;

    // Publish guard (presence-based, never a marker scan): an unresolved author merge blocks publish.
    if doc::read_doc::<ConflictState>(ctx.fs, &sp.conflict)?.is_some() {
        return Err(ClientError::PublishBlocked {
            skill: skill_name.to_owned(),
        });
    }

    let signer = DeviceSigner::load_or_generate(ctx.fs, &ctx.layout)?;
    let transport = connect(&instance.base_url);
    let map: PlacementMap = doc::read_doc(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing placement map".to_owned()))?;

    // Resume a crashed prior publish/propose for this skill (replay the SAME op_id) before minting a new
    // one — the plane returns the byte-identical receipt, so there is no double-advance / duplicate commit.
    let kinds = [OpKind::PublishDirect, OpKind::PublishPropose];
    let rec = match op_wal::find_pending_for_skill(
        ctx.fs,
        &ctx.layout,
        &workspace_id,
        id.as_str(),
        &kinds,
    )? {
        // A crashed prior publish is still in-flight: replay it ONLY if it matches THIS command (same
        // approved digest + same direct/propose mode) — otherwise refuse, so a new intent never silently
        // rides the old op's mode/bytes (the consent gate covers the replay path too).
        Some(pending) => {
            let pending_propose = matches!(pending.op, OpKind::PublishPropose);
            if pending.bundle_digest != approved_digest || pending_propose != propose {
                return Err(ClientError::PendingOp {
                    skill: skill_name.to_owned(),
                    detail: format!(
                        "a {} of {skill_name}@{} is in flight — settle it (re-run that publish), then retry",
                        if pending_propose {
                            "proposal"
                        } else {
                            "direct publish"
                        },
                        pending.bundle_digest
                    ),
                });
            }
            pending
        }
        None => build_publish_op(
            ctx,
            &sp,
            id.as_str(),
            &lock,
            &map,
            &workspace_id,
            propose,
            &approved_digest,
        )?,
    };

    let receipt = contribute::run_write(ctx, &*transport, &signer, &sp, &rec)?;
    let mut outcome = map_outcome(
        ctx,
        &sp,
        &lock,
        &map,
        &rec,
        &receipt,
        skill_name,
        &approved_digest,
    )?;

    // First-publish invite fold: a GENESIS publish (no prior `current`) also mints a shareable `/i/` link
    // pre-offering the just-published skill, so a first publish stands up a door to it. BEST-EFFORT +
    // owner-gated — minting the link signs a governance op the plane DENIES for a non-owner; on any failure
    // the publish STILL succeeds with `invite_link: None` (the pointer move is the real outcome, the link a
    // convenience). Fires only for a genesis publish (`expected == (0,0)`), never an ordinary one.
    if let PublishOutcome::Published(data) = &mut outcome
        && rec.expected_generation == GENESIS
    {
        data.invite_link = mint_invite(
            ctx,
            gov_connect,
            Vec::new(),
            None,
            vec![rec.skill_id.clone()],
        )
        .ok()
        .map(|inv| inv.invite_link);
    }
    Ok(outcome)
}

/// Build the fresh op: precondition the state, run the consent gate over the scanned draft, compute the
/// byte-identical `(commit_id, bundle_digest)`, commit the candidate into the local store (renderable for a
/// replay + local history), and assemble the [`OpRecord`] (the WAL write itself happens in `run_write`).
#[allow(clippy::too_many_arguments)]
fn build_publish_op(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    id: &str,
    lock: &Lock,
    map: &PlacementMap,
    workspace_id: &str,
    propose: bool,
    approved_digest: &str,
) -> Result<OpRecord, ClientError> {
    let sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("missing sync state".to_owned()))?;

    // Be current before publishing: a behind state (a newer `current` not yet applied) would publish on a
    // stale base and could clobber the unapplied version — surface it as a locally-detected CONFLICT
    // (pull to rebase), never a confusing server DENIED.
    if sync.applied != sync.observed {
        return Err(ClientError::Conflict {
            skill: lock.name.clone(),
            current: Some(sync.observed),
        });
    }

    // The draft = the live placement, scanned (the SAME source `diff` uses).
    let placement = sync_engine::first_placement(map)?;
    let scanned = scan::scan(std::path::Path::new(&placement))?;
    let digest = scanned.bundle_digest;
    let digest_hex = to_hex(&digest);

    // Consent gate: `--approve <skill>@<digest>` must match the digest of the bytes being shipped. Refuse
    // BEFORE signing or sending — the disclosure/integrity gate (never a silent mode-flip).
    if digest_hex != approved_digest {
        return Err(ClientError::ApprovalMismatch {
            skill: lock.name.clone(),
            expected: digest_hex,
            got: approved_digest.to_owned(),
        });
    }

    // Genesis (no `current` yet) is a zero-parent commit at (0,0); a normal publish parents on `current`.
    let (parents, expected): (Vec<[u8; 32]>, Generation) = if sync.observed == GENESIS {
        (Vec::new(), GENESIS)
    } else {
        (vec![parse_hex32(&lock.base_commit)?], sync.observed)
    };

    // The byte-identical id the plane re-derives (I-COMMIT-PARITY): author = the device id (NOT the signing
    // key id), message = the fixed publish message — both folded into `commit_id`.
    let commit_id = sign::commit_id(&Commit {
        parents: &parents,
        tree: digest,
        author: &ctx.device_id,
        message: PUBLISH_MESSAGE,
    })
    .map_err(|_| ClientError::Corrupt("commit id preimage".to_owned()))?;

    // Pin the candidate in the local store (so a replay re-renders the byte-identical snapshot, and the
    // local history/diff can reach it) BEFORE the WAL/send.
    let store = Store::open(&sp.store)?;
    let import: Vec<ImportFile<'_>> = scanned
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store.commit(commit_id, &parents, &tree, &ctx.device_id, PUBLISH_MESSAGE)?;
    sync_engine::fsync_store(ctx, &store)?;

    let op_id_bytes = ctx.ids.new_op_id();
    let op_id = uuid::Uuid::from_bytes(op_id_bytes)
        .as_hyphenated()
        .to_string();
    Ok(OpRecord {
        schema_version: SCHEMA_VERSION,
        op_id,
        workspace_id: workspace_id.to_owned(),
        skill_id: id.to_owned(),
        op: if propose {
            OpKind::PublishPropose
        } else {
            OpKind::PublishDirect
        },
        candidate_commit: to_hex(&commit_id),
        bundle_digest: digest_hex,
        expected_generation: expected,
        good: None,
        last_receipt: None,
    })
}

/// Map the plane's typed write outcome to a [`PublishOutcome`] (or a typed [`ClientError`]).
#[allow(clippy::too_many_arguments)]
fn map_outcome(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
    rec: &OpRecord,
    receipt: &WriteReceipt,
    skill_name: &str,
    approved_digest: &str,
) -> Result<PublishOutcome, ClientError> {
    match receipt.outcome() {
        TerminalOutcome::Ok => {
            // A direct publish moved `current` — advance the local state (read-your-writes).
            let signed = receipt.signed_record.as_ref().ok_or_else(|| {
                ClientError::Corrupt("an OK publish carried no signed pointer".to_owned())
            })?;
            let new_gen = contribute::apply_publish_ok(ctx, sp, lock, map, rec, signed)?;
            Ok(PublishOutcome::Published(PublishData {
                skill_id: rec.skill_id.clone(),
                version_id: rec.candidate_commit.clone(),
                bundle_digest: rec.bundle_digest.clone(),
                current_generation: new_gen,
                invite_link: None,
            }))
        }
        TerminalOutcome::NeedsReview => Ok(PublishOutcome::Proposed(ProposeData {
            proposal: format!("{skill_name}@{}", rec.candidate_commit),
            base_version_id: lock.base_commit.clone(),
            title: skill_name.to_owned(),
            body: None,
        })),
        TerminalOutcome::ApprovalRequired => Err(ClientError::ApprovalRequired {
            skill: skill_name.to_owned(),
            digest: approved_digest.to_owned(),
        }),
        TerminalOutcome::Conflict => Err(ClientError::Conflict {
            skill: skill_name.to_owned(),
            current: receipt.error.as_ref().and_then(|e| e.current_generation),
        }),
        TerminalOutcome::Denied => Err(ClientError::Denied(denied_code(receipt))),
        // Any other terminal class (RetryableFailure / Unavailable / PermanentFailure / …) is surfaced
        // verbatim, not flattened to a generic transport error.
        _ => Err(contribute::plane_terminal(receipt)),
    }
}

/// The wire error code on a DENIED (for the agent to branch on); never a secret.
fn denied_code(receipt: &WriteReceipt) -> String {
    receipt
        .error
        .as_ref()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| "DENIED".to_owned())
}

/// Split a `<skill>@<digest>` consent token on its first `@`.
///
/// # Errors
/// [`ClientError::Corrupt`] if the token is not `<skill>@<digest>`.
fn parse_skill_at(token: &str) -> Result<(String, String), ClientError> {
    match token.split_once('@') {
        Some((skill, rest)) if !skill.is_empty() && !rest.is_empty() => {
            Ok((skill.to_owned(), rest.to_owned()))
        }
        _ => Err(ClientError::Corrupt(format!(
            "--approve must be `<skill>@<digest>`, got `{token}`"
        ))),
    }
}
