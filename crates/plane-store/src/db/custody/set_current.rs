//! The one pointer-move transaction — the raw-SQL half of `set-current`.
//!
//! One `SERIALIZABLE` (`run_serializable!`) write transaction advances a skill's `current` pointer by exactly one step, under
//! a compare-and-set on the whole `(epoch, seq)` pair, re-roots the migrated bytes, and writes a durable
//! all-outcome receipt — **with no filesystem op inside the transaction**. The ordered sub-steps (and why
//! each ordering is load-bearing) are in [`run`]. All `sqlx` stays here; the caller
//! ([`crate::custody::set_current`]) hands in server-trusted values and gets back a domain
//! [`SetCurrentReceipt`]. Every access fact (the credential resolution, the membership/role gates, the
//! review policy)
//! enters through the [`AccessWitness`] seam — read INSIDE this transaction, so a directory row committed
//! before it (a revoke, a membership removal) is serialized ahead and decides the outcome. The receipt
//! persistence/replay machinery + the terminal-outcome writers `run` and `reject_run` call live in
//! [`super::receipts`]; this file keeps the ordered state machine itself.

use sqlx::{Postgres, Transaction};
use topos_types::{
    CurrentRecord, Generation, PointerScope, TerminalOutcome, WIRE_SCHEMA_VERSION,
    WireCurrentRecord,
};

use super::witness::{
    AccessWitness, ActorRole, GenesisRegistration, PlacementDecision, SessionWriteGate, SkillGate,
};
use crate::actor::{
    REVIEWER_ROLE_REQUIRED_CODE, REVIEWER_ROLE_REQUIRED_MSG, SESSION_REVIEW_ACTING_DENIED,
    WriteActor,
};
use crate::custody::set_current::{DeviceOp, PromoteInput, RejectInput, SetCurrentReceipt};
use crate::db::custody::proposals::{
    ProposalStatus, insert_approval, insert_proposal, insert_proposal_object, proposal_id_exists,
    proposal_proposer, read_open_proposal, resolve_proposal, set_proposal_status,
};
use crate::db::custody::receipts::{
    BoundIdentity, Replay, StoredReceipt, conflict, denied, denied_code, denied_preauth,
    first_parent_mismatch, insert_receipt, permanent, permanent_key_reuse, reject_denied,
    reject_denied_code, reject_denied_preauth, reject_terminal, replay, retryable,
};
use crate::db::{Db, blob32};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

/// The I-JSON safe-integer bound (2^53 − 1) the wire record enforces — a generation a JSON consumer
/// (the web app, an agent) could not represent exactly is never stored or served.
const MAX_SAFE_INT: u64 = (1u64 << 53) - 1;

impl Db {
    /// Run the one pointer-move transaction. Commits on a terminal outcome (the receipt — and, for `OK`,
    /// the pointer/provenance — persist together); rolls back only on an internal/integrity fault.
    pub(crate) async fn set_current_txn(
        &self,
        input: PromoteInput<'_>,
    ) -> Result<SetCurrentReceipt> {
        run_serializable!(self, tx, run(&mut tx, &input, self).await)
    }

    /// The standalone `review --reject` / proposer-withdraw transaction. NOT a pointer move — it never enters
    /// [`run`]: `current` is untouched, there is no lease. One `SERIALIZABLE` transaction mirrors the
    /// promotion's discipline where it overlaps — receipt-replay first, then in-transaction authorization
    /// (the SAME witness lookups, `op = ReviewReject`) — then resolves the proposal and classifies it: `open` ⇒
    /// flip to `rejected`; already `rejected` ⇒ idempotent OK (a lost-ack retry under a different op_id); and
    /// `accepted` or absent ⇒ a typed DENIED. One path serves both reviewer-reject and proposer-withdraw;
    /// `resolved_by` records who.
    pub(crate) async fn review_reject_txn(&self, r: RejectInput<'_>) -> Result<SetCurrentReceipt> {
        run_serializable!(self, tx, reject_run(&mut tx, &r, self).await)
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

    /// Read back the stored `current` record (the serialized [`WireCurrentRecord`] document) for a skill —
    /// what a follower's pointer fetch returns. `None` until the pointer has first been moved.
    pub(crate) async fn read_current_record(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Option<Vec<u8>>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let row = sqlx::query!(
            r#"SELECT record AS "record?: Vec<u8>" FROM current
               WHERE workspace_id = $1 AND skill_id = $2"#,
            ws_s,
            skill_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.and_then(|r| r.record))
    }
}

/// The ordered sub-steps of the one transaction. Each ordering is load-bearing. The device lane's
/// credential resolution (step 0) runs first — an unauthenticated caller reaches nothing — but its
/// REVOKED check is deferred into authz, and replay runs between them, so a since-revoked device
/// still gets its stored OK on retry. Authz runs before the CAS, so an unauthorized
/// caller never learns the live generation from a CONFLICT. The CAS runs before availability and lineage, so
/// a stale base returns CONFLICT-rebase, never a confusing DENIED for GC-reclaimed objects. Provenance and
/// reachability are written before the pointer advance (the `current` to `skill_commit` foreign key is
/// immediate) and before the lease release, so the GC keep-set — any `commit_object` edge, or a live lease —
/// covers the objects continuously across the re-root, with no reclaim window.
async fn run(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    witness: &impl AccessWitness,
) -> Result<SetCurrentReceipt> {
    let bound = BoundIdentity {
        command: crate::set_current::device_op_command(input.op),
        skill_id: input.skill.as_str(),
        commit: Some(input.candidate_commit),
        bundle_digest: Some(input.candidate_bundle_digest),
        expected: input.expected,
    };

    // (0) Device-lane AUTHENTICATION — before any probe or durable write: re-resolve the presented
    // workspace credential against the live registry row INSIDE this transaction (the pool
    // pre-resolution that keyed the pre-txn machinery is advisory; this read is the authority) and
    // require it to name exactly the pre-resolved device. An unknown/rotated-away credential is
    // DENIED without a durable receipt (`denied_preauth` — an unauthenticated caller must not mint
    // rows). The `revoked` flag is deliberately NOT checked here — the replay probe below must still
    // serve a since-revoked device its stored OK; the authz arm (3) denies its fresh work.
    let device_identity = match &input.actor {
        WriteActor::Device {
            credential_sha256,
            device_key_id,
        } => match witness.device(tx, input.ws, credential_sha256).await? {
            Some(d) if d.device_key_id == *device_key_id => Some(d),
            _ => return denied_preauth(tx, input, &bound, "device unknown or revoked").await,
        },
        WriteActor::Session { .. } => None,
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
        Replay::Hit(receipt) => {
            // A concurrent DUPLICATE whose pre-txn stable replay (`replay_revert` / `replay_revert_session`)
            // missed the still-in-flight original may have re-staged a forward-commit/publish lease under
            // this op_id before the original's receipt became visible; the original already released its own
            // lease at its terminal writer, so this re-staged one must be released here too — otherwise it
            // strands, GC-rooting already-`commit_object`-rooted objects forever (codex design-gate finding 1).
            // INVARIANT this rests on: every Hit-replayable outcome either roots its objects elsewhere
            // (`commit_object` on OK/publish/revert, `proposal_object` on NEEDS_REVIEW) or legitimately
            // abandoned them (CONFLICT/DENIED released their lease already) — so the lease under this op_id is
            // never the sole root of a live version. A future op that rooted objects SOLELY via a lease
            // outliving its terminal writer would be silently unrooted here and must not reach this arm.
            // The delete is idempotent (a no-op if absent). An approve leases nothing — skip it, exactly as
            // the Mismatch arm below and every terminal writer do.
            if !matches!(input.op, DeviceOp::ReviewApprove) {
                delete_lease(tx, input.ws, input.op_id).await?;
            }
            return Ok(receipt);
        }
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

    // (2 + 3) Authorization — authoritative + in-txn, and the ONE place the transaction branches on
    // the lane. Both arms read `current` only after their authentication step, so an unauthorized
    // caller never learns the live generation; both hoist the acting principal the actor-blind tails
    // consume.
    //
    // ONE MEMBERSHIP PREDICATE, EVERY LANE: both arms gate on a CONFIRMED `workspace_member` seat —
    // the same directory join the read path runs (deleting the membership row kills writes AND reads
    // the moment it commits). The remaining lane asymmetry is ROLE: the session arm (a
    // review/revert surface) requires an owner|reviewer seat; the device arm takes any confirmed
    // member, with the ROLE BAND feeding the per-bundle protection gate below (a member's direct
    // publish on a reviewed bundle downgrades to a proposal; reviewer+ lands directly).
    let (acting, device_role, current) = match &input.actor {
        WriteActor::Device { .. } => {
            // Authenticated at step (0) — the in-transaction credential resolution, so a revoke or a
            // membership removal committed before this transaction is serialized ahead and decides it.
            let device = device_identity.clone().ok_or_else(|| {
                AuthorityError::internal(DeviceLaneUnresolved) // step (0) resolves every device-lane call
            })?;
            if device.revoked {
                return denied_preauth(tx, input, &bound, "device unknown or revoked").await;
            }
            let current = read_current(tx, input.ws, input.skill).await?;
            // THE MEMBERSHIP GATE: a confirmed workspace seat authorizes the write (the git/GitHub
            // model — push access is workspace-wide; protection + channel modes are the finer gates).
            let Some(role) = witness.member_role(tx, input.ws, &device.principal).await? else {
                return denied(
                    tx,
                    input,
                    &bound,
                    "principal is not a confirmed workspace member",
                )
                .await;
            };
            (device.principal, Some(role), current)
        }
        WriteActor::Session { acting, .. } => {
            // The public session API constructs ONLY a review approve or a revert (both promote to
            // `current` under the SAME owner|reviewer gate); any other op here is an internal mis-route, a
            // fault — never a receipt. Revert bypasses the review gate and four-eyes by design (it restores
            // already-consented bytes — the safety net); the op tail already routes it to `promote`, and
            // review-required fires only for a direct publish, so no extra branching is needed here.
            if !matches!(input.op, DeviceOp::ReviewApprove | DeviceOp::Revert) {
                return Err(AuthorityError::internal(SessionOpNotReviewable));
            }
            // The in-txn role gate (authoritative; the orchestration's pool-level pre-gate is the cheap
            // fence). The witness answers the whole role matrix: a non-member / merely-invited /
            // unknown-workspace caller gets the ONE uniform denial, synthesized — never persisted (the
            // session recording rule: a web-verified email proves nothing about THIS workspace, and a
            // durable row would let any account grow the ledger). A CONFIRMED plain member is entitled
            // to a recorded, replayable answer: the durable typed role denial.
            match witness.session_write_gate(tx, input.ws, acting).await? {
                SessionWriteGate::Authorized => {}
                SessionWriteGate::RoleDenied => {
                    return denied_code(
                        tx,
                        input,
                        &bound,
                        REVIEWER_ROLE_REQUIRED_CODE,
                        REVIEWER_ROLE_REQUIRED_MSG,
                    )
                    .await;
                }
                SessionWriteGate::Unproven => {
                    return denied_preauth(tx, input, &bound, SESSION_REVIEW_ACTING_DENIED).await;
                }
            }
            let current = read_current(tx, input.ws, input.skill).await?;
            ((*acting).clone(), None, current)
        }
    };

    // (3c) The catalog gate — the skill's lifecycle status + resolved protection, read inside the
    // txn (the per-bundle pin, else the workspace default: the cascade is the directory's). An
    // archived or deleted skill refuses EVERY pointer write, typed, before the CAS (a DENIED, never
    // a confusing CONFLICT against a frozen pointer). `Missing` is a genesis (or a pre-catalog
    // seeded pointer) — registered at step (6c) below, after every deny-returning gate has passed.
    let gate = witness.skill_gate(tx, input.ws, input.skill).await?;
    match gate {
        SkillGate::Archived => {
            return denied(tx, input, &bound, "the skill is archived").await;
        }
        SkillGate::Deleted => {
            return denied(tx, input, &bound, "the skill is deleted").await;
        }
        SkillGate::Missing { .. } | SkillGate::Active { .. } => {}
    }

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
    }

    // (6b) The protection gate — REROUTES instead of refusing (never reject an intent the system
    // can honor at a safer level): a DEVICE-lane direct publish or revert on an effectively-REVIEWED
    // bundle by a plain MEMBER runs the propose arm below and answers NEEDS_REVIEW with a
    // `downgraded` detail; reviewer+ lands directly (the protected-branch model). Genesis (no
    // `current`) always lands — a proposal against nothing is meaningless, and the role matrix gives
    // members brand-new skills. The session lane never downgrades (its owner|reviewer gate already
    // ran; the web revert is the safety net by design).
    let downgrade = matches!(input.op, DeviceOp::PublishDirect | DeviceOp::Revert)
        && current.is_some()
        && gate.reviewed()
        && matches!(device_role, Some(role) if !role.lands_on_reviewed());

    // (6c) The catalog + channel directory writes — every deny-returning gate above has passed, so
    // for a genesis the catalog row, the placement, the author's self-follow, and the pointer land
    // in ONE transaction, never an orphan. A publish that carries `--to` places its reference here
    // (gated by the CHANNEL's mode, independently of the version gate — the outcome rides the
    // receipt's details either way); the advisory display name is recorded last-writer-wins. This
    // block must stay the LAST work before the op tail: the tails write receipts, and a
    // receipted-terminal between here and them would commit these rows alongside a failure. (The
    // one reachable case — the propose arm's pathological op-id-collision permanent — commits an
    // idempotent placement row, which curation semantics make harmless: placements apply
    // immediately and independently of the version gate.)
    let mut extra_details = serde_json::Map::new();
    if matches!(input.op, DeviceOp::PublishDirect | DeviceOp::PublishPropose) {
        let placement = match gate {
            SkillGate::Missing { .. } => {
                match witness
                    .register_publish(
                        tx,
                        input.ws,
                        input.skill,
                        input.display_name,
                        &acting,
                        input.channel,
                        input.created_at,
                    )
                    .await?
                {
                    GenesisRegistration::Registered { placement, .. } => Some(placement),
                    GenesisRegistration::NameTaken { name } => {
                        return denied(
                            tx,
                            input,
                            &bound,
                            &format!("the skill name {name:?} is already taken in this workspace"),
                        )
                        .await;
                    }
                }
            }
            SkillGate::Active { .. } => {
                if let Some(dn) = input.display_name {
                    witness
                        .set_display_name(tx, input.ws, input.skill, dn)
                        .await?;
                }
                match input.channel {
                    Some(ch) => Some(
                        witness
                            .place_skill(tx, input.ws, input.skill, ch, &acting, input.created_at)
                            .await?,
                    ),
                    None => None,
                }
            }
            SkillGate::Archived | SkillGate::Deleted => None, // denied at (3c)
        };
        if let Some(p) = placement {
            let (key, channel) = match p {
                PlacementDecision::Placed { channel } => ("placed_channel", channel),
                PlacementDecision::Created { channel } => ("created_channel", channel),
                PlacementDecision::RoleDenied { channel } => ("placement_denied", channel),
                PlacementDecision::BadName { channel } => ("placement_bad_name", channel),
            };
            extra_details.insert(key.to_owned(), serde_json::Value::String(channel));
        }
    }
    if downgrade {
        extra_details.insert("downgraded".to_owned(), serde_json::Value::Bool(true));
    }
    let details = if extra_details.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(extra_details))
    };

    // (7) The op-specific tail — over the SAME shared body above (replay, authz, the catalog gate,
    // the whole-`(epoch, seq)` CAS, availability, lineage, the first-parent assert, all of which ran
    // for EVERY op). A direct publish and a revert PROMOTE — or, downgraded by the protection gate,
    // open a proposal; `--propose` opens one voluntarily; `review --approve` promotes the locked
    // proposal sideways through the SAME promote plus the status handoff. (`review --reject` never
    // reaches `run` — it is a standalone status-flip transaction.)
    match input.op {
        DeviceOp::PublishDirect | DeviceOp::Revert if downgrade => {
            propose_arm(tx, input, &bound, &acting, details).await
        }
        DeviceOp::PublishDirect | DeviceOp::Revert => {
            promote(tx, input, new_gen, &bound, details).await
        }
        DeviceOp::PublishPropose => propose_arm(tx, input, &bound, &acting, details).await,
        DeviceOp::ReviewApprove => {
            approve_arm(tx, input, new_gen, &bound, gate.reviewed(), &acting, witness).await
        }
        DeviceOp::ReviewReject => Err(AuthorityError::internal(RejectNotPromotable)),
    }
}

/// Record provenance + reachability, advance the pointer, and persist it with the durable OK receipt —
/// the shared pointer-advance for a direct publish, a revert, AND the accepted half of a proposal. The
/// `commit_object` edges it writes PERMANENTLY root the candidate's objects (the accepted-trunk root), so for
/// an approve this write IS the handoff from the proposal's gated `proposal_object` root to the trunk. Does
/// NOT touch the lease — the caller decides (publish/revert release theirs after this; approve has none).
async fn advance_current(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    new_gen: Generation,
    bound: &BoundIdentity<'_>,
    details: Option<serde_json::Value>,
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
    let record = serialize_record(input.ws, input.skill, input.candidate_commit, new_gen)?;
    upsert_current(
        tx,
        input.ws,
        input.skill,
        input.candidate_commit,
        new_gen,
        &record,
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
        record: Some(record),
        created_at: input.created_at.to_owned(),
        details,
    };
    insert_receipt(tx, input.ws, &input.actor.receipt_actor(), &stored).await?;
    Ok(stored.into_receipt())
}

/// A direct publish / revert: advance `current`, then release the lease — AFTER the edges root the objects,
/// so the GC keep-set never had a gap.
async fn promote(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    new_gen: Generation,
    bound: &BoundIdentity<'_>,
    details: Option<serde_json::Value>,
) -> Result<SetCurrentReceipt> {
    let receipt = advance_current(tx, input, new_gen, bound, details).await?;
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
    details: Option<serde_json::Value>,
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
        record: None,
        created_at: input.created_at.to_owned(),
        details,
    };
    insert_receipt(tx, input.ws, &input.actor.receipt_actor(), &stored).await?;
    delete_lease(tx, input.ws, input.op_id).await?;
    Ok(stored.into_receipt())
}

/// `review --approve`: promote the locked open proposal SIDEWAYS to `current`. The shared body already ran
/// the CAS (a stale base ⇒ CONFLICT *before* here), availability (against the shared `present`/tombstone
/// predicate, no lease gate), and the first-parent assert. Here: lock + assert the open proposal under the
/// write lock; enforce four-eyes on an effectively-REVIEWED bundle; record the approval; advance `current`
/// (whose `commit_object` write is the handoff from the gated `proposal_object` root to the permanent trunk
/// root); flip the proposal to `accepted`; notify the author (the verdict and its notice commit together).
/// No lease to release.
async fn approve_arm(
    tx: &mut Transaction<'_, Postgres>,
    input: &PromoteInput<'_>,
    new_gen: Generation,
    bound: &BoundIdentity<'_>,
    reviewed: bool,
    reviewer: &Principal,
    witness: &impl AccessWitness,
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
    // Four-eyes (the anti-poisoning gate): fires ONLY on an effectively-reviewed bundle (the
    // per-bundle pin, else the workspace default), where the gate's value needs a SECOND actor.
    // On an open bundle, a solo author may approve their own proposal (a deferred self-publish).
    if reviewed && reviewer.as_str() == proposal.proposer.as_str() {
        return denied(
            tx,
            input,
            bound,
            "the proposer may not approve their own proposal on a reviewed bundle",
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
    let receipt = advance_current(tx, input, new_gen, bound, None).await?;
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
    // The author's verdict notice — skipped for a self-approve (the actor already knows).
    if reviewer.as_str() != proposal.proposer.as_str() {
        witness
            .notify_verdict(
                tx,
                input.ws,
                input.skill,
                input.candidate_commit,
                &proposal.proposer,
                "accepted",
                None,
                reviewer,
                input.created_at,
            )
            .await?;
    }
    Ok(receipt)
}

// --- review --reject / proposer-withdraw (a standalone status-flip transaction, not a pointer move) ---

async fn reject_run(
    tx: &mut Transaction<'_, Postgres>,
    r: &RejectInput<'_>,
    witness: &impl AccessWitness,
) -> Result<SetCurrentReceipt> {
    let bound = BoundIdentity {
        command: crate::set_current::device_op_command(r.op),
        skill_id: r.skill.as_str(),
        commit: Some(r.commit),
        bundle_digest: Some(r.bundle_digest),
        expected: r.expected,
    };

    // (0) Device-lane AUTHENTICATION — the same step-(0) resolve `run` performs: re-resolve the
    // presented credential inside THIS transaction and require it to name the pre-resolved device.
    // An unknown/rotated credential is DENIED without a durable receipt (a reject has no lease, so
    // nothing is even released); `revoked` is deferred past the replay probe, as in `run`.
    let device_identity = match &r.actor {
        WriteActor::Device {
            credential_sha256,
            device_key_id,
        } => match witness.device(tx, r.ws, credential_sha256).await? {
            Some(d) if d.device_key_id == *device_key_id => Some(d),
            _ => return Ok(reject_denied_preauth(r, "device unknown or revoked")),
        },
        WriteActor::Session { .. } => None,
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

    // (2) Authorization — the reject twin of `run`'s one lane fork (one membership predicate on both
    // lanes; the asymmetry is ROLE alone: session rejects need an owner|reviewer seat).
    let acting: Principal = match &r.actor {
        WriteActor::Device { .. } => {
            // Authenticated at step (0); a revoke serialized ahead of this blocks the reject.
            let device = device_identity
                .clone()
                .ok_or_else(|| AuthorityError::internal(DeviceLaneUnresolved))?;
            if device.revoked {
                return Ok(reject_denied_preauth(r, "device unknown or revoked"));
            }
            if !witness
                .confirmed_member(tx, r.ws, &device.principal)
                .await?
            {
                return reject_denied(tx, r, "principal is not a confirmed workspace member").await;
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
            match witness.session_write_gate(tx, r.ws, acting).await? {
                SessionWriteGate::Authorized => {}
                SessionWriteGate::RoleDenied => {
                    return reject_denied_code(
                        tx,
                        r,
                        REVIEWER_ROLE_REQUIRED_CODE,
                        REVIEWER_ROLE_REQUIRED_MSG,
                    )
                    .await;
                }
                SessionWriteGate::Unproven => {
                    return Ok(reject_denied_preauth(r, SESSION_REVIEW_ACTING_DENIED));
                }
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
            // The author's verdict notice, committed with the verdict — skipped for a withdraw
            // (the proposer rejecting their own proposal already knows).
            if let Some(author) = proposal_proposer(tx, r.ws, &id).await?
                && author.as_str() != acting.as_str()
            {
                witness
                    .notify_verdict(
                        tx, r.ws, r.skill, r.commit, &author, "rejected", r.reason, &acting,
                        r.created_at,
                    )
                    .await?;
            }
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
        // Circumstantially closed (the skill archived / a version purged) — already resolved, with
        // its own recorded reason; a late reject must not overwrite that story.
        Some((_, ProposalStatus::Closed)) => {
            reject_denied(tx, r, "the proposal was closed when its skill left circulation").await
        }
        None => reject_denied(tx, r, "no open proposal for this candidate and base").await,
    }
}

// --- tx-bound + pool SQL helpers (every one workspace-scoped) ---

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

async fn upsert_current(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    generation: Generation,
    record: &[u8],
    updated_at: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let cid = commit.0.as_slice();
    let epoch = u64_to_i64(generation.epoch)?;
    let seq = u64_to_i64(generation.seq)?;
    // The advisory display name lives on the CATALOG row (recorded through the witness at the
    // directory-writes step); the pointer row carries only pointer facts.
    sqlx::query!(
        "INSERT INTO current (workspace_id, skill_id, commit_id, epoch, seq, record, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (workspace_id, skill_id) DO UPDATE SET \
           commit_id = excluded.commit_id, epoch = excluded.epoch, seq = excluded.seq, \
           record = excluded.record, updated_at = excluded.updated_at",
        ws_s,
        skill_s,
        cid,
        epoch,
        seq,
        record,
        updated_at,
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

/// Serialize the [`WireCurrentRecord`] document stored in `current.record` + the OK receipt — the
/// pointer read's wire body. Its authority is the database row it mirrors; its integrity story is the
/// content-addressed `version_id` a follower re-verifies by digest on every apply.
fn serialize_record(
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    generation: Generation,
) -> Result<Vec<u8>> {
    let record = WireCurrentRecord {
        // The `current` record is a WIRE shape (it rides the read route + the OK receipt).
        schema_version: WIRE_SCHEMA_VERSION,
        scope: PointerScope {
            workspace_id: ws.as_str().to_owned(),
            skill_id: skill.as_str().to_owned(),
        },
        record: CurrentRecord {
            version_id: topos_core::digest::to_hex(&commit.0),
            generation,
        },
    };
    serde_json::to_vec(&record).map_err(AuthorityError::internal)
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

#[derive(Debug, thiserror::Error)]
#[error(
    "a device-lane authz arm ran without the step-(0) credential resolution (an internal fault)"
)]
struct DeviceLaneUnresolved;
