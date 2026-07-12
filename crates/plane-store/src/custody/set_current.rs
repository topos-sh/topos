//! The one pointer-move write — `set-current` (publish · genesis · revert), the orchestration half.
//!
//! `publish`, `revert`, and (later) `review --approve` are three intents, **one** operation: advance the
//! per-skill `current` pointer by exactly one `(epoch, seq)` step, under a compare-and-set, re-rooting
//! the migrated bytes — all in one serializable, pure-DB transaction. This module
//! does the work that happens **outside** that transaction (no filesystem op may run inside it): it
//! re-verifies the migrated candidate is renderable, derives the candidate's object set, and — for a revert
//! — constructs the forward commit. Then it drives the one transaction in [`crate::db`].
//!
//! Scope here is the **backbone**: genesis + direct publish + revert + the review-required typed-fail gate.
//! The propose -> review-approve promotion, two-parent author merges, the client pull engine, and the HTTP
//! surface are later work; this is exercised in-process against a real Postgres + git store.

use topos_core::identity::{self, Commit};
use topos_types::{Generation, TerminalOutcome};

use crate::actor::WriteActor;
use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, OpId, SkillId, WorkspaceId};
use crate::lifecycle::{self, StagedCandidate};

/// The pointer-move operations — the server's op vocabulary. Dispatches the transaction's op tail and
/// keys each receipt's canonical `command` string; the CLIENT never carries it (the route names the op).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceOp {
    /// A direct publish (or the genesis create).
    PublishDirect,
    /// `publish --propose` — open a proposal, move nothing.
    PublishPropose,
    /// `revert --to` — the forward promote of a prior version's tree.
    Revert,
    /// `review --approve` — promote the locked open proposal sideways.
    ReviewApprove,
    /// `review --reject` — the standalone status flip (with its mandatory reason).
    ReviewReject,
    /// `review --withdraw` — the AUTHOR retracting their own open proposal (a status flip to
    /// `closed`, no verdict notice — the author did it).
    ReviewWithdraw,
}

/// The device-lane request as PRESENTED on the wire: the workspace credential (a LIVE bearer secret)
/// plus the op + CAS target. The public entry resolves the credential over the pool (an unknown one
/// is a synthesized pre-auth DENIED, never persisted) into the internal `DeviceOpRequest`; the
/// transaction then re-authenticates in-transaction by the credential's sha256.
#[derive(Clone)]
pub struct DeviceOpAuth {
    /// The presented workspace credential (only its sha256 is ever stored or compared server-side).
    pub credential: String,
    /// The operation the route names (`PublishDirect` / `PublishPropose` / `Revert` / `ReviewApprove`
    /// / `ReviewReject`) — the client never carries it.
    pub op: DeviceOp,
    /// The `(epoch, seq)` the compare-and-set targets.
    pub expected: Generation,
}

// `credential` is a LIVE bearer secret — redact it so a formatted request value (a debug trace, a
// panic message) can never mint a second custody surface for it.
impl std::fmt::Debug for DeviceOpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceOpAuth")
            .field("credential", &"<redacted>")
            .field("op", &self.op)
            .field("expected", &self.expected)
            .finish()
    }
}

/// The device-lane request fields that accompany a pointer-move — built by the public entry AFTER its
/// pool pre-resolution of the presented workspace credential (an unknown credential never constructs
/// one). `credential_sha256` is what the transaction re-authenticates by (the in-transaction registry
/// lookup, serialized with the write); `device_key_id` is the resolved row's stable name, keying the
/// pre-transaction receipt machinery. Every identity the receipt binds is the **server's** value (the
/// rehashed candidate id + bundle digest + the request scope), never a client claim.
#[derive(Debug, Clone)]
pub(crate) struct DeviceOpRequest {
    /// The presented workspace credential's sha256 (the transaction's authentication input).
    pub credential_sha256: [u8; 32],
    /// The pool-resolved device key id (the receipts/audit actor; re-verified in-transaction).
    pub device_key_id: String,
    /// The operation: `PublishDirect` (direct publish / genesis) or `Revert` in the backbone.
    pub op: DeviceOp,
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
    /// The canonical command this op carried (`publish-direct` / `publish-propose` / `revert` /
    /// `review-approve` / `review-reject`) — part of the bound identity a same-`op_id` retry must match.
    pub command: String,
    /// The skill the op targets — part of the bound identity.
    pub skill_id: String,
    /// The candidate's server-rehashed version id. `None` for an outcome that ingested no version (a
    /// rejected key-reuse, or a pre-ingest typed failure), never a client-claimed value.
    pub version_id: Option<CommitId>,
    /// The candidate's server-rehashed bundle digest — `None` whenever `version_id` is (no version ingested).
    pub bundle_digest: Option<[u8; 32]>,
    /// The `(epoch, seq)` the compare-and-set targeted — part of the bound identity.
    pub expected: Generation,
    /// The terminal outcome.
    pub outcome: TerminalOutcome,
    /// The live `(epoch, seq)` — the **new** generation on `OK`, the **current** generation on `CONFLICT`.
    pub current: Option<Generation>,
    /// The serialized `WireCurrentRecord` document (`OK` only) — re-served byte-identically on replay,
    /// even after `current` has advanced to a later version.
    pub record: Option<Vec<u8>>,
    /// The server-stamped creation timestamp — STORED (not recomputed), so a lost-ack retry replays it
    /// byte-for-byte.
    pub created_at: String,
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
/// [`crate::db`]. Every identity field is the **server's** value (the rehashed candidate, the request
/// scope), never a client claim.
pub(crate) struct PromoteInput<'a> {
    pub ws: &'a WorkspaceId,
    pub skill: &'a SkillId,
    pub op_id: &'a str,
    /// Which lane the request arrived on — the txn body branches on it ONLY at its authz step.
    pub actor: WriteActor<'a>,
    pub op: DeviceOp,
    pub expected: Generation,
    pub candidate_commit: CommitId,
    pub candidate_bundle_digest: [u8; 32],
    pub parents: &'a [CommitId],
    pub object_ids: &'a [ObjectId],
    /// The skill's advisory display name (`None` ⇒ keep any existing name). UNSIGNED — never in the
    /// device-op preimage or the bundle digest; recorded on the CATALOG row (and, at a genesis, it
    /// seeds the catalog name).
    pub display_name: Option<&'a str>,
    /// The `--to` channel placement (`None` ⇒ no explicit placement; a genesis defaults to `everyone`).
    pub channel: Option<&'a str>,
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
/// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    staged: &StagedCandidate,
    device: &DeviceOpRequest,
    display_name: Option<&str>,
    channel: Option<&str>,
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
            display_name,
            channel,
        },
        WriteActor::Device {
            credential_sha256: device.credential_sha256,
            device_key_id: &device.device_key_id,
        },
        device.op,
        device.expected,
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
    device: &DeviceOpRequest,
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
            detail_msg("a direct publish must arrive as PublishDirect"),
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
/// The `actor` names the lane (a device credential, or a verified web session) and `expected` is that lane's CAS
/// target generation. The forward commit is server-constructed identically for BOTH lanes, so a keyless web
/// session names only the full `good` id. Every pre-transaction failure routes through [`pretxn_fail`], so
/// it is DURABLE for a device (its replay surface) and SYNTHESIZED for a session (the recording rule).
///
/// # Errors
/// As [`publish`]; plus a git-store fault constructing the forward commit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn revert(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    good: CommitId,
    actor: WriteActor<'_>,
    expected: Generation,
    author: &str,
    message: &str,
    op_id: &OpId,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let command = device_op_command(DeviceOp::Revert);

    // good's tree digest (recorded on its provenance row; render needs a KNOWN digest, so it cannot
    // discover this) + its purge tombstone, SCOPED TO THIS SKILL — reverting to another skill's commit is
    // refused here, so the forward commit can never graft a foreign tree under this skill.
    // Absent/legacy/foreign ⇒ refused.
    let Some((good_digest, good_purged_at)) = authority
        .db()
        .skill_commit_digest_and_purge(ws, skill, good)
        .await?
    else {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(good),
            None,
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("revert target has no recorded bundle digest"),
            created_at,
        )
        .await;
    };

    // Idempotent replay BEFORE rebuilding the forward commit. The forward commit re-parents on the LIVE
    // `current`, so after the first revert commits a retry would derive a DIFFERENT commit id, and the
    // in-transaction replay (which compares the commit) would burn the op as OP_ID_REUSED rather than
    // replaying the original OK. Keying on the stable (command, skill, target digest, expected) replays it —
    // per lane: the device slot by device key id, the session slot by acting email + `request_sha256`.
    let replayed = match &actor {
        WriteActor::Device { device_key_id, .. } => {
            authority
                .db()
                .replay_revert(
                    ws,
                    device_key_id,
                    op_id.as_str(),
                    skill,
                    good_digest,
                    expected,
                )
                .await?
        }
        WriteActor::Session {
            acting,
            request_sha256,
        } => {
            authority
                .db()
                .replay_revert_session(
                    ws,
                    acting.as_str(),
                    op_id.as_str(),
                    skill,
                    good_digest,
                    expected,
                    *request_sha256,
                )
                .await?
        }
    };
    if let Some(replayed) = replayed {
        return Ok(replayed);
    }

    // THE PURGE GATE — after the replay probe (a revert that succeeded BEFORE the purge still owes
    // its byte-identical replay), before any staging: a purged target's bytes are gone by decision
    // (the hash stays as a who/when tombstone), so forward-promoting its tree would strand `current`
    // over deliberately-reclaimed content. A typed refusal, on BOTH lanes (the session revert runs
    // this same path).
    if good_purged_at.is_some() {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(good),
            Some(good_digest),
            expected,
            TerminalOutcome::Denied,
            detail_code_msg("TARGET_PURGED", "the target version's bytes were purged"),
            created_at,
        )
        .await;
    }

    // The current pointer is the forward commit's first parent.
    let Some(current) = authority.db().read_current_commit(ws, skill).await? else {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            None,
            None,
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("cannot revert a skill with no current pointer"),
            created_at,
        )
        .await;
    };

    // good's object set (from its reachability edges — no git_oid reverse-map) + its tree structure
    // (path, mode, git_oid) for the durable commit. Both come from already-recorded, present state.
    let object_ids = authority.db().commit_object_ids(ws, good).await?;

    // A revert may target ONLY an ACCEPTED version — one rooted on `commit_object`. `propose` records a
    // `skill_commit` provenance row (+ digest) for its candidate but roots its bytes via `proposal_object`,
    // NEVER `commit_object` — so an empty trunk object set means `good` is a proposal (or otherwise
    // un-accepted) commit. Forward-promoting its tree would smuggle un-reviewed bytes past the review gate +
    // four-eyes (revert bypasses both) and strand `current` over immediately-GC-reclaimable bytes (the forward
    // commit roots nothing). Refuse it before constructing anything. (Every real bundle carries >= 1 file —
    // the no-empty-bundle policy — so an empty `commit_object` set is an exact "not accepted" test.)
    if object_ids.is_empty() {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(good),
            Some(good_digest),
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("revert target is not an accepted version"),
            created_at,
        )
        .await;
    }

    // Scope the (non-`Send`) gix `Store` to this synchronous block so it drops before the
    // `stage_forward_commit` await below — keeping the revert future `Send` (axum requires it).
    let entries: Vec<(String, topos_gitstore::FileMode, [u8; 20])> = {
        let store = authority.store_for_write(ws)?;
        let leaves = store
            .read_tree_structure(good.0)
            .map_err(AuthorityError::integrity)?;
        leaves
            .into_iter()
            .map(|l| (l.path, l.mode, l.git_oid))
            .collect()
    };

    // The forward commit: same tree (digest) as good, parented on current. Re-derive its id through the
    // kernel; the store refuses a lying id.
    let parents = [current];
    let version_id = identity::commit_id(&Commit {
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
            // A revert restores prior bytes; it never renames or re-places the skill.
            display_name: None,
            channel: None,
        },
        actor,
        DeviceOp::Revert,
        expected,
        created_at,
        now,
    )
    .await
}

/// The server-trusted inputs to the reject transaction (built here, consumed in [`crate::db`]). The
/// identity fields are server-derived values (the proposal's commit + its recorded digest + the request
/// scope), never a client claim.
pub(crate) struct RejectInput<'a> {
    pub ws: &'a WorkspaceId,
    pub skill: &'a SkillId,
    pub commit: CommitId,
    pub bundle_digest: [u8; 32],
    /// Which lane the request arrived on — `reject_run` branches on it ONLY at its authz step.
    pub actor: WriteActor<'a>,
    pub op: DeviceOp,
    pub op_id: &'a str,
    pub expected: Generation,
    /// The MANDATORY rejection reason (`Some(trimmed non-empty)` on BOTH lanes now — the entry
    /// points refuse an empty one typed). A WITHDRAW carries `None`: its stored resolution is the
    /// fixed `withdrawn`, and no verdict notice is written.
    pub reason: Option<&'a str>,
    pub created_at: &'a str,
}

/// Drive a `publish --propose` for an already-staged-and-migrated candidate: open a proposal WITHOUT moving
/// `current`. Same shape as [`publish`] (re-verify renderability before the transaction, then run the one
/// write), but the device op is `PublishPropose`, so the shared `run`'s propose arm fires — recording the
/// proposal + its gated object roots and returning NEEDS_REVIEW. `current` is untouched; no pointer moves.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn propose(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    staged: &StagedCandidate,
    device: &DeviceOpRequest,
    display_name: Option<&str>,
    channel: Option<&str>,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    debug_assert!(
        matches!(device.op, DeviceOp::PublishPropose),
        "set_current::propose requires a PublishPropose device op"
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
            display_name,
            channel,
        },
        WriteActor::Device {
            credential_sha256: device.credential_sha256,
            device_key_id: &device.device_key_id,
        },
        device.op,
        device.expected,
        created_at,
        now,
    )
    .await
}

/// Drive a `review --approve`: promote an existing OPEN proposal to `current` (the sideways move). Nothing is
/// uploaded, leased, or migrated — the candidate is already in the main store, rooted by its proposal. This
/// resolves the proposal's server-trusted promote inputs (its recorded digest, base commit, and rooted object
/// set), re-verifies the candidate is renderable BEFORE the transaction (a stale-or-reclaimed proposal is
/// classified as a clean CONFLICT, never a corruption alarm), then runs the one write through `run`'s approve
/// arm. `expected` (the device's `expected_es`) is the proposal's base generation.
///
/// # Errors
/// Propagates a genuine store/integrity fault on a definitively non-stale proposal whose bytes are lost.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_approve(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    actor: WriteActor<'_>,
    expected: Generation,
    op_id: &OpId,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let base = expected;
    let command = device_op_command(DeviceOp::ReviewApprove);

    // The proposal commit's recorded digest (server-trusted, written at propose) — needed to bound the
    // receipt and render. Absent ⇒ this skill has no such commit ⇒ a typed permanent failure (there is
    // nothing to approve).
    let Some(digest) = authority
        .db()
        .skill_commit_bundle_digest(ws, skill, commit)
        .await?
    else {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            None,
            base,
            TerminalOutcome::PermanentFailure,
            detail_msg("no such proposal commit for this skill"),
            created_at,
        )
        .await;
    };

    // The proposal's IMMUTABLE promote inputs (its base commit + the rooted object set). Absent ⇒ no proposal
    // of this candidate+base ever existed ⇒ a typed permanent failure.
    let Some(inputs) = authority
        .db()
        .proposal_approve_inputs(ws, skill, commit, base)
        .await?
    else {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            Some(digest),
            base,
            TerminalOutcome::PermanentFailure,
            detail_msg("no proposal for this candidate and base"),
            created_at,
        )
        .await;
    };

    if !op_id_is_canonical(op_id.as_str()) {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            Some(digest),
            base,
            TerminalOutcome::PermanentFailure,
            detail_msg("op_id is not a canonical UUID"),
            created_at,
        )
        .await;
    }

    // Re-verify the proposal's bytes are renderable BEFORE the transaction — availability (Step E) is DB-only
    // (`status = 'present'`), so a crash that lost a present row's bytes would otherwise promote a missing
    // version. On a fault, propagate it as genuine corruption ONLY if the proposal is still LIVE + APPROVABLE
    // — `open` AND `current.es` still == base — because only then did the gate guarantee its bytes present, so
    // their loss is a real fault. Otherwise the proposal is stale (current moved ⇒ the transaction's CAS
    // returns CONFLICT) OR no longer open (rejected/accepted ⇒ its unique bytes were LEGITIMATELY GC-reclaimed,
    // and the transaction returns DENIED/CONFLICT): fall through and let the transaction produce that typed,
    // receipted outcome, never a misleading Integrity 500 for a normal approve-after-reject or stale approve.
    if let Err(e) = crate::read::render_version(authority, ws, commit.0, digest).await {
        let live = authority.db().read_current_generation(ws, skill).await? == Some(base);
        let open = authority
            .db()
            .open_proposal_exists(ws, skill, commit, base)
            .await?;
        if live && open {
            return Err(e);
        }
    }

    let input = PromoteInput {
        ws,
        skill,
        op_id: op_id.as_str(),
        actor,
        op: DeviceOp::ReviewApprove,
        expected: base,
        candidate_commit: commit,
        candidate_bundle_digest: digest,
        parents: std::slice::from_ref(&inputs.base_commit),
        object_ids: &inputs.object_ids,
        // An approve promotes already-reviewed bytes; it carries no rename and no placement.
        display_name: None,
        channel: None,
        created_at,
        now,
    };
    authority.db().set_current_txn(input).await
}

/// Drive a `review --reject` / proposer-withdraw: flip an open proposal to `rejected` — moving no
/// pointer. Resolves the proposal commit's recorded digest (the bound receipt identity), then runs the
/// standalone reject transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_reject(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    actor: WriteActor<'_>,
    expected: Generation,
    reason: Option<&str>,
    op_id: &OpId,
    created_at: &str,
) -> Result<SetCurrentReceipt> {
    let command = device_op_command(DeviceOp::ReviewReject);
    let Some(digest) = authority
        .db()
        .skill_commit_bundle_digest(ws, skill, commit)
        .await?
    else {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            None,
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("no such proposal commit for this skill"),
            created_at,
        )
        .await;
    };
    if !op_id_is_canonical(op_id.as_str()) {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            Some(digest),
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("op_id is not a canonical UUID"),
            created_at,
        )
        .await;
    }
    authority
        .db()
        .review_reject_txn(RejectInput {
            ws,
            skill,
            commit,
            bundle_digest: digest,
            actor,
            op: DeviceOp::ReviewReject,
            op_id: op_id.as_str(),
            expected,
            reason,
            created_at,
        })
        .await
}

/// Drive a `review --withdraw`: the AUTHOR retracting their own open proposal — a status flip to
/// `closed` (`resolved_reason = 'withdrawn'`), moving no pointer, writing no verdict notice (the
/// author did it). Same pre-transaction shape as [`review_reject`]; the in-transaction gate is
/// actor == proposer (a non-author's attempt is a typed durable denial, mirroring four-eyes).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_withdraw(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    actor: WriteActor<'_>,
    expected: Generation,
    op_id: &OpId,
    created_at: &str,
) -> Result<SetCurrentReceipt> {
    let command = device_op_command(DeviceOp::ReviewWithdraw);
    let Some(digest) = authority
        .db()
        .skill_commit_bundle_digest(ws, skill, commit)
        .await?
    else {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            None,
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("no such proposal commit for this skill"),
            created_at,
        )
        .await;
    };
    if !op_id_is_canonical(op_id.as_str()) {
        return pretxn_fail(
            authority,
            &actor,
            ws,
            op_id.as_str(),
            command,
            skill,
            Some(commit),
            Some(digest),
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("op_id is not a canonical UUID"),
            created_at,
        )
        .await;
    }
    authority
        .db()
        .review_reject_txn(RejectInput {
            ws,
            skill,
            commit,
            bundle_digest: digest,
            actor,
            op: DeviceOp::ReviewWithdraw,
            op_id: op_id.as_str(),
            expected,
            reason: None,
            created_at,
        })
        .await
}

/// Write a pre-transaction terminal outcome through the actor's lane: DURABLE for a device (the replay
/// surface for those outcomes), SYNTHESIZED — same fields, never persisted — for a session. A
/// web-verified email proves nothing about membership in the target workspace, so a session pre-txn
/// failure must not grow `op_receipts` (the session-lane recording rule); a deterministic re-run is owed
/// instead of a byte-stable replay.
#[allow(clippy::too_many_arguments)]
async fn pretxn_fail(
    authority: &Authority,
    actor: &WriteActor<'_>,
    ws: &WorkspaceId,
    op_id: &str,
    command: &str,
    skill: &SkillId,
    commit: Option<CommitId>,
    bundle_digest: Option<[u8; 32]>,
    expected: Generation,
    outcome: TerminalOutcome,
    details: Option<serde_json::Value>,
    created_at: &str,
) -> Result<SetCurrentReceipt> {
    match actor {
        WriteActor::Device { device_key_id, .. } => {
            authority
                .db()
                .record_pretxn(pretxn(
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
                ))
                .await
        }
        WriteActor::Session { .. } => Ok(SetCurrentReceipt {
            op_id: op_id.to_owned(),
            command: command.to_owned(),
            skill_id: skill.as_str().to_owned(),
            version_id: commit,
            bundle_digest,
            expected,
            outcome,
            current: None,
            record: None,
            created_at: created_at.to_owned(),
            details,
        }),
    }
}

/// Reject an op routed to the wrong entry point (e.g. a `propose` whose device op is not `PublishPropose`) — a
/// permanent, receipted failure that uploaded / migrated / opened NOTHING (a guard before any state change),
/// closing the same review-gate-bypass class [`reject_non_publish_op`] closes for direct publish.
pub(crate) async fn reject_op_mismatch(
    authority: &Authority,
    ws: &WorkspaceId,
    skill: &SkillId,
    op_id: &OpId,
    device: &DeviceOpRequest,
    created_at: &str,
    msg: &str,
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
            detail_msg(msg),
            created_at,
        ))
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
    /// The skill's advisory display name (`None` ⇒ keep any existing name).
    display_name: Option<&'a str>,
    /// The `--to` channel placement (`None` ⇒ no explicit placement).
    channel: Option<&'a str>,
}

/// The shared driver: bridge the op id, render-verify the migrated candidate (a pre-transaction filesystem
/// read — never inside the pure-DB write), then run the one serializable write.
async fn drive(
    authority: &Authority,
    cand: Candidate<'_>,
    actor: WriteActor<'_>,
    op: DeviceOp,
    expected: Generation,
    created_at: &str,
    now: i64,
) -> Result<SetCurrentReceipt> {
    let command = device_op_command(op);

    // The op_id must be a CANONICAL lowercase-hyphenated UUID (and, on the session lane, is the request
    // id the composing route already proved canonical) — the receipt slot's key must stay 1:1 with one
    // spelling, or a varied-form retry would miss its receipt and re-execute. A non-canonical op id is a
    // permanent failure, DURABLE for a device (its replay surface) and SYNTHESIZED for a session (the
    // recording rule), routed through `pretxn_fail`.
    if !op_id_is_canonical(cand.op_id.as_str()) {
        return pretxn_fail(
            authority,
            &actor,
            cand.ws,
            cand.op_id.as_str(),
            command,
            cand.skill,
            Some(cand.commit),
            Some(cand.bundle_digest),
            expected,
            TerminalOutcome::PermanentFailure,
            detail_msg("op_id is not a canonical UUID"),
            created_at,
        )
        .await;
    }

    // Re-verify the migrated candidate is renderable BEFORE the transaction (the migrate path defers this
    // renderability re-check to the pointer-move). A failure is a genuine fault — a `present` row whose bytes
    // a crash lost, a corrupt blob, or a database error — so PROPAGATE it (Integrity/Internal): it rolls the
    // attempt back and surfaces a corruption/DB alarm, never a *receipted* `RETRYABLE_FAILURE` (which a retry
    // would replay forever as a sticky terminal even after the underlying fault cleared).
    crate::read::render_version(authority, cand.ws, cand.commit.0, cand.bundle_digest).await?;

    let input = PromoteInput {
        ws: cand.ws,
        skill: cand.skill,
        op_id: cand.op_id.as_str(),
        actor,
        op,
        expected,
        candidate_commit: cand.commit,
        candidate_bundle_digest: cand.bundle_digest,
        parents: cand.parents,
        object_ids: cand.object_ids,
        display_name: cand.display_name,
        channel: cand.channel,
        created_at,
        now,
    };
    authority.db().set_current_txn(input).await
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
        DeviceOp::ReviewWithdraw => "review-withdraw",
    }
}

/// A small JSON detail object carrying a human-readable message (the open `details` field).
pub(crate) fn detail_msg(msg: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "message": msg }))
}

/// A detail object with a machine-branchable `code` beside the message (the session lane's
/// `code_details` shape, on the custody path).
pub(crate) fn detail_code_msg(code: &str, msg: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "code": code, "message": msg }))
}

/// Whether an op-id string is **exactly** the canonical lowercase-hyphenated UUID form — `parse_str`
/// also accepts the simple (32-hex) and braced spellings, which decode to the SAME 16 bytes, so without
/// this check two distinct receipt-key strings could name one operation and split the idempotency slot
/// (a varied-form retry would miss its receipt and re-execute). Requiring the canonical form keeps the
/// receipt key 1:1 with the operation, so they can never split.
fn op_id_is_canonical(op_id: &str) -> bool {
    uuid::Uuid::parse_str(op_id).is_ok_and(|uuid| uuid.as_hyphenated().to_string() == op_id)
}

/// The revert commit frame was rejected by the kernel (an internal fault — the inputs are server-derived).
#[derive(Debug, thiserror::Error)]
#[error("could not build the revert commit frame")]
struct RevertFrame;
