//! The durable all-outcome receipt machinery — the raw-SQL half shared by the pointer-move and the
//! standalone reject transactions.
//!
//! Split from [`super::set_current`] so [`run`](super::set_current)'s ordered arms stay that file's single
//! story: this file holds the receipt read/insert + the bound-identity replay (a same-`op_id` retry returns
//! the stored receipt byte-identically; a mismatched identity is a permanent key-reuse), the
//! terminal-outcome writers (each persists its receipt in-txn and releases the candidate's promotion lease;
//! the pre-authentication DENIEDs are synthesized, never persisted), the pre-transaction receipt write, and
//! the outcome codecs. Plain free fns over the open `Transaction` — no ordering decision lives here; the
//! callers (`run` / `reject_run`) own the load-bearing step order.

use sqlx::{Postgres, Transaction};
use topos_core::sign::DeviceOp;
use topos_types::{Generation, TerminalOutcome};

use super::Db;
use super::blob32;
use super::set_current::{CurrentRow, delete_lease, i64_to_u64, u64_to_i64};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, SkillId, WorkspaceId};
use crate::set_current::{PretxnReceipt, PromoteInput, RejectInput, SetCurrentReceipt};

impl Db {
    /// Record a **pre-transaction** terminal outcome idempotently (a render-verify/op-id/preflight failure):
    /// a same-op_id retry with the matching bound identity returns the stored receipt; a mismatch is a
    /// permanent key-reuse (never overwrites the slot). One own transaction (it runs outside the main write).
    pub(crate) async fn record_pretxn(&self, r: PretxnReceipt<'_>) -> Result<SetCurrentReceipt> {
        run_serializable!(self, tx, record_pretxn_body(&mut tx, &r).await)
    }

    /// Idempotent replay for a **revert**, keyed on the op id and compared on the STABLE request identity
    /// (command + skill + target tree digest + expected generation) — **not** the server-derived forward
    /// commit id, which re-parents on the live `current` and so changes after the first revert commits (the
    /// in-transaction replay, which does compare the commit, would then spuriously see a mismatch and burn
    /// the op as `OP_ID_REUSED` instead of replaying the original `OK`). Run this BEFORE rebuilding the
    /// forward commit: `Some(receipt)` replays a prior result (a true retry — or a permanent `OP_ID_REUSED`
    /// if the same op id was reused for a different target/generation); `None` means proceed (a fresh op).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn replay_revert(
        &self,
        ws: &WorkspaceId,
        device_key_id: &str,
        op_id: &str,
        skill: &SkillId,
        good_digest: [u8; 32],
        expected: Generation,
    ) -> Result<Option<SetCurrentReceipt>> {
        let Some(stored) = get_receipt(self.pool(), ws, device_key_id, op_id).await? else {
            return Ok(None);
        };
        let stable_match = stored.command == "revert"
            && stored.skill_id == skill.as_str()
            && stored.bundle_digest == Some(good_digest)
            && stored.expected == expected;
        Ok(Some(if stable_match {
            stored.into_receipt()
        } else {
            // The reuse receipt's bound identity is the revert's STABLE request key (no forward commit id,
            // which re-parents on the live `current`); `permanent_key_reuse` carries no version regardless.
            // Use the ORIGINAL receipt's `created_at` (not the incoming request's) so the reuse receipt is
            // byte-stable across retries.
            let bound = BoundIdentity {
                command: "revert",
                skill_id: skill.as_str(),
                commit: None,
                bundle_digest: Some(good_digest),
                expected,
            };
            permanent_key_reuse(op_id, &bound, &stored.created_at)
        }))
    }

    /// Fast-path replay of a prior **gated** (`APPROVAL_REQUIRED`) direct-publish outcome, keyed on the
    /// STABLE (command, skill, expected) — so a retry of a gated op stays sticky across a `review_required`
    /// flip without re-gating (which would mismatch a `commit = None` gate receipt and burn `OP_ID_REUSED`)
    /// or re-ingesting (a gated op's lease is released, so a re-ingest hits `LeaseNotLive`). Only the gated
    /// outcome is fast-pathed; any other returns `None`, and the preflight skip-gates a stored **OK** (whose
    /// completed lease makes a re-ingest safe) via [`published_ok_exists`](Self::published_ok_exists), leaving
    /// the commit comparison to the in-txn replay. Mirrors [`replay_revert`](Self::replay_revert).
    pub(crate) async fn replay_gated_publish(
        &self,
        ws: &WorkspaceId,
        device_key_id: &str,
        op_id: &str,
        skill: &SkillId,
        expected: Generation,
    ) -> Result<Option<SetCurrentReceipt>> {
        let Some(stored) = get_receipt(self.pool(), ws, device_key_id, op_id).await? else {
            return Ok(None);
        };
        // A gated outcome MUST be replayed on this pre-ingest fast path rather than left to the promote: every
        // non-OK outcome RELEASES its migrate lease, so re-ingesting a gated op would hit `LeaseNotLive`. Both
        // gate paths can produce it — the PREFLIGHT (pre-ingest, `commit = None`) and the rare IN-TXN gate
        // (post-ingest, `commit = Some`, when `review_required` flips on mid-publish). The unbound one NEEDS
        // this fast path (the promote's commit-comparison replay can't match a `None` commit). The bound one
        // is replayed here too, deliberately: a gated op published NO bytes, so replaying `APPROVAL_REQUIRED`
        // for a same-op_id retry of different bytes — instead of `OP_ID_REUSED` — denies the publish either
        // way (the client re-runs as a proposal) and avoids re-migrating its released lease. Any OTHER stored
        // outcome returns `None`; the preflight then skip-gates a stored OK (whose COMPLETED lease makes a
        // re-ingest safe) via `published_ok_exists`.
        if stored.outcome != TerminalOutcome::ApprovalRequired {
            return Ok(None);
        }
        let stable_match = stored.command == "publish-direct"
            && stored.skill_id == skill.as_str()
            && stored.expected == expected;
        Ok(Some(if stable_match {
            stored.into_receipt()
        } else {
            // The op id was reused for a DIFFERENT direct publish (skill/expected differ): the byte-stable
            // reuse receipt, bound on the gated request's stable key (no commit/digest — nothing ingested).
            let bound = BoundIdentity {
                command: "publish-direct",
                skill_id: skill.as_str(),
                commit: None,
                bundle_digest: None,
                expected,
            };
            permanent_key_reuse(op_id, &bound, &stored.created_at)
        }))
    }

    /// Whether a prior **OK** pointer-move receipt exists for this `(workspace, device, op id)`. The publish
    /// preflight consults it to avoid RE-GATING a publish that already SUCCEEDED: re-gating would bind a fresh
    /// `commit = None` receipt and mismatch the stored OK (burning the op as `OP_ID_REUSED` once
    /// `review_required` flips ON between attempts). A stored OK instead skips the gate so the promote path's
    /// in-txn replay (which runs BEFORE the in-txn gate) returns the original OK — safe because an OK leaves a
    /// **completed** (non-expiring) lease, so the re-ingest succeeds. (Non-OK outcomes release their lease, so
    /// they are NOT skip-gated; a gated one is replayed by [`replay_gated_publish`](Self::replay_gated_publish),
    /// and a CONFLICT/DENIED retry keeps its prior behaviour — re-gated to `OP_ID_REUSED` under review-on, or
    /// the unchanged review-off flow.)
    pub(crate) async fn published_ok_exists(
        &self,
        ws: &WorkspaceId,
        device_key_id: &str,
        op_id: &str,
    ) -> Result<bool> {
        Ok(matches!(
            get_receipt(self.pool(), ws, device_key_id, op_id).await?,
            Some(stored) if stored.outcome == TerminalOutcome::Ok
        ))
    }
}

/// The body of [`Db::record_pretxn`], factored out so the pointer-move runner can re-run it on a
/// serialization retry: it borrows `r` (never consumes it) and touches only the transaction, so a retry
/// is byte-identical. Records a pre-transaction terminal outcome idempotently (replay-hit returns the
/// stored receipt; a bound-identity mismatch is a permanent key-reuse; a fresh op inserts the receipt).
async fn record_pretxn_body(
    tx: &mut Transaction<'_, Postgres>,
    r: &PretxnReceipt<'_>,
) -> Result<SetCurrentReceipt> {
    let bound = BoundIdentity {
        command: r.command,
        skill_id: r.skill.as_str(),
        commit: r.commit,
        bundle_digest: r.bundle_digest,
        expected: r.expected,
    };
    let outcome = match replay(tx, r.ws, r.device_key_id, r.op_id, &bound).await? {
        Replay::Hit(receipt) => receipt,
        Replay::Mismatch(original_at) => permanent_key_reuse(r.op_id, &bound, &original_at),
        Replay::Fresh => {
            let stored = StoredReceipt {
                op_id: r.op_id.to_owned(),
                command: r.command.to_owned(),
                skill_id: r.skill.as_str().to_owned(),
                commit: r.commit,
                bundle_digest: r.bundle_digest,
                expected: r.expected,
                outcome: r.outcome,
                current: None,
                signed_record: None,
                key_id: None,
                created_at: r.created_at.to_owned(),
                details: r.details.clone(),
            };
            insert_receipt(tx, r.ws, r.device_key_id, &stored).await?;
            stored.into_receipt()
        }
    };
    Ok(outcome)
}

// --- terminal-outcome receipt writers (each persists the receipt in-txn and returns the projection) ---

pub(super) async fn denied(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    msg: &str,
) -> Result<SetCurrentReceipt> {
    write_terminal(
        tx,
        input,
        bound,
        TerminalOutcome::Denied,
        None,
        None,
        None,
        detail(msg),
    )
    .await
}

/// A **pre-authentication** DENIED (unknown/revoked device, invalid signature): synthesize the receipt
/// WITHOUT persisting it — mirroring the governance preamble's posture (`db/enroll`): the failure is not
/// attributable to any verified actor, so a durable row keyed on the attacker-chosen `(device_key_id,
/// op_id)` would let an UNAUTHENTICATED client mint audit noise and grow `op_receipts` without bound.
/// Determinism makes a retry reproduce the same outcome (exactly like [`permanent_key_reuse`], which is
/// likewise never stored); only `created_at` re-stamps, and an unauthenticated caller is owed no
/// byte-stable replay. This ALSO means a later, correctly-signed retry of the same op id proceeds fresh
/// instead of replaying a burned DENIED. The candidate's promotion lease IS still released (this write
/// removes a row the caller's own ingest created — it grows nothing): an unauthenticated publish/propose
/// already migrated its bytes, and skipping the release would leave them GC-rooted forever. (The pre-txn
/// typed failures written by `record_pretxn` — op-mismatch, non-UUID op ids, the review gate — stay
/// durable: they are the replay surface for those outcomes.)
pub(super) async fn denied_preauth(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    msg: &str,
) -> Result<SetCurrentReceipt> {
    if !matches!(input.op, DeviceOp::ReviewApprove) {
        delete_lease(tx, input.ws, input.op_id).await?;
    }
    Ok(SetCurrentReceipt {
        op_id: input.op_id.to_owned(),
        command: bound.command.to_owned(),
        skill_id: bound.skill_id.to_owned(),
        version_id: bound.commit,
        bundle_digest: bound.bundle_digest,
        expected: bound.expected,
        outcome: TerminalOutcome::Denied,
        current: None,
        signed_record: None,
        key_id: None,
        created_at: input.created_at.to_owned(),
        details: detail(msg),
    })
}

pub(super) async fn conflict(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    live: Generation,
) -> Result<SetCurrentReceipt> {
    write_terminal(
        tx,
        input,
        bound,
        TerminalOutcome::Conflict,
        Some(live),
        None,
        None,
        None,
    )
    .await
}

pub(super) async fn first_parent_mismatch(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    cur: &CurrentRow,
) -> Result<SetCurrentReceipt> {
    let details = Some(serde_json::json!({
        "code": "FIRST_PARENT_MISMATCH",
        "current_commit_id": topos_core::digest::to_hex(&cur.commit.0),
    }));
    write_terminal(
        tx,
        input,
        bound,
        TerminalOutcome::Denied,
        Some(cur.generation),
        None,
        None,
        details,
    )
    .await
}

pub(super) async fn approval_required(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
) -> Result<SetCurrentReceipt> {
    write_terminal(
        tx,
        input,
        bound,
        TerminalOutcome::ApprovalRequired,
        None,
        None,
        None,
        detail("direct publish under review-required; re-run as a proposal"),
    )
    .await
}

pub(super) async fn retryable(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    msg: &str,
) -> Result<SetCurrentReceipt> {
    write_terminal(
        tx,
        input,
        bound,
        TerminalOutcome::RetryableFailure,
        None,
        None,
        None,
        detail(msg),
    )
    .await
}

pub(super) async fn permanent(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    msg: &str,
) -> Result<SetCurrentReceipt> {
    write_terminal(
        tx,
        input,
        bound,
        TerminalOutcome::PermanentFailure,
        None,
        None,
        None,
        detail(msg),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn write_terminal(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    outcome: TerminalOutcome,
    current: Option<Generation>,
    signed_record: Option<Vec<u8>>,
    key_id: Option<String>,
    details: Option<serde_json::Value>,
) -> Result<SetCurrentReceipt> {
    let stored = StoredReceipt {
        op_id: input.op_id.to_owned(),
        command: bound.command.to_owned(),
        skill_id: bound.skill_id.to_owned(),
        commit: bound.commit,
        bundle_digest: bound.bundle_digest,
        expected: bound.expected,
        outcome,
        current,
        signed_record,
        key_id,
        created_at: input.created_at.to_owned(),
        details,
    };
    insert_receipt(tx, input.ws, input.device_key_id, &stored).await?;
    // Release the candidate's promotion lease: a terminal non-OK abandons this candidate, and its lease was
    // made non-expiring by a successful migrate, so without this its objects would be GC-rooted forever. The
    // delete is idempotent (a no-op if the lease is absent or only expiring). A retry of the SAME op_id
    // replays this receipt before reaching here; a rebase is a new op_id with its own fresh lease. A
    // `review --approve` leased NOTHING (its candidate was migrated at propose), so it has no lease to release
    // — skip the delete for it (it would be a harmless no-op, but staying op-aware keeps the contract clear).
    if !matches!(input.op, DeviceOp::ReviewApprove) {
        delete_lease(tx, input.ws, input.op_id).await?;
    }
    Ok(stored.into_receipt())
}

fn detail(msg: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "message": msg }))
}

/// A same-`op_id` retry whose bound identity differs from the recorded op — a permanent key-reuse. NOT
/// receipted (the slot belongs to the original op); determinism makes re-running it return this same value.
/// The identity fields (command / skill_id / expected) come from the **incoming** request's `bound`; the
/// candidate fields are `None` because a rejected reuse ingests no version (the original op's bytes are not
/// this op's to claim); and `created_at` is the **original** receipt's timestamp (the caller passes
/// `stored.created_at`), so re-running the same reuse returns the byte-identical value rather than a fresh
/// clock read.
pub(super) fn permanent_key_reuse(
    op_id: &str,
    bound: &BoundIdentity<'_>,
    created_at: &str,
) -> SetCurrentReceipt {
    SetCurrentReceipt {
        op_id: op_id.to_owned(),
        command: bound.command.to_owned(),
        skill_id: bound.skill_id.to_owned(),
        version_id: None,
        bundle_digest: None,
        expected: bound.expected,
        outcome: TerminalOutcome::PermanentFailure,
        current: None,
        signed_record: None,
        key_id: None,
        created_at: created_at.to_owned(),
        details: Some(serde_json::json!({ "code": "OP_ID_REUSED" })),
    }
}

// --- the reject transaction's receipt writers (no pointer data, no lease — reject moves nothing) ---

/// Write a reject terminal receipt (no pointer data, no lease — reject moves nothing). `code` distinguishes a
/// fresh rejection from an idempotent re-reject in the receipt `details`, so a caller branches on it rather
/// than on the `Ok` outcome (which a reject shares with a promotion).
pub(super) async fn reject_terminal(
    tx: &mut Transaction<'_, Postgres>,
    r: &RejectInput<'_>,
    outcome: TerminalOutcome,
    code: &str,
) -> Result<SetCurrentReceipt> {
    let stored = StoredReceipt {
        op_id: r.op_id.to_owned(),
        command: crate::set_current::device_op_command(r.op).to_owned(),
        skill_id: r.skill.as_str().to_owned(),
        commit: Some(r.commit),
        bundle_digest: Some(r.bundle_digest),
        expected: r.expected,
        outcome,
        current: None,
        signed_record: None,
        key_id: None,
        created_at: r.created_at.to_owned(),
        details: Some(serde_json::json!({ "code": code })),
    };
    insert_receipt(tx, r.ws, r.device_key_id, &stored).await?;
    Ok(stored.into_receipt())
}

/// The reject transaction's **pre-authentication** DENIED — synthesized, never persisted (the same
/// rationale as [`denied_preauth`]; a reject holds no lease, so there is nothing to release either).
pub(super) fn reject_denied_preauth(r: &RejectInput<'_>, msg: &str) -> SetCurrentReceipt {
    SetCurrentReceipt {
        op_id: r.op_id.to_owned(),
        command: crate::set_current::device_op_command(r.op).to_owned(),
        skill_id: r.skill.as_str().to_owned(),
        version_id: Some(r.commit),
        bundle_digest: Some(r.bundle_digest),
        expected: r.expected,
        outcome: TerminalOutcome::Denied,
        current: None,
        signed_record: None,
        key_id: None,
        created_at: r.created_at.to_owned(),
        details: detail(msg),
    }
}

pub(super) async fn reject_denied(
    tx: &mut Transaction<'_, Postgres>,
    r: &RejectInput<'_>,
    msg: &str,
) -> Result<SetCurrentReceipt> {
    let stored = StoredReceipt {
        op_id: r.op_id.to_owned(),
        command: crate::set_current::device_op_command(r.op).to_owned(),
        skill_id: r.skill.as_str().to_owned(),
        commit: Some(r.commit),
        bundle_digest: Some(r.bundle_digest),
        expected: r.expected,
        outcome: TerminalOutcome::Denied,
        current: None,
        signed_record: None,
        key_id: None,
        created_at: r.created_at.to_owned(),
        details: detail(msg),
    };
    insert_receipt(tx, r.ws, r.device_key_id, &stored).await?;
    Ok(stored.into_receipt())
}

// --- the bound identity + replay ---

/// The fields a same-`op_id` retry must match to replay (the value of the receipt, never its key).
pub(super) struct BoundIdentity<'a> {
    pub(super) command: &'a str,
    pub(super) skill_id: &'a str,
    pub(super) commit: Option<CommitId>,
    pub(super) bundle_digest: Option<[u8; 32]>,
    pub(super) expected: Generation,
}

// A transient control-flow enum: built and immediately destructured in `replay`, never stored or collected,
// so the size gap between the (intentionally rich) `Hit` receipt and the unit arms is irrelevant — boxing it
// would add a heap alloc on the replay path for no benefit.
#[allow(clippy::large_enum_variant)]
pub(super) enum Replay {
    Hit(SetCurrentReceipt),
    /// The slot exists but the bound identity differs (a key reuse). Carries the ORIGINAL receipt's
    /// `created_at` so the synthesized reuse receipt is byte-stable across retries (it is itself never stored).
    Mismatch(String),
    Fresh,
}

pub(super) async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    device_key_id: &str,
    op_id: &str,
    bound: &BoundIdentity<'_>,
) -> Result<Replay> {
    match get_receipt(&mut **tx, ws, device_key_id, op_id).await? {
        None => Ok(Replay::Fresh),
        Some(stored) => {
            if stored.command == bound.command
                && stored.skill_id == bound.skill_id
                && stored.commit == bound.commit
                && stored.bundle_digest == bound.bundle_digest
                && stored.expected == bound.expected
            {
                Ok(Replay::Hit(stored.into_receipt()))
            } else {
                Ok(Replay::Mismatch(stored.created_at))
            }
        }
    }
}

// --- the stored receipt row ---

pub(super) struct StoredReceipt {
    pub(super) op_id: String,
    pub(super) command: String,
    pub(super) skill_id: String,
    pub(super) commit: Option<CommitId>,
    pub(super) bundle_digest: Option<[u8; 32]>,
    pub(super) expected: Generation,
    pub(super) outcome: TerminalOutcome,
    pub(super) current: Option<Generation>,
    pub(super) signed_record: Option<Vec<u8>>,
    pub(super) key_id: Option<String>,
    pub(super) created_at: String,
    pub(super) details: Option<serde_json::Value>,
}

impl StoredReceipt {
    pub(super) fn into_receipt(self) -> SetCurrentReceipt {
        SetCurrentReceipt {
            op_id: self.op_id,
            command: self.command,
            skill_id: self.skill_id,
            version_id: self.commit,
            bundle_digest: self.bundle_digest,
            expected: self.expected,
            outcome: self.outcome,
            current: self.current,
            signed_record: self.signed_record,
            key_id: self.key_id,
            created_at: self.created_at,
            details: self.details,
        }
    }
}

// --- the op_receipts row SQL (workspace-scoped; the one durable receipt slot per (device, op id)) ---

async fn get_receipt<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    device_key_id: &str,
    op_id: &str,
) -> Result<Option<StoredReceipt>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT command AS "command!", skill_id AS "skill_id!",
                  commit_id AS "commit_id?: Vec<u8>", bundle_digest AS "bundle_digest?: Vec<u8>",
                  expected_epoch AS "expected_epoch!: i64", expected_seq AS "expected_seq!: i64",
                  outcome AS "outcome!", current_epoch AS "current_epoch?: i64",
                  current_seq AS "current_seq?: i64", signed_record AS "signed_record?: Vec<u8>",
                  key_id AS "key_id?", created_at AS "created_at!", details AS "details?"
           FROM op_receipts WHERE workspace_id = $1 AND device_key_id = $2 AND op_id = $3"#,
        ws_s,
        device_key_id,
        op_id,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    let Some(r) = row else { return Ok(None) };
    let current = match (r.current_epoch, r.current_seq) {
        (Some(e), Some(s)) => Some(Generation {
            epoch: i64_to_u64(e)?,
            seq: i64_to_u64(s)?,
        }),
        _ => None,
    };
    Ok(Some(StoredReceipt {
        op_id: op_id.to_owned(),
        command: r.command,
        skill_id: r.skill_id,
        commit: r.commit_id.map(|b| blob32(&b)).transpose()?.map(CommitId),
        bundle_digest: r.bundle_digest.map(|b| blob32(&b)).transpose()?,
        expected: Generation {
            epoch: i64_to_u64(r.expected_epoch)?,
            seq: i64_to_u64(r.expected_seq)?,
        },
        outcome: parse_outcome(&r.outcome)?,
        current,
        signed_record: r.signed_record,
        key_id: r.key_id,
        created_at: r.created_at,
        // A stored `details` column that no longer parses is store corruption: silently dropping it (an
        // `.ok()`) would REPLAY an altered receipt — breaking the byte-identical-replay invariant without a
        // sound. Alarm instead (mirrors `BadOutcome` on the outcome column).
        details: r
            .details
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| AuthorityError::integrity(BadDetails))?,
    }))
}

pub(super) async fn insert_receipt(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    device_key_id: &str,
    r: &StoredReceipt,
) -> Result<()> {
    let ws_s = ws.as_str();
    let commit = r.commit.map(|c| c.0.to_vec());
    let digest = r.bundle_digest.map(|d| d.to_vec());
    let expected_epoch = u64_to_i64(r.expected.epoch)?;
    let expected_seq = u64_to_i64(r.expected.seq)?;
    let outcome = outcome_str(r.outcome);
    let current_epoch = r.current.map(|g| u64_to_i64(g.epoch)).transpose()?;
    let current_seq = r.current.map(|g| u64_to_i64(g.seq)).transpose()?;
    let details = r.details.as_ref().map(ToString::to_string);
    sqlx::query!(
        "INSERT INTO op_receipts (workspace_id, device_key_id, op_id, command, skill_id, commit_id, \
            bundle_digest, expected_epoch, expected_seq, outcome, current_epoch, current_seq, \
            signed_record, key_id, created_at, details) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        ws_s,
        device_key_id,
        r.op_id,
        r.command,
        r.skill_id,
        commit,
        digest,
        expected_epoch,
        expected_seq,
        outcome,
        current_epoch,
        current_seq,
        r.signed_record,
        r.key_id,
        r.created_at,
        details,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

// --- the outcome codecs (the frozen terminal-outcome strings a stored receipt round-trips) ---

fn outcome_str(o: TerminalOutcome) -> &'static str {
    match o {
        TerminalOutcome::Ok => "OK",
        TerminalOutcome::ApprovalRequired => "APPROVAL_REQUIRED",
        TerminalOutcome::NeedsReview => "NEEDS_REVIEW",
        TerminalOutcome::Conflict => "CONFLICT",
        TerminalOutcome::Diverged => "DIVERGED",
        TerminalOutcome::Denied => "DENIED",
        TerminalOutcome::Unavailable => "UNAVAILABLE",
        TerminalOutcome::AmbiguousName => "AMBIGUOUS_NAME",
        TerminalOutcome::KeyRepinRequired => "KEY_REPIN_REQUIRED",
        TerminalOutcome::RetryableFailure => "RETRYABLE_FAILURE",
        TerminalOutcome::PermanentFailure => "PERMANENT_FAILURE",
    }
}

fn parse_outcome(s: &str) -> Result<TerminalOutcome> {
    Ok(match s {
        "OK" => TerminalOutcome::Ok,
        "APPROVAL_REQUIRED" => TerminalOutcome::ApprovalRequired,
        "NEEDS_REVIEW" => TerminalOutcome::NeedsReview,
        "CONFLICT" => TerminalOutcome::Conflict,
        "DIVERGED" => TerminalOutcome::Diverged,
        "DENIED" => TerminalOutcome::Denied,
        "UNAVAILABLE" => TerminalOutcome::Unavailable,
        "AMBIGUOUS_NAME" => TerminalOutcome::AmbiguousName,
        "KEY_REPIN_REQUIRED" => TerminalOutcome::KeyRepinRequired,
        "RETRYABLE_FAILURE" => TerminalOutcome::RetryableFailure,
        "PERMANENT_FAILURE" => TerminalOutcome::PermanentFailure,
        _ => return Err(AuthorityError::integrity(BadOutcome)),
    })
}

#[derive(Debug, thiserror::Error)]
#[error("a stored receipt outcome is not a known terminal code")]
struct BadOutcome;

#[derive(Debug, thiserror::Error)]
#[error(
    "a stored receipt's details column is not valid JSON — the receipt cannot replay byte-identically"
)]
struct BadDetails;
