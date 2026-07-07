//! The one pointer-move transaction — the raw-SQL half of `set-current`.
//!
//! One `SERIALIZABLE` (`run_serializable!`) write transaction advances a skill's `current` pointer by exactly one step, under
//! a compare-and-set on the whole `(epoch, seq)` pair, signs the new pointer, re-roots the migrated bytes,
//! and writes a durable all-outcome receipt — **with no filesystem op inside the transaction**. The
//! ordered sub-steps (and why each ordering is load-bearing) are in [`run`]. All `sqlx` stays here; the
//! caller ([`crate::set_current`]) hands in server-trusted values and a signer and gets back a domain
//! [`SetCurrentReceipt`]. The receipt persistence/replay machinery + the terminal-outcome writers `run` and
//! `reject_run` call live in [`super::receipts`]; this file keeps the ordered state machine itself.

use sqlx::{Postgres, Transaction};
use topos_core::sign::{CurrentPointer, DeviceOp, DeviceOpFields, verify_device_op};
use topos_types::{
    CurrentRecord, Generation, PointerScope, Signature, SignatureAlg, SignedCurrentRecord,
    TerminalOutcome, WIRE_SCHEMA_VERSION,
};

use super::governance::read_member_role;
use super::proposals::{
    ProposalStatus, insert_approval, insert_proposal, insert_proposal_object, proposal_id_exists,
    read_open_proposal, resolve_proposal, set_proposal_status,
};
use super::receipts::{
    BoundIdentity, Replay, StoredReceipt, approval_required, conflict, denied, denied_code,
    denied_preauth, first_parent_mismatch, insert_receipt, permanent, permanent_key_reuse,
    reject_denied, reject_denied_code, reject_denied_preauth, reject_terminal, replay, retryable,
};
use super::{Db, blob32};
use crate::actor::WriteActor;
use crate::error::{AuthorityError, Result};
use crate::governance::Role;
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};
use crate::session_review::{
    REVIEWER_ROLE_REQUIRED_CODE, REVIEWER_ROLE_REQUIRED_MSG, SESSION_REVIEW_ACTING_DENIED,
};
use crate::set_current::{PromoteInput, RejectInput, SetCurrentReceipt};
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
        run_serializable!(self, tx, run(&mut tx, &input, signer).await)
    }

    /// The standalone `review --reject` / proposer-withdraw transaction. NOT a pointer move — it never enters
    /// [`run`]: `current` is untouched, nothing is signed, there is no lease. One `SERIALIZABLE` transaction mirrors the
    /// promotion's discipline where it overlaps — receipt-replay first, then in-transaction authorization (the
    /// SAME device-op frame, `op = ReviewReject`) — then resolves the proposal and classifies it: `open` ⇒
    /// flip to `rejected`; already `rejected` ⇒ idempotent OK (a lost-ack retry under a different op_id); and
    /// `accepted` or absent ⇒ a typed DENIED. One path serves both reviewer-reject and proposer-withdraw;
    /// `resolved_by` records who.
    pub(crate) async fn review_reject_txn(&self, r: RejectInput<'_>) -> Result<SetCurrentReceipt> {
        run_serializable!(self, tx, reject_run(&mut tx, &r).await)
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
               WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = $3"#,
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
               WHERE workspace_id = $1 AND skill_id = $2"#,
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
               WHERE workspace_id = $1 AND skill_id = $2"#,
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
               WHERE workspace_id = $1 AND commit_id = $2"#,
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
               WHERE workspace_id = $1 AND skill_id = $2"#,
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
            r#"SELECT review_required AS "review_required!: i64" FROM workspace_policy WHERE workspace_id = $1"#,
            ws_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some_and(|r| r.review_required != 0))
    }

    /// Upsert the workspace's `review_required` policy (the write the read above consults). The single home
    /// for the policy row; `Authority::set_review_required` is the public op, and the test-fixtures
    /// `seed_review_required` shim delegates to it. The upsert has no foreign key onto the standalone
    /// `workspace` row (so the publish/read tests that seed no workspace stay green).
    pub(crate) async fn set_review_required(
        &self,
        ws: &WorkspaceId,
        review_required: bool,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let rr = i64::from(review_required);
        sqlx::query!(
            "INSERT INTO workspace_policy (workspace_id, review_required) VALUES ($1, $2) \
             ON CONFLICT (workspace_id) DO UPDATE SET review_required = excluded.review_required",
            ws_s,
            rr,
        )
        .execute(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
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
    tx: &mut Transaction<'_, Postgres>,
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
    // permanent key-reuse (the receipt slot belongs to the original op, never overwritten). The probe is
    // lane-blind (see `replay`): a device op id and a session request id fail closed against each other.
    match replay(
        tx,
        input.ws,
        &input.actor.receipt_actor(),
        input.op_id,
        &bound,
    )
    .await?
    {
        Replay::Hit(receipt) => return Ok(receipt),
        Replay::Mismatch(original_at) => {
            // A key-reuse refusal ABANDONS the incoming candidate, exactly like the receipted
            // terminals below: a publish/propose/revert already migrated its bytes under a committed
            // lease keyed by this op_id (the slot owner's own lease is long gone — its terminal
            // writer released it), so without this release the rejected candidate's objects would
            // stay GC-rooted forever. An approve leases nothing — skip it, as everywhere else.
            if !matches!(input.op, DeviceOp::ReviewApprove) {
                delete_lease(tx, input.ws, input.op_id).await?;
            }
            return Ok(permanent_key_reuse(input.op_id, &bound, &original_at));
        }
        Replay::Fresh => {}
    }

    // (2) Policy — read inside the txn (the source of truth; a preflight may have read a now-stale value).
    let review_required = read_review_required(tx, input.ws).await?;

    // (3 + 3b) Authorization — authoritative + in-txn, and the ONE place the transaction branches on the
    // lane. Both arms read `current` only after their authentication step, so an unauthorized caller never
    // learns the live generation; both hoist the acting principal the actor-blind tails consume.
    //
    // DELIBERATE LANE ASYMMETRY (stated once, here): the device arm's gate is per-skill `roster`
    // membership (genesis-aware, unchanged); the session arm's gate is the WORKSPACE role — a confirmed
    // owner or reviewer seat, the first enforcement of the reviewer role. A session reviewer needs no
    // per-skill roster row (the session read lane's catalog-visibility-is-membership decision, carried to
    // the review write).
    let (acting, genesis_standup, current) = match &input.actor {
        WriteActor::Device {
            device_key_id,
            signature,
        } => {
            // Resolve the device to a NON-REVOKED public key bound to a principal, verify the device-op
            // signature over SERVER-trusted fields, and require the principal is rostered. A revoke
            // committed before this txn is serialized ahead of it and blocks the move. A
            // PRE-AUTHENTICATION failure (unknown/revoked device, invalid signature) is DENIED **without a
            // durable receipt** — see `denied_preauth`; the authenticated-but-unauthorized denials below
            // stay receipted.
            let Some(device) = resolve_device(tx, input.ws, device_key_id).await? else {
                return denied_preauth(tx, input, &bound, "device unknown or revoked").await;
            };
            if device.revoked {
                return denied_preauth(tx, input, &bound, "device unknown or revoked").await;
            }
            let fields = DeviceOpFields {
                workspace_id: input.ws.as_str(),
                skill_id: input.skill.as_str(),
                op: input.op,
                op_id: input.op_id_bytes,
                device_key_id,
                expected_epoch: input.expected.epoch,
                expected_seq: input.expected.seq,
                commit_id: input.candidate_commit.0,
                bundle_digest: input.candidate_bundle_digest,
            };
            if !verify_device_op(&fields, signature, &device.public_key) {
                return denied_preauth(tx, input, &bound, "device signature invalid").await;
            }
            // The roster gate — genesis-aware. `current` is read FIRST so a missing per-skill roster row is
            // tolerated ONLY on the genesis-eligible shape (absent pointer + a zero-parent direct publish)
            // by a CONFIRMED workspace member — a fresh skill has no roster yet, so its first publisher
            // must be seated by workspace membership, then self-rostered. The self-INSERT is DEFERRED to
            // just before the op tail: every terminal writer below (denied/conflict/retryable) COMMITS its
            // receipt, so an inline insert here would commit an orphan roster row alongside a later
            // availability/lineage DENIED.
            let current = read_current(tx, input.ws, input.skill).await?;
            let genesis_standup =
                if super::roster_exists(&mut **tx, input.ws, input.skill, &device.principal).await?
                {
                    false
                } else {
                    let genesis_shaped = current.is_none()
                        && matches!(input.op, DeviceOp::PublishDirect)
                        && input.parents.is_empty();
                    if !genesis_shaped {
                        return denied(tx, input, &bound, "principal not rostered for the skill")
                            .await;
                    }
                    if !super::workspace_member_confirmed(&mut **tx, input.ws, &device.principal)
                        .await?
                    {
                        return denied(
                            tx,
                            input,
                            &bound,
                            "principal is not a confirmed workspace member",
                        )
                        .await;
                    }
                    true
                };
            (device.principal, genesis_standup, current)
        }
        WriteActor::Session { acting, .. } => {
            // The public session API constructs ONLY a review approve; any other op here is an internal
            // mis-route, a fault — never a receipt.
            if !matches!(input.op, DeviceOp::ReviewApprove) {
                return Err(AuthorityError::internal(SessionOpNotReviewable));
            }
            // The in-txn role gate (authoritative; the orchestration's pool-level pre-gate is the cheap
            // fence). A non-member / merely-invited / unknown-workspace caller gets the ONE uniform
            // denial, synthesized — never persisted (the session recording rule: a web-verified email
            // proves nothing about THIS workspace, and a durable row would let any account grow the
            // ledger). A CONFIRMED plain member is entitled to a recorded, replayable answer: the durable
            // typed role denial.
            match read_member_role(tx, input.ws, acting).await? {
                Some((role, status)) if status == "confirmed" => {
                    if role != Role::Owner.as_str() && role != Role::Reviewer.as_str() {
                        return denied_code(
                            tx,
                            input,
                            &bound,
                            REVIEWER_ROLE_REQUIRED_CODE,
                            REVIEWER_ROLE_REQUIRED_MSG,
                        )
                        .await;
                    }
                }
                _ => {
                    return denied_preauth(tx, input, &bound, SESSION_REVIEW_ACTING_DENIED).await;
                }
            }
            let current = read_current(tx, input.ws, input.skill).await?;
            ((*acting).clone(), false, current)
        }
    };

    // (4) Compare-and-set on the WHOLE (epoch, seq). Absent pointer ⇒ the genesis branch (a zero-parent
    // create-at-(1,1)); a present pointer whose generation differs ⇒ CONFLICT carrying the LIVE generation.
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
    // tombstoned, checked as ONE set-valued statement (a per-object round-trip would stretch the
    // SERIALIZABLE conflict window linearly with bundle size). Plus the lease-completion gate: the
    // committed (non-expiring) lease for THIS candidate is the only in-txn evidence that migrate finished
    // (commit_durable wrote the git commit + tree).
    if !all_present_not_tombstoned(tx, input.ws, input.object_ids).await? {
        return denied(
            tx,
            input,
            &bound,
            "a candidate object is not present or is tombstoned",
        )
        .await;
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
    if let Some(cur) = &current {
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

    // (6b) The genesis standup — every deny-returning gate above has passed, and for a genesis (`current`
    // absent) the only outcomes past this point are the promote's OK or a rolling-back `Err`, so the
    // self-inserted roster row and the pointer land in ONE transaction, never an orphan. This insert must
    // stay the LAST statement before the op tail — a future receipted-terminal between here and the promote
    // would re-open the orphan-row window.
    if genesis_standup {
        super::insert_roster(&mut **tx, input.ws, input.skill, &acting).await?;
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
        DeviceOp::PublishPropose => propose_arm(tx, input, &bound, &acting).await,
        DeviceOp::ReviewApprove => {
            approve_arm(tx, input, signer, new_gen, &bound, review_required, &acting).await
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
    tx: &mut Transaction<'_, Postgres>,
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
        input.display_name,
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
    insert_receipt(tx, input.ws, &input.actor.receipt_actor(), &stored).await?;
    Ok(stored.into_receipt())
}

/// A direct publish / revert: advance `current`, then release the lease — AFTER the edges root the objects,
/// so the GC keep-set never had a gap.
async fn promote(
    tx: &mut Transaction<'_, Postgres>,
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
    tx: &mut Transaction<'_, Postgres>,
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
    insert_receipt(tx, input.ws, &input.actor.receipt_actor(), &stored).await?;
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
    tx: &mut Transaction<'_, Postgres>,
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
        None,
        input.created_at,
    )
    .await?;
    Ok(receipt)
}

// --- review --reject / proposer-withdraw (a standalone status-flip transaction, not a pointer move) ---

async fn reject_run(
    tx: &mut Transaction<'_, Postgres>,
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
    // The probe is lane-blind (see `replay`), exactly as in `run`.
    match replay(tx, r.ws, &r.actor.receipt_actor(), r.op_id, &bound).await? {
        Replay::Hit(receipt) => return Ok(receipt),
        Replay::Mismatch(original_at) => {
            return Ok(permanent_key_reuse(r.op_id, &bound, &original_at));
        }
        Replay::Fresh => {}
    }

    // (2) Authorization — the reject twin of `run`'s one lane fork (the same deliberate lane asymmetry:
    // device = per-skill roster; session = confirmed owner|reviewer workspace seat).
    let acting: Principal = match &r.actor {
        WriteActor::Device {
            device_key_id,
            signature,
        } => {
            // The SAME in-transaction device-op verification the promotion runs (a non-revoked registered
            // key bound to a rostered principal), over the `ReviewReject`-typed frame. A revoke serialized
            // ahead of this blocks the reject. A pre-authentication failure is DENIED without a durable
            // receipt (mirroring `run`'s `denied_preauth`; a reject has no lease, so nothing is even
            // released) — the roster denial below names a verified device and stays receipted.
            let Some(device) = resolve_device(tx, r.ws, device_key_id).await? else {
                return Ok(reject_denied_preauth(r, "device unknown or revoked"));
            };
            if device.revoked {
                return Ok(reject_denied_preauth(r, "device unknown or revoked"));
            }
            let fields = DeviceOpFields {
                workspace_id: r.ws.as_str(),
                skill_id: r.skill.as_str(),
                op: r.op,
                op_id: r.op_id_bytes,
                device_key_id,
                expected_epoch: r.expected.epoch,
                expected_seq: r.expected.seq,
                commit_id: r.commit.0,
                bundle_digest: r.bundle_digest,
            };
            if !verify_device_op(&fields, signature, &device.public_key) {
                return Ok(reject_denied_preauth(r, "device signature invalid"));
            }
            if !super::roster_exists(&mut **tx, r.ws, r.skill, &device.principal).await? {
                return reject_denied(tx, r, "principal not rostered for the skill").await;
            }
            device.principal
        }
        WriteActor::Session { acting, .. } => {
            // The public session API constructs ONLY a review reject here (see `run`'s session belt).
            if !matches!(r.op, DeviceOp::ReviewReject) {
                return Err(AuthorityError::internal(SessionOpNotReviewable));
            }
            // The same session role gate as `run`: uniform synthesized denial for anyone unproven in THIS
            // workspace; a durable typed role denial for a confirmed plain member.
            match read_member_role(tx, r.ws, acting).await? {
                Some((role, status)) if status == "confirmed" => {
                    if role != Role::Owner.as_str() && role != Role::Reviewer.as_str() {
                        return reject_denied_code(
                            tx,
                            r,
                            REVIEWER_ROLE_REQUIRED_CODE,
                            REVIEWER_ROLE_REQUIRED_MSG,
                        )
                        .await;
                    }
                }
                _ => return Ok(reject_denied_preauth(r, SESSION_REVIEW_ACTING_DENIED)),
            }
            (*acting).clone()
        }
    };

    // (3) Resolve + classify the proposal (under the write lock). One reject path serves reviewer-reject and
    // proposer-withdraw; `resolved_by` records the acting principal either way.
    match resolve_proposal(tx, r.ws, r.skill, r.commit, r.expected).await? {
        Some((id, ProposalStatus::Open)) => {
            set_proposal_status(
                tx,
                r.ws,
                &id,
                ProposalStatus::Rejected,
                &acting,
                r.reason,
                r.created_at,
            )
            .await?;
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

// --- tx-bound + pool SQL helpers (every one workspace-scoped) ---

struct DeviceRecord {
    public_key: [u8; 32],
    principal: Principal,
    revoked: bool,
}

async fn resolve_device(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    device_key_id: &str,
) -> Result<Option<DeviceRecord>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT public_key AS "public_key!: Vec<u8>", principal AS "principal!", revoked AS "revoked!: i64"
           FROM device_registry WHERE workspace_id = $1 AND device_key_id = $2"#,
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

async fn read_review_required(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<bool> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT review_required AS "review_required!: i64" FROM workspace_policy WHERE workspace_id = $1"#,
        ws_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some_and(|r| r.review_required != 0))
}

pub(super) struct CurrentRow {
    pub(super) commit: CommitId,
    pub(super) generation: Generation,
}

async fn read_current(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
) -> Result<Option<CurrentRow>> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let row = sqlx::query!(
        r#"SELECT commit_id AS "commit_id!: Vec<u8>", epoch AS "epoch!: i64", seq AS "seq!: i64"
           FROM current WHERE workspace_id = $1 AND skill_id = $2"#,
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

/// Whether EVERY given object is `present` and not tombstoned — one set-valued query (`ANY` array bind)
/// counting the passing subset against the (distinct) candidate set, replacing a per-object round-trip.
async fn all_present_not_tombstoned(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    object_ids: &[ObjectId],
) -> Result<bool> {
    let ws_s = ws.as_str();
    let oids: Vec<Vec<u8>> = object_ids.iter().map(|o| o.0.to_vec()).collect();
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM object_presence op
           WHERE op.workspace_id = $1 AND op.status = 'present' AND op.object_id = ANY($2)
             AND NOT EXISTS (SELECT 1 FROM tombstones t
                             WHERE t.workspace_id = op.workspace_id AND t.blob_id = op.object_id)"#,
        ws_s,
        &oids,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // The candidate sets are distinct by construction (`distinct_object_ids` / the `commit_object` and
    // `proposal_object` primary keys), so an exact count match means every object passed.
    Ok(row.n == i64::try_from(object_ids.len()).unwrap_or(i64::MAX))
}

async fn lease_committed_commit(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &str,
) -> Result<Option<[u8; 32]>> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT commit_id AS "commit_id!: Vec<u8>" FROM promotion_lease
           WHERE workspace_id = $1 AND op_id = $2 AND expires_at IS NULL"#,
        ws_s,
        op_id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    row.map(|r| blob32(&r.commit_id)).transpose()
}

async fn commit_owner(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    commit: CommitId,
) -> Result<Option<SkillId>> {
    let ws_s = ws.as_str();
    let cid = commit.0.as_slice();
    let row = sqlx::query!(
        r#"SELECT skill_id AS "skill_id!" FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2"#,
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
    tx: &mut Transaction<'_, Postgres>,
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
        "INSERT INTO skill_commit (workspace_id, commit_id, skill_id, bundle_digest) VALUES ($1, $2, $3, $4) \
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
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    commit: CommitId,
    object_id: ObjectId,
) -> Result<()> {
    let ws_s = ws.as_str();
    let cid = commit.0.as_slice();
    let oid = object_id.0.as_slice();
    sqlx::query!(
        "INSERT INTO commit_object (workspace_id, commit_id, object_id) VALUES ($1, $2, $3) \
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

#[allow(clippy::too_many_arguments)]
async fn upsert_current(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    generation: Generation,
    signed_record: &[u8],
    display_name: Option<&str>,
    updated_at: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let cid = commit.0.as_slice();
    let epoch = u64_to_i64(generation.epoch)?;
    let seq = u64_to_i64(generation.seq)?;
    // `display_name` is UNSIGNED advisory metadata (never in the pointer preimage or the digest). On the
    // update path it is LAST-WRITER-WINS among writers that express a name: `COALESCE(excluded, existing)`
    // keeps the current name when this move carries none (a revert / approve / a name-less publish), so a
    // pointer move never blanks a name it didn't mean to touch.
    sqlx::query!(
        "INSERT INTO current (workspace_id, skill_id, commit_id, epoch, seq, signed_record, updated_at, display_name) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (workspace_id, skill_id) DO UPDATE SET \
           commit_id = excluded.commit_id, epoch = excluded.epoch, seq = excluded.seq, \
           signed_record = excluded.signed_record, updated_at = excluded.updated_at, \
           display_name = COALESCE(excluded.display_name, current.display_name)",
        ws_s,
        skill_s,
        cid,
        epoch,
        seq,
        signed_record,
        updated_at,
        display_name,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

pub(super) async fn delete_lease(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &str,
) -> Result<()> {
    let ws_s = ws.as_str();
    sqlx::query!(
        "DELETE FROM promotion_lease WHERE workspace_id = $1 AND op_id = $2",
        ws_s,
        op_id,
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
        // The signed-`current` record is a WIRE shape (it rides the read route + the OK receipt).
        schema_version: WIRE_SCHEMA_VERSION,
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
// (`blob32` lives once in `super` — the shared `mod db` helper.)

pub(super) fn i64_to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

pub(super) fn u64_to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

#[derive(Debug, thiserror::Error)]
#[error("a stored generation is out of the safe-integer range")]
struct GenerationOutOfRange;

#[derive(Debug, thiserror::Error)]
#[error("a propose reached the open step with no recorded base parent")]
struct ProposeWithoutBase;

#[derive(Debug, thiserror::Error)]
#[error("review --reject must not be promoted through the pointer-move transaction")]
struct RejectNotPromotable;

#[derive(Debug, thiserror::Error)]
#[error("a session actor reached a non-review op (an internal mis-route, not a request)")]
struct SessionOpNotReviewable;
