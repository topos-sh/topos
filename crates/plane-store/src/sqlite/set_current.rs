//! The one pointer-move transaction — the raw-SQL half of `set-current`.
//!
//! One `BEGIN IMMEDIATE` write transaction advances a skill's `current` pointer by exactly one step, under
//! a compare-and-set on the whole `(epoch, seq)` pair, signs the new pointer, re-roots the migrated bytes,
//! and writes a durable all-outcome receipt — **with no filesystem op inside the transaction**. The
//! ordered sub-steps (and why each ordering is load-bearing) are in [`run`]. All `sqlx` stays here; the
//! caller ([`crate::set_current`]) hands in server-trusted values and a signer and gets back a domain
//! [`SetCurrentReceipt`].

use sqlx::{Sqlite, Transaction};
use topos_core::sign::{CurrentPointer, DeviceOp, DeviceOpFields, verify_device_op};
use topos_types::{
    CurrentRecord, Generation, PointerScope, Signature, SignatureAlg, SignedCurrentRecord,
    TerminalOutcome,
};

use super::Db;
use super::proposals::{
    ProposalStatus, insert_approval, insert_proposal, insert_proposal_object, proposal_id_exists,
    read_open_proposal, resolve_proposal, set_proposal_status,
};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};
use crate::set_current::{PretxnReceipt, PromoteInput, RejectInput, SetCurrentReceipt};
use crate::signer::PlaneSigner;

/// The JCS / I-JSON safe-integer bound (2^53 − 1) the pointer preimage enforces — a generation a follower
/// could never verify is never stored or signed.
const MAX_SAFE_INT: u64 = (1u64 << 53) - 1;

impl Db {
    /// Run the one pointer-move transaction. Commits on a terminal outcome (the receipt — and, for `OK`,
    /// the pointer/provenance — persist together); rolls back only on an internal/integrity fault.
    pub(crate) async fn set_current_txn(
        &self,
        input: PromoteInput<'_>,
        signer: &PlaneSigner,
    ) -> Result<SetCurrentReceipt> {
        let mut tx = self.begin_immediate().await?;
        match run(&mut tx, &input, signer).await {
            Ok(receipt) => {
                tx.commit().await.map_err(AuthorityError::internal)?;
                Ok(receipt)
            }
            Err(e) => {
                tx.rollback().await.map_err(AuthorityError::internal)?;
                Err(e)
            }
        }
    }

    /// The standalone `review --reject` / proposer-withdraw transaction. NOT a pointer move — it never enters
    /// [`run`]: `current` is untouched, nothing is signed, there is no lease. One `BEGIN IMMEDIATE` mirrors the
    /// promotion's discipline where it overlaps — receipt-replay first, then in-transaction authorization (the
    /// SAME device-op frame, `op = ReviewReject`) — then resolves the proposal and classifies it: `open` ⇒
    /// flip to `rejected`; already `rejected` ⇒ idempotent OK (a lost-ack retry under a different op_id); and
    /// `accepted` or absent ⇒ a typed DENIED. One path serves both reviewer-reject and proposer-withdraw;
    /// `resolved_by` records who.
    pub(crate) async fn review_reject_txn(&self, r: RejectInput<'_>) -> Result<SetCurrentReceipt> {
        let mut tx = self.begin_immediate().await?;
        match reject_run(&mut tx, &r).await {
            Ok(receipt) => {
                tx.commit().await.map_err(AuthorityError::internal)?;
                Ok(receipt)
            }
            Err(e) => {
                tx.rollback().await.map_err(AuthorityError::internal)?;
                Err(e)
            }
        }
    }

    /// Record a **pre-transaction** terminal outcome idempotently (a render-verify/op-id/preflight failure):
    /// a same-op_id retry with the matching bound identity returns the stored receipt; a mismatch is a
    /// permanent key-reuse (never overwrites the slot). One own transaction (it runs outside the main write).
    pub(crate) async fn record_pretxn(&self, r: PretxnReceipt<'_>) -> Result<SetCurrentReceipt> {
        let mut tx = self.begin_immediate().await?;
        let bound = BoundIdentity {
            command: r.command,
            skill_id: r.skill.as_str(),
            commit: r.commit,
            bundle_digest: r.bundle_digest,
            expected: r.expected,
        };
        let outcome = match replay(&mut tx, r.ws, r.device_key_id, r.op_id, &bound).await? {
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
                insert_receipt(&mut tx, r.ws, r.device_key_id, &stored).await?;
                stored.into_receipt()
            }
        };
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(outcome)
    }

    /// The recorded bundle digest of a commit's provenance row **scoped to the requesting skill** (revert
    /// reads the target's tree digest here — the git commit does not persist it). The `skill_id` filter is
    /// load-bearing security: it forbids reverting to a commit owned by **another** skill in the same
    /// workspace (which would graft that skill's tree under this skill's `commit_object` edges and leak its
    /// bytes). `None` if the commit is not a version of this skill, or its digest is unrecorded (a legacy
    /// pre-pointer-move version) — either way it cannot be a revert target.
    pub(crate) async fn skill_commit_bundle_digest(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
    ) -> Result<Option<[u8; 32]>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let cid = commit.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT bundle_digest AS "bundle_digest?: Vec<u8>" FROM skill_commit
               WHERE workspace_id = ?1 AND skill_id = ?2 AND commit_id = ?3"#,
            ws_s,
            skill_s,
            cid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row.and_then(|r| r.bundle_digest) {
            Some(bytes) => Ok(Some(blob32(&bytes)?)),
            None => Ok(None),
        }
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

    /// The `(epoch, seq)` generation `current` points at for a skill, if a pointer exists (a pool read). The
    /// approve path uses it to classify a pre-transaction render fault: a fault on a proposal whose base still
    /// equals this is genuine corruption; a fault on one whose base no longer matches is a stale proposal the
    /// transaction will `CONFLICT` cleanly.
    pub(crate) async fn read_current_generation(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Option<Generation>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let row = sqlx::query!(
            r#"SELECT epoch AS "epoch!: i64", seq AS "seq!: i64" FROM current
               WHERE workspace_id = ?1 AND skill_id = ?2"#,
            ws_s,
            skill_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(Generation {
                epoch: i64_to_u64(r.epoch)?,
                seq: i64_to_u64(r.seq)?,
            })),
        }
    }

    /// The commit id `current` points at for a skill, if a pointer exists (revert's first parent).
    pub(crate) async fn read_current_commit(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Option<CommitId>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let row = sqlx::query!(
            r#"SELECT commit_id AS "commit_id!: Vec<u8>" FROM current
               WHERE workspace_id = ?1 AND skill_id = ?2"#,
            ws_s,
            skill_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| Ok(CommitId(blob32(&r.commit_id)?))).transpose()
    }

    /// The distinct object ids a commit reaches (revert derives the forward commit's object set from its
    /// target's reachability edges — no git_oid reverse-map, no byte reads).
    pub(crate) async fn commit_object_ids(
        &self,
        ws: &WorkspaceId,
        commit: CommitId,
    ) -> Result<Vec<ObjectId>> {
        let ws_s = ws.as_str();
        let cid = commit.0.as_slice();
        let rows = sqlx::query!(
            r#"SELECT object_id AS "object_id!: Vec<u8>" FROM commit_object
               WHERE workspace_id = ?1 AND commit_id = ?2"#,
            ws_s,
            cid,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| Ok(ObjectId(blob32(&r.object_id)?)))
            .collect()
    }

    /// Read back the signed `current` record (the serialized `SignedCurrentRecord` envelope) for a skill —
    /// what a follower's pointer fetch returns. `None` until the pointer has been moved (signed).
    pub(crate) async fn read_signed_record(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Option<Vec<u8>>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let row = sqlx::query!(
            r#"SELECT signed_record AS "signed_record?: Vec<u8>" FROM current
               WHERE workspace_id = ?1 AND skill_id = ?2"#,
            ws_s,
            skill_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.and_then(|r| r.signed_record))
    }

    /// Whether the workspace's review-required policy is on (the cheap preflight read; the in-txn read is
    /// authoritative). Absent row ⇒ off (the default).
    pub(crate) async fn workspace_review_required(&self, ws: &WorkspaceId) -> Result<bool> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT review_required AS "review_required!: i64" FROM workspace_policy WHERE workspace_id = ?1"#,
            ws_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some_and(|r| r.review_required != 0))
    }
}

/// The ordered sub-steps of the one transaction. Each ordering is load-bearing. Replay runs before authz, so
/// a since-revoked device still gets its stored OK on retry. Authz runs before the CAS, so an unauthorized
/// caller never learns the live generation from a CONFLICT. The CAS runs before availability and lineage, so
/// a stale base returns CONFLICT-rebase, never a confusing DENIED for GC-reclaimed objects. Provenance and
/// reachability are written before the pointer advance (the `current` to `skill_commit` foreign key is
/// immediate) and before the lease release, so the GC keep-set — any `commit_object` edge, or a live lease —
/// covers the objects continuously across the re-root, with no reclaim window.
async fn run(
    tx: &mut Transaction<'_, Sqlite>,
    input: &PromoteInput<'_>,
    signer: &PlaneSigner,
) -> Result<SetCurrentReceipt> {
    let bound = BoundIdentity {
        command: crate::set_current::device_op_command(input.op),
        skill_id: input.skill.as_str(),
        commit: Some(input.candidate_commit),
        bundle_digest: Some(input.candidate_bundle_digest),
        expected: input.expected,
    };

    // (1) Replay — return the stored receipt on a bound-identity match; a same-op_id different identity is a
    // permanent key-reuse (the receipt slot belongs to the original op, never overwritten).
    match replay(tx, input.ws, input.device_key_id, input.op_id, &bound).await? {
        Replay::Hit(receipt) => return Ok(receipt),
        Replay::Mismatch(original_at) => {
            return Ok(permanent_key_reuse(input.op_id, &bound, &original_at));
        }
        Replay::Fresh => {}
    }

    // (2) Policy — read inside the txn (the source of truth; a preflight may have read a now-stale value).
    let review_required = read_review_required(tx, input.ws).await?;

    // (3) Authorization — authoritative + in-txn. Resolve the device to a NON-REVOKED public key bound to a
    // principal, verify the device-op signature over SERVER-trusted fields, and require the principal is
    // rostered. A revoke committed before this txn is serialized ahead of it and blocks the move.
    let Some(device) = resolve_device(tx, input.ws, input.device_key_id).await? else {
        return denied(tx, input, &bound, "device unknown or revoked").await;
    };
    if device.revoked {
        return denied(tx, input, &bound, "device unknown or revoked").await;
    }
    let fields = DeviceOpFields {
        workspace_id: input.ws.as_str(),
        skill_id: input.skill.as_str(),
        op: input.op,
        op_id: input.op_id_bytes,
        device_key_id: input.device_key_id,
        expected_epoch: input.expected.epoch,
        expected_seq: input.expected.seq,
        commit_id: input.candidate_commit.0,
        bundle_digest: input.candidate_bundle_digest,
    };
    if !verify_device_op(&fields, input.signature, &device.public_key) {
        return denied(tx, input, &bound, "device signature invalid").await;
    }
    if !super::roster_exists(&mut **tx, input.ws, input.skill, &device.principal).await? {
        return denied(tx, input, &bound, "principal not rostered for the skill").await;
    }

    // (4) Compare-and-set on the WHOLE (epoch, seq). Absent pointer ⇒ the genesis branch (a zero-parent
    // create-at-(1,1)); a present pointer whose generation differs ⇒ CONFLICT carrying the LIVE generation.
    let current = read_current(tx, input.ws, input.skill).await?;
    let new_gen = match &current {
        None => {
            // Only a DIRECT publish may create the genesis pointer. A propose needs an existing base (a
            // proposal cannot be opened against a `current` that does not exist), and there is nothing to
            // approve or revert without one. The orchestration rejects these pre-ingest; this is the
            // in-transaction backstop — a typed DENIED, not a confusing genesis.
            if !matches!(input.op, DeviceOp::PublishDirect) {
                return denied(tx, input, &bound, "no current pointer to act against").await;
            }
            if !input.parents.is_empty() {
                return denied(
                    tx,
                    input,
                    &bound,
                    "no current pointer and a non-genesis commit",
                )
                .await;
            }
            Generation { epoch: 1, seq: 1 }
        }
        Some(cur) => {
            if cur.generation != input.expected {
                return conflict(tx, input, &bound, cur.generation).await;
            }
            // Guard the stored generation is in range (it carries no DB-level CHECK), then advance seq.
            if cur.generation.epoch > MAX_SAFE_INT || cur.generation.seq > MAX_SAFE_INT {
                return Err(AuthorityError::integrity(GenerationOutOfRange));
            }
            match cur.generation.seq.checked_add(1) {
                Some(seq) if seq <= MAX_SAFE_INT => Generation {
                    epoch: cur.generation.epoch,
                    seq,
                },
                _ => {
                    return permanent(
                        tx,
                        input,
                        &bound,
                        "generation would exceed the safe-integer bound",
                    )
                    .await;
                }
            }
        }
    };

    // (5) Availability — every candidate object is present (not deleting/absent/unavailable) and not
    // tombstoned. Plus the lease-completion gate: the committed (non-expiring) lease for THIS candidate is
    // the only in-txn evidence that migrate finished (commit_durable wrote the git commit + tree).
    for obj in input.object_ids {
        if !object_present_not_tombstoned(tx, input.ws, *obj).await? {
            return denied(
                tx,
                input,
                &bound,
                "a candidate object is not present or is tombstoned",
            )
            .await;
        }
    }
    // The lease-completion gate proves migrate finished for an UPLOADED candidate (publish / propose / revert
    // each hold a committed lease over their candidate). `review --approve` uploads and leases NOTHING — the
    // candidate is already durably in the main store, rooted by its proposal — so there is no lease to gate
    // on; skip it. (Its availability is still checked above, against the shared `present`/tombstone predicate.)
    if !matches!(input.op, DeviceOp::ReviewApprove) {
        match lease_committed_commit(tx, input.ws, input.op_id).await? {
            Some(c) if c == input.candidate_commit.0 => {}
            _ => {
                return retryable(
                    tx,
                    input,
                    &bound,
                    "the candidate's promotion lease is not committed",
                )
                .await;
            }
        }
    }

    // (6) Lineage — no cross-skill adoption; same-skill parents.
    if matches!(commit_owner(tx, input.ws, input.candidate_commit).await?, Some(owner) if owner != *input.skill)
    {
        return denied(
            tx,
            input,
            &bound,
            "candidate commit is owned by another skill",
        )
        .await;
    }
    let is_genesis = current.is_none();
    if !is_genesis {
        // Backbone rejects two-parent author merges wholesale (owned by a later increment).
        if input.parents.len() > 1 {
            return denied(
                tx,
                input,
                &bound,
                "two-parent author merges are not supported here",
            )
            .await;
        }
        // Same-skill lineage: every parent must already be in this skill's history.
        for p in input.parents {
            match commit_owner(tx, input.ws, *p).await? {
                Some(owner) if owner == *input.skill => {}
                _ => {
                    return denied(tx, input, &bound, "a parent is not in this skill's history")
                        .await;
                }
            }
        }
        // First-parent assert (load-bearing, orthogonal to the CAS): parents[0] == current.commit_id. A
        // CAS-pass + parent-mismatch is an es/commit desync (a clock anomaly) — a distinct DENIED carrying
        // the live commit id, never an auto-rebase.
        let cur = current.as_ref().expect("present current (non-genesis)");
        match input.parents.first() {
            Some(p0) if *p0 == cur.commit => {}
            _ => return first_parent_mismatch(tx, input, &bound, cur).await,
        }
        // The review-required gate (typed fail): a DIRECT publish only — revert + genesis bypass it. The
        // lease is released by the terminal-receipt writer (every non-OK outcome releases it).
        if matches!(input.op, DeviceOp::PublishDirect) && review_required {
            return approval_required(tx, input, &bound).await;
        }
    }

    // (7) The op-specific tail — over the SAME shared body above (replay, policy, authz, the whole-`(epoch,
    // seq)` CAS, availability, lineage, the first-parent assert, all of which ran for EVERY op). A direct
    // publish and a revert PROMOTE; `--propose` opens a proposal (no pointer move, nothing signed); `review
    // --approve` promotes the locked proposal sideways through the SAME promote plus the status handoff.
    // (`review --reject` never reaches `run` — it is a standalone status-flip transaction.)
    match input.op {
        DeviceOp::PublishDirect | DeviceOp::Revert => {
            promote(tx, input, signer, new_gen, &bound).await
        }
        DeviceOp::PublishPropose => propose_arm(tx, input, &bound, &device.principal).await,
        DeviceOp::ReviewApprove => {
            approve_arm(
                tx,
                input,
                signer,
                new_gen,
                &bound,
                review_required,
                &device.principal,
            )
            .await
        }
        DeviceOp::ReviewReject => Err(AuthorityError::internal(RejectNotPromotable)),
    }
}

/// Record provenance + reachability, sign the advanced pointer, and persist it with the durable OK receipt —
/// the shared pointer-advance for a direct publish, a revert, AND the accepted half of a proposal. The
/// `commit_object` edges it writes PERMANENTLY root the candidate's objects (the accepted-trunk root), so for
/// an approve this write IS the handoff from the proposal's gated `proposal_object` root to the trunk. Does
/// NOT touch the lease — the caller decides (publish/revert release theirs after this; approve has none).
async fn advance_current(
    tx: &mut Transaction<'_, Sqlite>,
    input: &PromoteInput<'_>,
    signer: &PlaneSigner,
    new_gen: Generation,
    bound: &BoundIdentity<'_>,
) -> Result<SetCurrentReceipt> {
    insert_skill_commit(
        tx,
        input.ws,
        input.candidate_commit,
        input.skill,
        input.candidate_bundle_digest,
    )
    .await?;
    for obj in input.object_ids {
        insert_commit_object(tx, input.ws, input.candidate_commit, *obj).await?;
    }
    let pointer = CurrentPointer {
        workspace_id: input.ws.as_str(),
        skill_id: input.skill.as_str(),
        version_id: input.candidate_commit.0,
        epoch: new_gen.epoch,
        seq: new_gen.seq,
    };
    let signature = signer.sign_pointer(&pointer)?;
    let signed_record = serialize_signed_record(&pointer, signer.key_id(), &signature)?;
    upsert_current(
        tx,
        input.ws,
        input.skill,
        input.candidate_commit,
        new_gen,
        &signed_record,
        input.now,
    )
    .await?;
    let stored = StoredReceipt {
        op_id: input.op_id.to_owned(),
        command: bound.command.to_owned(),
        skill_id: input.skill.as_str().to_owned(),
        commit: Some(input.candidate_commit),
        bundle_digest: Some(input.candidate_bundle_digest),
        expected: input.expected,
        outcome: TerminalOutcome::Ok,
        current: Some(new_gen),
        signed_record: Some(signed_record),
        key_id: Some(signer.key_id().to_owned()),
        created_at: input.created_at.to_owned(),
        details: None,
    };
    insert_receipt(tx, input.ws, input.device_key_id, &stored).await?;
    Ok(stored.into_receipt())
}

/// A direct publish / revert: advance `current`, then release the lease — AFTER the edges root the objects,
/// so the GC keep-set never had a gap.
async fn promote(
    tx: &mut Transaction<'_, Sqlite>,
    input: &PromoteInput<'_>,
    signer: &PlaneSigner,
    new_gen: Generation,
    bound: &BoundIdentity<'_>,
) -> Result<SetCurrentReceipt> {
    let receipt = advance_current(tx, input, signer, new_gen, bound).await?;
    delete_lease(tx, input.ws, input.op_id).await?;
    Ok(receipt)
}

/// `publish --propose`: open a proposal — record provenance, the proposal row, and its GATED object roots —
/// WITHOUT moving `current` or signing anything. The proposal is born NON-STALE (the CAS proved `current.es
/// == expected_es`, which is recorded as its base). Then release the migrate lease: the `proposal_object`
/// rows now root the objects (the lease → proposal-root handoff, done INSIDE the one transaction, so a
/// concurrent GC sees old-lease-or-new-root, never neither). An idempotent re-propose of the same
/// candidate+base under a NEW op_id finds the existing open proposal and returns NEEDS_REVIEW without
/// inserting a duplicate (the partial-unique index is the structural backstop).
async fn propose_arm(
    tx: &mut Transaction<'_, Sqlite>,
    input: &PromoteInput<'_>,
    bound: &BoundIdentity<'_>,
    proposer: &Principal,
) -> Result<SetCurrentReceipt> {
    let base = input.expected;
    // A DIFFERENT device minting this op_id (a ~122-bit UUID collision; a same-device retry already replayed
    // at step 1, before `run` reached here) would PK-collide on the `proposals` row and fault a non-receipted
    // `Internal`. Preempt it with a typed, receipted permanent failure (which also releases the staged lease).
    if proposal_id_exists(tx, input.ws, input.op_id).await? {
        return permanent(tx, input, bound, "op id already names a proposal").await;
    }
    // Provenance first (the `proposals.commit_id` foreign key targets `skill_commit`).
    insert_skill_commit(
        tx,
        input.ws,
        input.candidate_commit,
        input.skill,
        input.candidate_bundle_digest,
    )
    .await?;
    if read_open_proposal(tx, input.ws, input.skill, input.candidate_commit, base)
        .await?
        .is_none()
    {
        // The candidate's first parent == `current.commit_id` (asserted in the shared body); recorded as the
        // authoritative first-parent source a later `review --approve` re-asserts against the live `current`.
        let base_commit = input
            .parents
            .first()
            .copied()
            .ok_or_else(|| AuthorityError::internal(ProposeWithoutBase))?;
        insert_proposal(
            tx,
            input.ws,
            input.op_id,
            input.skill,
            input.candidate_commit,
            base_commit,
            base,
            proposer,
            input.created_at,
        )
        .await?;
        for obj in input.object_ids {
            insert_proposal_object(tx, input.ws, input.op_id, *obj).await?;
        }
    }
    let stored = StoredReceipt {
        op_id: input.op_id.to_owned(),
        command: bound.command.to_owned(),
        skill_id: input.skill.as_str().to_owned(),
        commit: Some(input.candidate_commit),
        bundle_digest: Some(input.candidate_bundle_digest),
        expected: input.expected,
        outcome: TerminalOutcome::NeedsReview,
        current: None,
        signed_record: None,
        key_id: None,
        created_at: input.created_at.to_owned(),
        details: None,
    };
    insert_receipt(tx, input.ws, input.device_key_id, &stored).await?;
    delete_lease(tx, input.ws, input.op_id).await?;
    Ok(stored.into_receipt())
}

/// `review --approve`: promote the locked open proposal SIDEWAYS to `current`. The shared body already ran
/// the CAS (a stale base ⇒ CONFLICT *before* here), availability (against the shared `present`/tombstone
/// predicate, no lease gate), and the first-parent assert. Here: lock + assert the open proposal under the
/// write lock; enforce four-eyes under `review_required`; record the approval; advance `current` (whose
/// `commit_object` write is the handoff from the gated `proposal_object` root to the permanent trunk root);
/// flip the proposal to `accepted`. No lease to release.
async fn approve_arm(
    tx: &mut Transaction<'_, Sqlite>,
    input: &PromoteInput<'_>,
    signer: &PlaneSigner,
    new_gen: Generation,
    bound: &BoundIdentity<'_>,
    review_required: bool,
    reviewer: &Principal,
) -> Result<SetCurrentReceipt> {
    let base = input.expected; // == current.es (the CAS proved it) ⇒ this is NOT a stale CONFLICT
    let Some(proposal) =
        read_open_proposal(tx, input.ws, input.skill, input.candidate_commit, base).await?
    else {
        // The CAS passed (current.es == base), so the base is fresh — yet no OPEN proposal matches it: the
        // proposal was already accepted, or rejected. A resolved/absent target, not a stale base ⇒ DENIED.
        return denied(
            tx,
            input,
            bound,
            "no open proposal for this candidate and base",
        )
        .await;
    };
    // Four-eyes (the anti-poisoning gate): fires ONLY under `review_required`, where the gate's value needs
    // a SECOND actor. With it off, a solo author may approve their own proposal (a deferred self-publish).
    if review_required && reviewer.as_str() == proposal.proposer.as_str() {
        return denied(
            tx,
            input,
            bound,
            "the proposer may not approve their own proposal under review-required",
        )
        .await;
    }
    insert_approval(
        tx,
        input.ws,
        input.candidate_commit,
        base,
        reviewer,
        input.created_at,
    )
    .await?;
    let receipt = advance_current(tx, input, signer, new_gen, bound).await?;
    set_proposal_status(
        tx,
        input.ws,
        &proposal.id,
        ProposalStatus::Accepted,
        reviewer,
    )
    .await?;
    Ok(receipt)
}

// --- review --reject / proposer-withdraw (a standalone status-flip transaction, not a pointer move) ---

async fn reject_run(
    tx: &mut Transaction<'_, Sqlite>,
    r: &RejectInput<'_>,
) -> Result<SetCurrentReceipt> {
    let bound = BoundIdentity {
        command: crate::set_current::device_op_command(r.op),
        skill_id: r.skill.as_str(),
        commit: Some(r.commit),
        bundle_digest: Some(r.bundle_digest),
        expected: r.expected,
    };

    // (1) Replay — a same-op_id retry replays the stored receipt; a different bound identity is key-reuse.
    match replay(tx, r.ws, r.device_key_id, r.op_id, &bound).await? {
        Replay::Hit(receipt) => return Ok(receipt),
        Replay::Mismatch(original_at) => {
            return Ok(permanent_key_reuse(r.op_id, &bound, &original_at));
        }
        Replay::Fresh => {}
    }

    // (2) Authorization — the SAME in-transaction device-op verification the promotion runs (a non-revoked
    // registered key bound to a rostered principal), over the `ReviewReject`-typed frame. A revoke serialized
    // ahead of this blocks the reject.
    let Some(device) = resolve_device(tx, r.ws, r.device_key_id).await? else {
        return reject_denied(tx, r, "device unknown or revoked").await;
    };
    if device.revoked {
        return reject_denied(tx, r, "device unknown or revoked").await;
    }
    let fields = DeviceOpFields {
        workspace_id: r.ws.as_str(),
        skill_id: r.skill.as_str(),
        op: r.op,
        op_id: r.op_id_bytes,
        device_key_id: r.device_key_id,
        expected_epoch: r.expected.epoch,
        expected_seq: r.expected.seq,
        commit_id: r.commit.0,
        bundle_digest: r.bundle_digest,
    };
    if !verify_device_op(&fields, r.signature, &device.public_key) {
        return reject_denied(tx, r, "device signature invalid").await;
    }
    if !super::roster_exists(&mut **tx, r.ws, r.skill, &device.principal).await? {
        return reject_denied(tx, r, "principal not rostered for the skill").await;
    }

    // (3) Resolve + classify the proposal (under the write lock). One reject path serves reviewer-reject and
    // proposer-withdraw; `resolved_by` records the acting principal either way.
    match resolve_proposal(tx, r.ws, r.skill, r.commit, r.expected).await? {
        Some((id, ProposalStatus::Open)) => {
            set_proposal_status(tx, r.ws, &id, ProposalStatus::Rejected, &device.principal).await?;
            reject_terminal(tx, r, TerminalOutcome::Ok, "PROPOSAL_REJECTED").await
        }
        // Idempotent: a lost-ack retry under a NEW op_id (the original op_id replays at step 1) — already done.
        Some((_, ProposalStatus::Rejected)) => {
            reject_terminal(tx, r, TerminalOutcome::Ok, "PROPOSAL_ALREADY_REJECTED").await
        }
        // An accepted proposal is terminal the other way; absent means there is nothing to reject.
        Some((_, ProposalStatus::Accepted)) => {
            reject_denied(tx, r, "the proposal is already accepted").await
        }
        None => reject_denied(tx, r, "no open proposal for this candidate and base").await,
    }
}

/// Write a reject terminal receipt (no pointer data, no lease — reject moves nothing). `code` distinguishes a
/// fresh rejection from an idempotent re-reject in the receipt `details`, so a caller branches on it rather
/// than on the `Ok` outcome (which a reject shares with a promotion).
async fn reject_terminal(
    tx: &mut Transaction<'_, Sqlite>,
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

async fn reject_denied(
    tx: &mut Transaction<'_, Sqlite>,
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

// --- terminal-outcome receipt writers (each persists the receipt in-txn and returns the projection) ---

async fn denied(
    tx: &mut Transaction<'_, Sqlite>,
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

async fn conflict(
    tx: &mut Transaction<'_, Sqlite>,
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

async fn first_parent_mismatch(
    tx: &mut Transaction<'_, Sqlite>,
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

async fn approval_required(
    tx: &mut Transaction<'_, Sqlite>,
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

async fn retryable(
    tx: &mut Transaction<'_, Sqlite>,
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

async fn permanent(
    tx: &mut Transaction<'_, Sqlite>,
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
    tx: &mut Transaction<'_, Sqlite>,
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
fn permanent_key_reuse(
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

// --- the bound identity + replay ---

/// The fields a same-`op_id` retry must match to replay (the value of the receipt, never its key).
struct BoundIdentity<'a> {
    command: &'a str,
    skill_id: &'a str,
    commit: Option<CommitId>,
    bundle_digest: Option<[u8; 32]>,
    expected: Generation,
}

// A transient control-flow enum: built and immediately destructured in `replay`, never stored or collected,
// so the size gap between the (intentionally rich) `Hit` receipt and the unit arms is irrelevant — boxing it
// would add a heap alloc on the replay path for no benefit.
#[allow(clippy::large_enum_variant)]
enum Replay {
    Hit(SetCurrentReceipt),
    /// The slot exists but the bound identity differs (a key reuse). Carries the ORIGINAL receipt's
    /// `created_at` so the synthesized reuse receipt is byte-stable across retries (it is itself never stored).
    Mismatch(String),
    Fresh,
}

async fn replay(
    tx: &mut Transaction<'_, Sqlite>,
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

struct StoredReceipt {
    op_id: String,
    command: String,
    skill_id: String,
    commit: Option<CommitId>,
    bundle_digest: Option<[u8; 32]>,
    expected: Generation,
    outcome: TerminalOutcome,
    current: Option<Generation>,
    signed_record: Option<Vec<u8>>,
    key_id: Option<String>,
    created_at: String,
    details: Option<serde_json::Value>,
}

impl StoredReceipt {
    fn into_receipt(self) -> SetCurrentReceipt {
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

// --- tx-bound + pool SQL helpers (every one workspace-scoped) ---

struct DeviceRecord {
    public_key: [u8; 32],
    principal: Principal,
    revoked: bool,
}

async fn resolve_device(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    device_key_id: &str,
) -> Result<Option<DeviceRecord>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT public_key AS "public_key!: Vec<u8>", principal AS "principal!", revoked AS "revoked!: i64"
           FROM device_registry WHERE workspace_id = ?1 AND device_key_id = ?2"#,
        ws_s,
        device_key_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => {
            // Stored values are validated on the way in, so a re-parse failure is store corruption.
            let public_key = blob32(&r.public_key)?;
            let principal = Principal::parse(&r.principal).map_err(AuthorityError::integrity)?;
            Ok(Some(DeviceRecord {
                public_key,
                principal,
                revoked: r.revoked != 0,
            }))
        }
    }
}

async fn read_review_required(tx: &mut Transaction<'_, Sqlite>, ws: &WorkspaceId) -> Result<bool> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT review_required AS "review_required!: i64" FROM workspace_policy WHERE workspace_id = ?1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some_and(|r| r.review_required != 0))
}

struct CurrentRow {
    commit: CommitId,
    generation: Generation,
}

async fn read_current(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    skill: &SkillId,
) -> Result<Option<CurrentRow>> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let row = sqlx::query!(
        r#"SELECT commit_id AS "commit_id!: Vec<u8>", epoch AS "epoch!: i64", seq AS "seq!: i64"
           FROM current WHERE workspace_id = ?1 AND skill_id = ?2"#,
        ws_s,
        skill_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(CurrentRow {
            commit: CommitId(blob32(&r.commit_id)?),
            generation: Generation {
                epoch: i64_to_u64(r.epoch)?,
                seq: i64_to_u64(r.seq)?,
            },
        })),
    }
}

async fn object_present_not_tombstoned(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    object_id: ObjectId,
) -> Result<bool> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    let row = sqlx::query!(
        r#"SELECT 1 AS "ok!: i64" FROM object_presence
           WHERE workspace_id = ?1 AND object_id = ?2 AND status = 'present'
             AND NOT EXISTS (SELECT 1 FROM tombstones WHERE workspace_id = ?1 AND blob_id = ?2)"#,
        ws_s,
        oid,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some())
}

async fn lease_committed_commit(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    op_id: &str,
) -> Result<Option<[u8; 32]>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT commit_id AS "commit_id!: Vec<u8>" FROM promotion_lease
           WHERE workspace_id = ?1 AND op_id = ?2 AND expires_at IS NULL"#,
        ws_s,
        op_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    row.map(|r| blob32(&r.commit_id)).transpose()
}

async fn commit_owner(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    commit: CommitId,
) -> Result<Option<SkillId>> {
    let ws_s = ws.as_str();
    let cid = commit.0.as_slice();
    let row = sqlx::query!(
        r#"SELECT skill_id AS "skill_id!" FROM skill_commit WHERE workspace_id = ?1 AND commit_id = ?2"#,
        ws_s,
        cid,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            SkillId::parse(&r.skill_id).map_err(AuthorityError::integrity)?,
        )),
    }
}

async fn insert_skill_commit(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    commit: CommitId,
    skill: &SkillId,
    bundle_digest: [u8; 32],
) -> Result<()> {
    let ws_s = ws.as_str();
    let cid = commit.0.as_slice();
    let skill_s = skill.as_str();
    let digest = bundle_digest.as_slice();
    // Backfill the digest if a row already exists with none (a commit recorded before the pointer-move
    // path, or by a digest-less writer) — otherwise that version, once current, could never be a revert
    // target. COALESCE keeps an existing digest (idempotent) and never changes the owning skill (the
    // cross-skill-adoption check already ran, so the existing row is this skill's).
    sqlx::query!(
        "INSERT INTO skill_commit (workspace_id, commit_id, skill_id, bundle_digest) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (workspace_id, commit_id) \
         DO UPDATE SET bundle_digest = COALESCE(skill_commit.bundle_digest, excluded.bundle_digest)",
        ws_s,
        cid,
        skill_s,
        digest,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

async fn insert_commit_object(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    commit: CommitId,
    object_id: ObjectId,
) -> Result<()> {
    let ws_s = ws.as_str();
    let cid = commit.0.as_slice();
    let oid = object_id.0.as_slice();
    sqlx::query!(
        "INSERT INTO commit_object (workspace_id, commit_id, object_id) VALUES (?1, ?2, ?3) \
         ON CONFLICT (workspace_id, commit_id, object_id) DO NOTHING",
        ws_s,
        cid,
        oid,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

async fn upsert_current(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    generation: Generation,
    signed_record: &[u8],
    updated_at: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let cid = commit.0.as_slice();
    let epoch = u64_to_i64(generation.epoch)?;
    let seq = u64_to_i64(generation.seq)?;
    sqlx::query!(
        "INSERT INTO current (workspace_id, skill_id, commit_id, epoch, seq, signed_record, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (workspace_id, skill_id) DO UPDATE SET \
           commit_id = excluded.commit_id, epoch = excluded.epoch, seq = excluded.seq, \
           signed_record = excluded.signed_record, updated_at = excluded.updated_at",
        ws_s,
        skill_s,
        cid,
        epoch,
        seq,
        signed_record,
        updated_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

async fn delete_lease(
    tx: &mut Transaction<'_, Sqlite>,
    ws: &WorkspaceId,
    op_id: &str,
) -> Result<()> {
    let ws_s = ws.as_str();
    sqlx::query!(
        "DELETE FROM promotion_lease WHERE workspace_id = ?1 AND op_id = ?2",
        ws_s,
        op_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

async fn get_receipt<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    device_key_id: &str,
    op_id: &str,
) -> Result<Option<StoredReceipt>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT command AS "command!", skill_id AS "skill_id!",
                  commit_id AS "commit_id?: Vec<u8>", bundle_digest AS "bundle_digest?: Vec<u8>",
                  expected_epoch AS "expected_epoch!: i64", expected_seq AS "expected_seq!: i64",
                  outcome AS "outcome!", current_epoch AS "current_epoch?: i64",
                  current_seq AS "current_seq?: i64", signed_record AS "signed_record?: Vec<u8>",
                  key_id AS "key_id?", created_at AS "created_at!", details AS "details?"
           FROM op_receipts WHERE workspace_id = ?1 AND device_key_id = ?2 AND op_id = ?3"#,
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
        details: r
            .details
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok()),
    }))
}

async fn insert_receipt(
    tx: &mut Transaction<'_, Sqlite>,
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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

/// Serialize the `SignedCurrentRecord` envelope stored in `current.signed_record` + `op_receipts`. The
/// signature is over `pointer_preimage` (the JCS string), NOT this envelope — a verifier reconstructs the
/// `CurrentPointer` strictly from {scope, record} and re-derives the preimage; `key_id`/`schema_version`
/// are NOT part of the signed bytes.
fn serialize_signed_record(
    pointer: &CurrentPointer<'_>,
    key_id: &str,
    signature: &[u8; 64],
) -> Result<Vec<u8>> {
    let record = SignedCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: pointer.workspace_id.to_owned(),
            skill_id: pointer.skill_id.to_owned(),
        },
        record: CurrentRecord {
            version_id: topos_core::digest::to_hex(&pointer.version_id),
            generation: Generation {
                epoch: pointer.epoch,
                seq: pointer.seq,
            },
        },
        signature: Signature {
            alg: SignatureAlg::Ed25519,
            key_id: key_id.to_owned(),
            value: base64_url(signature),
        },
    };
    serde_json::to_vec(&record).map_err(AuthorityError::internal)
}

/// base64url, unpadded — the frozen wire form of a 64-byte signature (86 chars).
fn base64_url(signature: &[u8; 64]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature)
}

// --- small conversions (a stored value that violates a width/range CHECK is store corruption) ---

fn blob32(bytes: &[u8]) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadBlobWidth))
}

fn i64_to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

fn u64_to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

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
#[error("stored content id is not 32 bytes")]
struct BadBlobWidth;

#[derive(Debug, thiserror::Error)]
#[error("a stored generation is out of the safe-integer range")]
struct GenerationOutOfRange;

#[derive(Debug, thiserror::Error)]
#[error("a stored receipt outcome is not a known terminal code")]
struct BadOutcome;

#[derive(Debug, thiserror::Error)]
#[error("a propose reached the open step with no recorded base parent")]
struct ProposeWithoutBase;

#[derive(Debug, thiserror::Error)]
#[error("review --reject must not be promoted through the pointer-move transaction")]
struct RejectNotPromotable;
