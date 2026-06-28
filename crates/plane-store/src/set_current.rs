//! The one pointer-move write — `set-current` (publish · genesis · revert), the orchestration half.
//!
//! `publish`, `revert`, and (later) `review --approve` are three intents, **one** operation: advance the
//! per-skill `current` pointer by exactly one `(epoch, seq)` step, under a compare-and-set, signing the new
//! pointer and re-rooting the migrated bytes — all in one serializable, pure-DB transaction. This module
//! does the work that happens **outside** that transaction (no filesystem op may run inside it): it
//! re-verifies the migrated candidate is renderable, derives the candidate's object set, and — for a revert
//! — constructs the forward commit. Then it drives the one transaction in [`crate::sqlite`].
//!
//! Scope here is the **backbone**: genesis + direct publish + revert + the review-required typed-fail gate.
//! The propose -> review-approve promotion, two-parent author merges, the client pull engine, and the HTTP
//! surface are later work; this is exercised in-process against a real SQLite + git store.

use topos_core::sign::{self, Commit, DeviceOp};
use topos_types::{Generation, TerminalOutcome};

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, OpId, SkillId, WorkspaceId};
use crate::lifecycle::{self, StagedCandidate};

/// The device-signed request fields that accompany a pointer-move. The signature is over the device-op
/// preimage (`topos_core::sign::device_op_preimage`) the **plane reconstructs from server-trusted values**
/// (the rehashed candidate id + bundle digest + the request scope), so a valid signature *is* the binding
/// of this device to this exact promotion — never a client-claimed commit/digest.
#[derive(Debug, Clone)]
pub struct DeviceSignedOp {
    /// The id of the device signing key (the registry selects the public key by this).
    pub device_key_id: String,
    /// The operation: `PublishDirect` (direct publish / genesis) or `Revert` in the backbone.
    pub op: DeviceOp,
    /// The raw 64-byte Ed25519 device-op signature.
    pub signature: [u8; 64],
    /// The `(epoch, seq)` the compare-and-set targets.
    pub expected: Generation,
}

/// The durable, replayable result of a pointer-move. **Terminal outcomes are values, every one receipted**
/// — an [`AuthorityError`] is reserved for an internal/integrity fault (a torn store, not a protocol
/// outcome). A retry with the same `op_id` + bound identity returns the byte-identical receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCurrentReceipt {
    /// The client-minted op id this receipt is keyed by (with the workspace + device key id).
    pub op_id: String,
    /// The terminal outcome.
    pub outcome: TerminalOutcome,
    /// The live `(epoch, seq)` — the **new** generation on `OK`, the **current** generation on `CONFLICT`.
    pub current: Option<Generation>,
    /// The serialized `SignedCurrentRecord` envelope (`OK` only) — re-served byte-identically on replay,
    /// even after `current` has advanced to a later version.
    pub signed_record: Option<Vec<u8>>,
    /// The plane signing key id (`OK` only).
    pub key_id: Option<String>,
    /// Outcome-specific structured detail (e.g. the live commit id on a first-parent-mismatch `DENIED`).
    pub details: Option<serde_json::Value>,
}

impl SetCurrentReceipt {
    /// Whether the outcome is `OK`.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.outcome == TerminalOutcome::Ok
    }
}

/// The fully-resolved, server-trusted inputs to the one transaction — built here, consumed in
/// [`crate::sqlite`]. Every identity field is the **server's** value (the rehashed candidate, the request
/// scope), never a client claim.
pub(crate) struct PromoteInput<'a> {
    pub ws: &'a WorkspaceId,
    pub skill: &'a SkillId,
    pub op_id: &'a str,
    pub op_id_bytes: [u8; 16],
    pub device_key_id: &'a str,
    pub op: DeviceOp,
    pub signature: &'a [u8; 64],
    pub expected: Generation,
    pub candidate_commit: CommitId,
    pub candidate_bundle_digest: [u8; 32],
    pub parents: &'a [CommitId],
    pub object_ids: &'a [ObjectId],
    pub created_at: &'a str,
    pub now: i64,
}

/// Drive a **publish** (or genesis) pointer-move for an already-staged-and-migrated candidate.
///
/// The candidate must already be `ingest`ed + `migrate`d (its bytes durably installed, its promotion lease
/// committed). This re-verifies renderability **before** the transaction (the migrate path defers that
/// re-check to here), then runs the one serializable write.
///
/// # Errors
/// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault; the signer must be
/// configured ([`Authority::with_plane_key`]).
pub(crate) async fn publish(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    staged: &StagedCandidate,
    device: &DeviceSignedOp,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    // The publish entry promotes a direct publish only (the public `Authority::publish` rejects any other
    // device op before ingest; a revert has its own server-constructed path). The promote path's review
    // gate keys on `PublishDirect`, so a non-direct op here would be a gate bypass — never let one through.
    debug_assert!(
        matches!(device.op, DeviceOp::PublishDirect),
        "set_current::publish requires a PublishDirect device op"
    );
    let object_ids = lifecycle::distinct_object_ids(&staged.entries);
    drive(
        authority,
        Candidate {
            ws,
            skill,
            op_id: &staged.op_id,
            commit: staged.version_id,
            bundle_digest: staged.bundle_digest,
            parents: &staged.parents,
            object_ids: &object_ids,
        },
        device,
        created_at,
        now,
    )
    .await
}

/// Reject a publish whose device op is not `PublishDirect` — a review-gate-bypass guard. A permanent,
/// receipted failure that uploaded/migrated/leased **nothing** (it runs before ingest).
pub(crate) async fn reject_non_publish_op(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    op_id: &OpId,
    device: &DeviceSignedOp,
    created_at: &str,
) -> Result<SetCurrentReceipt> {
    authority
        .db()
        .record_pretxn(pretxn(
            ws,
            &device.device_key_id,
            op_id.as_str(),
            device_op_command(device.op),
            skill,
            None,
            None,
            device.expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("a direct publish must be signed as PublishDirect"),
            created_at,
        ))
        .await
}

/// Drive a **revert** to a known-good prior version: build the forward commit `{tree: good.tree, parents:
/// [current]}`, write it durably + lease its (already-present) objects, then run the same write. `seq` still
/// advances; the pointer never moves backward.
///
/// `good`'s `bundle_digest` is read from its provenance row (recorded by the pointer-move at its own
/// promote — the git commit does not persist it). A `good` with no recorded digest (a legacy version) or no
/// `current` to revert from is a typed `PERMANENT_FAILURE`.
///
/// # Errors
/// As [`publish`]; plus a git-store fault constructing the forward commit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn revert(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    good: CommitId,
    device: &DeviceSignedOp,
    author: &str,
    message: &str,
    op_id: &OpId,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let command = device_op_command(device.op);

    // good's tree digest (recorded on its provenance row; render needs a KNOWN digest, so it cannot
    // discover this), SCOPED TO THIS SKILL — reverting to another skill's commit is refused here, so the
    // forward commit can never graft a foreign tree under this skill. Absent/legacy/foreign ⇒ refused.
    let Some(good_digest) = authority
        .db()
        .skill_commit_bundle_digest(ws, skill, good)
        .await?
    else {
        return authority
            .db()
            .record_pretxn(pretxn(
                ws,
                &device.device_key_id,
                op_id.as_str(),
                command,
                skill,
                Some(good),
                None,
                device.expected,
                TerminalOutcome::PermanentFailure,
                detail_msg("revert target has no recorded bundle digest"),
                created_at,
            ))
            .await;
    };

    // Idempotent replay BEFORE rebuilding the forward commit. The forward commit re-parents on the LIVE
    // `current`, so after the first revert commits a retry would derive a DIFFERENT commit id, and the
    // in-transaction replay (which compares the commit) would burn the op as OP_ID_REUSED rather than
    // replaying the original OK. Keying on the stable (command, skill, target digest, expected) replays it.
    if let Some(replayed) = authority
        .db()
        .replay_revert(
            ws,
            &device.device_key_id,
            op_id.as_str(),
            skill,
            good_digest,
            device.expected,
        )
        .await?
    {
        return Ok(replayed);
    }

    // The current pointer is the forward commit's first parent.
    let Some(current) = authority.db().read_current_commit(ws, skill).await? else {
        return authority
            .db()
            .record_pretxn(pretxn(
                ws,
                &device.device_key_id,
                op_id.as_str(),
                command,
                skill,
                None,
                None,
                device.expected,
                TerminalOutcome::PermanentFailure,
                detail_msg("cannot revert a skill with no current pointer"),
                created_at,
            ))
            .await;
    };

    // good's object set (from its reachability edges — no git_oid reverse-map) + its tree structure
    // (path, mode, git_oid) for the durable commit. Both come from already-recorded, present state.
    let object_ids = authority.db().commit_object_ids(ws, good).await?;
    let store = authority.store_for_write(ws)?;
    let leaves = store
        .read_tree_structure(good.0)
        .map_err(AuthorityError::integrity)?;
    let entries: Vec<(String, topos_gitstore::FileMode, [u8; 20])> = leaves
        .into_iter()
        .map(|l| (l.path, l.mode, l.git_oid))
        .collect();

    // The forward commit: same tree (digest) as good, parented on current. Re-derive its id through the
    // kernel; the store refuses a lying id.
    let parents = [current];
    let version_id = sign::commit_id(&Commit {
        parents: &[current.0],
        tree: good_digest,
        author,
        message,
    })
    .map_err(|_| AuthorityError::internal(RevertFrame))?;

    // Write the commit durably + lease its objects (already present — a no-op copy), so the lease-completion
    // gate + the re-rooting handoff in the txn behave identically to a publish.
    lifecycle::stage_forward_commit(
        authority,
        ws,
        op_id,
        CommitId(version_id),
        good_digest,
        &entries,
        &[current],
        &object_ids,
        author,
        message,
        now,
    )
    .await?;

    drive(
        authority,
        Candidate {
            ws,
            skill,
            op_id,
            commit: CommitId(version_id),
            bundle_digest: good_digest,
            parents: &parents,
            object_ids: &object_ids,
        },
        device,
        created_at,
        now,
    )
    .await
}

/// The resolved candidate a pointer-move promotes — the server-trusted identity + object set, whether it
/// came from a publish upload or a server-constructed revert.
struct Candidate<'a> {
    ws: &'a WorkspaceId,
    skill: &'a SkillId,
    op_id: &'a OpId,
    commit: CommitId,
    bundle_digest: [u8; 32],
    parents: &'a [CommitId],
    object_ids: &'a [ObjectId],
}

/// The shared driver: bridge the op id, render-verify the migrated candidate (a pre-transaction filesystem
/// read — never inside the pure-DB write), then run the one serializable write.
async fn drive(
    authority: &Authority,
    cand: Candidate<'_>,
    device: &DeviceSignedOp,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let command = device_op_command(device.op);

    // The op_id must bridge to the 16 bytes the device-op signature binds. A non-bridgeable op id (not a
    // canonical UUID) could have migrated but can never be verified — a permanent, receipted failure.
    let Some(op_id_bytes) = parse_op_id(cand.op_id.as_str()) else {
        return authority
            .db()
            .record_pretxn(pretxn(
                cand.ws,
                &device.device_key_id,
                cand.op_id.as_str(),
                command,
                cand.skill,
                Some(cand.commit),
                Some(cand.bundle_digest),
                device.expected,
                TerminalOutcome::PermanentFailure,
                detail_msg("op_id is not a canonical UUID"),
                created_at,
            ))
            .await;
    };

    // Re-verify the migrated candidate is renderable BEFORE the transaction (the migrate path defers this
    // renderability re-check to the pointer-move). A failure is a genuine fault — a `present` row whose bytes
    // a crash lost, a corrupt blob, or a database error — so PROPAGATE it (Integrity/Internal): it rolls the
    // attempt back and surfaces a corruption/DB alarm, never a *receipted* `RETRYABLE_FAILURE` (which a retry
    // would replay forever as a sticky terminal even after the underlying fault cleared).
    crate::read::render_version(authority, cand.ws, cand.commit.0, cand.bundle_digest).await?;

    let signer = authority.plane_signer()?;
    let input = PromoteInput {
        ws: cand.ws,
        skill: cand.skill,
        op_id: cand.op_id.as_str(),
        op_id_bytes,
        device_key_id: &device.device_key_id,
        op: device.op,
        signature: &device.signature,
        expected: device.expected,
        candidate_commit: cand.commit,
        candidate_bundle_digest: cand.bundle_digest,
        parents: cand.parents,
        object_ids: cand.object_ids,
        created_at,
        now,
    };
    authority.db().set_current_txn(input, signer).await
}

/// The cheap **review-required preflight** (before any ingest): a direct publish into a `review_required`
/// workspace short-circuits to `APPROVAL_REQUIRED` having uploaded/migrated/opened **nothing**. The
/// in-transaction read+lock remains the authoritative source of truth (policy may flip between here and the
/// txn). Returns `None` when the caller may proceed to ingest.
///
/// # Errors
/// [`AuthorityError::Internal`] on a database fault.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish_preflight(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    op: DeviceOp,
    device_key_id: &str,
    op_id: &OpId,
    claimed_commit: Option<CommitId>,
    claimed_digest: Option<[u8; 32]>,
    expected: Generation,
    created_at: &str,
) -> Result<Option<SetCurrentReceipt>> {
    // Only a DIRECT publish is gated; revert + genesis are forward safety/creation moves that bypass it.
    if !matches!(op, DeviceOp::PublishDirect) {
        return Ok(None);
    }
    if !authority.db().workspace_review_required(ws).await? {
        return Ok(None);
    }
    // Genesis (no current pointer yet) bypasses the gate — someone must create the first version, and it
    // cannot be proposed against a base that does not exist. (Matches the in-txn genesis branch, which
    // returns before the gate.) A cheap read, still well before any ingest.
    if authority
        .db()
        .read_current_commit(ws, skill)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    // NOTE (safe only while policy is immutable): this gated receipt binds `commit/digest = None` (nothing
    // is ingested yet), whereas the in-txn promote path binds `commit = Some(candidate)`. A single op id
    // that took the in-txn path on one attempt and this preflight on another would see those bound
    // identities disagree and burn as OP_ID_REUSED. That split requires `review_required` to FLIP between
    // attempts — impossible in v0, where the policy is fixture-seeded with no mutation surface. Unifying the
    // bound (the client's claimed commit/digest, or keying the gate on stable fields) lands with the
    // set-policy verb + the propose path, which ship together.
    let receipt = authority
        .db()
        .record_pretxn(pretxn(
            ws,
            device_key_id,
            op_id.as_str(),
            device_op_command(op),
            skill,
            claimed_commit,
            claimed_digest,
            expected,
            TerminalOutcome::ApprovalRequired,
            detail_msg("direct publish under review-required; re-run as a proposal"),
            created_at,
        ))
        .await?;
    Ok(Some(receipt))
}

/// A pre-transaction terminal outcome to record (idempotently) by the outer orchestration — every pre-txn
/// failure is receipted too (invariant: all-outcome idempotency).
pub(crate) struct PretxnReceipt<'a> {
    pub ws: &'a WorkspaceId,
    pub device_key_id: &'a str,
    pub op_id: &'a str,
    pub command: &'a str,
    pub skill: &'a SkillId,
    pub commit: Option<CommitId>,
    pub bundle_digest: Option<[u8; 32]>,
    pub expected: Generation,
    pub outcome: TerminalOutcome,
    pub details: Option<serde_json::Value>,
    pub created_at: &'a str,
}

/// Assemble a [`PretxnReceipt`] (a terse constructor so the call sites stay readable).
#[allow(clippy::too_many_arguments)]
fn pretxn<'a>(
    ws: &'a WorkspaceId,
    device_key_id: &'a str,
    op_id: &'a str,
    command: &'a str,
    skill: &'a SkillId,
    commit: Option<CommitId>,
    bundle_digest: Option<[u8; 32]>,
    expected: Generation,
    outcome: TerminalOutcome,
    details: Option<serde_json::Value>,
    created_at: &'a str,
) -> PretxnReceipt<'a> {
    PretxnReceipt {
        ws,
        device_key_id,
        op_id,
        command,
        skill,
        commit,
        bundle_digest,
        expected,
        outcome,
        details,
        created_at,
    }
}

/// The canonical command string a receipt's bound identity is keyed on (distinct per device op).
pub(crate) fn device_op_command(op: DeviceOp) -> &'static str {
    match op {
        DeviceOp::PublishDirect => "publish-direct",
        DeviceOp::PublishPropose => "publish-propose",
        DeviceOp::Revert => "revert",
        DeviceOp::ReviewApprove => "review-approve",
        DeviceOp::ReviewReject => "review-reject",
    }
}

/// A small JSON detail object carrying a human-readable message (the open `details` field).
pub(crate) fn detail_msg(msg: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "message": msg }))
}

/// Parse a **canonical** lowercase-hyphenated UUID op-id string into the raw 16 bytes the device-op
/// signature binds. `None` unless the string is *exactly* its canonical hyphenated form — `parse_str` also
/// accepts the simple (32-hex) and braced spellings, which decode to the SAME 16 bytes, so without this
/// check two distinct receipt-key strings could map to one signed identity and split the idempotency slot
/// (a varied-form retry would miss its receipt and re-execute). Requiring the canonical form keeps the
/// receipt key (the string) 1:1 with the signed identity (the 16 bytes), so they can never split.
fn parse_op_id(op_id: &str) -> Option<[u8; 16]> {
    let uuid = uuid::Uuid::parse_str(op_id).ok()?;
    (uuid.as_hyphenated().to_string() == op_id).then(|| uuid.into_bytes())
}

/// The revert commit frame was rejected by the kernel (an internal fault — the inputs are server-derived).
#[derive(Debug, thiserror::Error)]
#[error("could not build the revert commit frame")]
struct RevertFrame;
