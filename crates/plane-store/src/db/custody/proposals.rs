//! The contribute authority's proposal + approval SQL — the raw-`sqlx` half of `publish --propose` and
//! `review --approve | --reject`. A child of `mod db`; every method/function takes the validated id
//! newtypes + data and returns plain domain values, so no `sqlx` type crosses the module boundary.
//!
//! A proposal roots its candidate's bytes through [`proposal_object`](super) — NOT `commit_object`, which
//! means "accepted trunk" — gated for BOTH retention and read on the derived `open AND base == current`
//! predicate (see [`crate::db::Db::authorize_object_read`] / [`crate::db::Db::claim_for_delete`]). `review --approve`
//! performs the handoff to `commit_object` inside the one promotion transaction; `review --reject` only flips
//! the stored status, after which the gate stops matching and ordinary GC reclaims the unique objects.

use sqlx::{Postgres, Transaction};
use topos_types::Generation;

use crate::db::{Db, ReadLane, blob32};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

/// A proposal's stored lifecycle state (`stale` is never stored — it is derived from `open` + the live
/// `current` generation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProposalStatus {
    Open,
    Accepted,
    Rejected,
}

impl ProposalStatus {
    /// The stored string form (matches the `proposals.status` CHECK constraint).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProposalStatus::Open => "open",
            ProposalStatus::Accepted => "accepted",
            ProposalStatus::Rejected => "rejected",
        }
    }
}

/// The server-trusted, IMMUTABLE inputs a `review --approve` needs to build its promotion — derived from the
/// proposal's recorded state, never from the (lossy) git commit-parent walk. The object set is exactly what
/// the proposal rooted (so availability + the `commit_object` handoff cover every object), and the base
/// commit is the candidate's first parent (the authoritative first-parent source the txn re-asserts).
#[derive(Debug, Clone)]
pub(crate) struct ProposalApproveInputs {
    pub base_commit: CommitId,
    pub object_ids: Vec<ObjectId>,
}

/// An OPEN proposal located under the write lock (the in-transaction authoritative resolution).
#[derive(Debug, Clone)]
pub(crate) struct OpenProposal {
    pub id: String,
    pub proposer: Principal,
}

/// One OPEN, non-stale proposal as the proposals-listing read returns it — the candidate `commit` (the
/// `@hash`), the `base` generation it was opened against, and when. NO proposer, NO objects: the listing is
/// a thin, low-disclosure read (the bytes ride the per-blob object route; the proposer/audit stays internal).
#[derive(Debug, Clone)]
pub(crate) struct OpenProposalRow {
    pub commit: CommitId,
    pub base: Generation,
    pub created_at: String,
}

impl Db {
    /// Resolve the IMMUTABLE promote inputs (`base_commit` + the rooted object set) for the proposal of
    /// `(ws, skill, commit, base)` — preferring an `open` row but accepting any status (the base commit and
    /// object set are content-fixed per candidate, identical across a resubmit). A pool read run BEFORE the
    /// promotion transaction to build the approve's `PromoteInput`. `None` ⇒ no such proposal ever existed —
    /// a typed pre-transaction failure (there is nothing to approve); a STALE-but-present proposal still
    /// resolves here, and the in-transaction compare-and-set is what turns it into a `CONFLICT`.
    pub(crate) async fn proposal_approve_inputs(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        base: Generation,
    ) -> Result<Option<ProposalApproveInputs>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let cid = commit.0.as_slice();
        let base_epoch = u64_to_i64(base.epoch)?;
        let base_seq = u64_to_i64(base.seq)?;
        let row = sqlx::query!(
            r#"SELECT base_commit_id AS "base_commit_id!: Vec<u8>" FROM proposals
               WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = $3
                 AND base_epoch = $4 AND base_seq = $5
               ORDER BY (status = 'open') DESC LIMIT 1"#,
            ws_s,
            skill_s,
            cid,
            base_epoch,
            base_seq,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        let Some(row) = row else { return Ok(None) };
        let base_commit = CommitId(blob32(&row.base_commit_id)?);
        // The rooted object set: every object any proposal of this candidate+base rooted (one set, since the
        // candidate's tree fixes it). DISTINCT folds a resubmit's duplicate rows.
        let objects = sqlx::query!(
            r#"SELECT DISTINCT po.object_id AS "object_id!: Vec<u8>"
               FROM proposal_object po
               JOIN proposals p ON p.workspace_id = po.workspace_id AND p.id = po.proposal_id
               WHERE po.workspace_id = $1 AND p.skill_id = $2 AND p.commit_id = $3
                 AND p.base_epoch = $4 AND p.base_seq = $5"#,
            ws_s,
            skill_s,
            cid,
            base_epoch,
            base_seq,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        let object_ids = objects
            .into_iter()
            .map(|r| Ok(ObjectId(blob32(&r.object_id)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(ProposalApproveInputs {
            base_commit,
            object_ids,
        }))
    }

    /// Whether an OPEN proposal exists for `(ws, skill, commit, base)` (a pool read). The approve path uses it
    /// to classify a pre-transaction render fault: a fault while a still-open, non-stale proposal is the
    /// target is genuine corruption (a crash lost the bytes the gate was protecting); a fault while the
    /// proposal is no longer open (rejected/accepted) — and its unique bytes were therefore legitimately GC-
    /// reclaimed — is NOT corruption, so the transaction is left to produce the typed `DENIED`/`CONFLICT`.
    pub(crate) async fn open_proposal_exists(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        base: Generation,
    ) -> Result<bool> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let cid = commit.0.as_slice();
        let base_epoch = u64_to_i64(base.epoch)?;
        let base_seq = u64_to_i64(base.seq)?;
        let row = sqlx::query!(
            r#"SELECT 1::int8 AS "one!: i64" FROM proposals
               WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = $3
                 AND base_epoch = $4 AND base_seq = $5 AND status = 'open'"#,
            ws_s,
            skill_s,
            cid,
            base_epoch,
            base_seq,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }

    /// List the OPEN, non-stale proposals on `(ws, skill)` for a gate-admitted `principal` — the
    /// proposals-listing read, split gate/reach like the object/version authorizations: the lane's
    /// principal gate ([`crate::db::Db::read_gate`]), then the principal-free [`Self::open_proposal_rows`].
    /// The gate **is** the authorization, and a denial folds to an EMPTY list, never a not-found — a
    /// valid token whose principal is not on this skill's roster sees `[]`, exactly as before the split
    /// (the route's scope/path assert is the cross-skill guard; membership is silent).
    pub(crate) async fn list_open_proposals(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        lane: ReadLane,
    ) -> Result<Vec<OpenProposalRow>> {
        if !self.read_gate(ws, skill, principal, lane).await? {
            return Ok(Vec::new());
        }
        self.open_proposal_rows(ws, skill).await
    }

    /// The principal-free proposals listing — the reach half (the lane gate has already admitted the
    /// caller). ONE join over `proposals ⋈ current`, gated on the SAME `open ∧ base == current` staleness
    /// predicate the read-reachability statements ([`crate::db::Db::object_witness`] /
    /// [`crate::db::Db::version_readable`]) and both GC keep-checks
    /// ([`crate::db::Db::claim_for_delete`] / [`crate::db::Db::claim_stale_for_recovery`]) use — this is the 5th
    /// verbatim copy of that literal — so a staled proposal vanishes from the list exactly as it drops out
    /// of read + retention (**keep == read == list**). Every table is bound on `workspace_id`, so no fact
    /// crosses a tenant. Ordered by `(created_at, commit_id)` for a stable enumeration.
    pub(crate) async fn open_proposal_rows(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Vec<OpenProposalRow>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let rows = sqlx::query!(
            r#"
            SELECT p.commit_id  AS "commit_id!: Vec<u8>",
                   p.base_epoch AS "base_epoch!: i64",
                   p.base_seq   AS "base_seq!: i64",
                   p.created_at AS "created_at!"
            FROM proposals p
            JOIN current   c ON c.workspace_id = p.workspace_id AND c.skill_id = p.skill_id
            WHERE p.workspace_id = $1 AND p.skill_id = $2
              AND p.status = 'open' AND c.epoch = p.base_epoch AND c.seq = p.base_seq
            ORDER BY p.created_at, p.commit_id
            "#,
            ws_s,
            skill_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(OpenProposalRow {
                    commit: CommitId(blob32(&r.commit_id)?),
                    base: Generation {
                        epoch: i64_to_u64(r.base_epoch)?,
                        seq: i64_to_u64(r.base_seq)?,
                    },
                    created_at: r.created_at,
                })
            })
            .collect()
    }
}

/// One proposal's stored facts, as the session detail read discloses them to a confirmed member —
/// including the `proposer` (deliberately withheld from the thin `/v1` listing; the session lane
/// discloses it for the four-eyes surface) and the resolution columns.
#[derive(Debug, Clone)]
pub(crate) struct ProposalDetailRow {
    pub status: ProposalStatus,
    pub base: Generation,
    pub created_at: String,
    pub proposer: String,
    pub resolved_by: Option<String>,
    pub resolved_reason: Option<String>,
    pub resolved_at: Option<String>,
}

impl Db {
    /// The ONE row the detail read shows for a candidate: two rows CAN coexist for one `(skill, commit)`
    /// (the partial-unique is per base — a rejected attempt plus a re-propose on a newer base), so the
    /// preference order is load-bearing — an OPEN row must never be shadowed by any terminal row
    /// regardless of timestamps: open > accepted > latest rejected, tie-break latest `created_at`.
    pub(crate) async fn read_proposal_detail(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
    ) -> Result<Option<ProposalDetailRow>> {
        let ws_s = ws.as_str();
        let skill_s = skill.as_str();
        let cid = commit.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT status AS "status!", base_epoch AS "base_epoch!: i64",
                      base_seq AS "base_seq!: i64", created_at AS "created_at!",
                      proposer AS "proposer!", resolved_by AS "resolved_by?",
                      resolved_reason AS "resolved_reason?", resolved_at AS "resolved_at?"
               FROM proposals
               WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = $3
               ORDER BY CASE status WHEN 'open' THEN 0 WHEN 'accepted' THEN 1 ELSE 2 END,
                        created_at DESC
               LIMIT 1"#,
            ws_s,
            skill_s,
            cid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(ProposalDetailRow {
                status: parse_status(&r.status)?,
                base: Generation {
                    epoch: i64_to_u64(r.base_epoch)?,
                    seq: i64_to_u64(r.base_seq)?,
                },
                created_at: r.created_at,
                proposer: r.proposer,
                resolved_by: r.resolved_by,
                resolved_reason: r.resolved_reason,
                resolved_at: r.resolved_at,
            })),
        }
    }
}

/// Insert a fresh `open` proposal (provenance — `skill_commit` — must already be written: the foreign key).
/// `id` IS the opening op_id. `base` is recorded as the candidate's base generation (born non-stale, since
/// the caller proved `current.es == base` via the compare-and-set just above).
#[allow(clippy::too_many_arguments)]
pub(super) async fn insert_proposal(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    id: &str,
    skill: &SkillId,
    commit: CommitId,
    base_commit: CommitId,
    base: Generation,
    proposer: &Principal,
    created_at: &str,
) -> Result<()> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let cid = commit.0.as_slice();
    let base_cid = base_commit.0.as_slice();
    let base_epoch = u64_to_i64(base.epoch)?;
    let base_seq = u64_to_i64(base.seq)?;
    let proposer_s = proposer.as_str();
    sqlx::query!(
        "INSERT INTO proposals \
           (workspace_id, id, skill_id, commit_id, base_commit_id, base_epoch, base_seq, status, proposer, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'open', $8, $9)",
        ws_s,
        id,
        skill_s,
        cid,
        base_cid,
        base_epoch,
        base_seq,
        proposer_s,
        created_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// Root one of a proposal's objects (the gated retention/read root; idempotent).
pub(super) async fn insert_proposal_object(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    proposal_id: &str,
    object_id: ObjectId,
) -> Result<()> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    sqlx::query!(
        "INSERT INTO proposal_object (workspace_id, proposal_id, object_id) VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, proposal_id, object_id) DO NOTHING",
        ws_s,
        proposal_id,
        oid,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// Whether a proposal row already exists under this op_id (the `proposals` PK is `(workspace_id, id=op_id)`).
/// `propose` checks this under the write lock before inserting: a SAME-device op_id retry replays at the
/// receipt layer before reaching here, so a hit means a DIFFERENT device minted the same op_id (a ~122-bit
/// UUIDv4 collision) — preempted as a typed, receipted terminal rather than a non-receipted PK-violation
/// `Internal`. (Widening the PK to include `device_key_id` would ripple into the FK + the triply-duplicated
/// retention predicate, so the workspace-unique op_id is relied on instead, with this guard as the backstop.)
pub(super) async fn proposal_id_exists(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    id: &str,
) -> Result<bool> {
    let ws_s = ws.as_str();
    let row = sqlx::query!(
        r#"SELECT 1::int8 AS "one!: i64" FROM proposals WHERE workspace_id = $1 AND id = $2"#,
        ws_s,
        id,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some())
}

/// Read the OPEN proposal for `(ws, skill, commit, base)` and LOCK it (`SELECT … FOR UPDATE`) for the rest
/// of the transaction — so a concurrent approve/reject of the same proposal serializes on this row rather
/// than on an incidental write-write collision. This is the faithful Postgres realization of the write lock
/// SQLite's `BEGIN IMMEDIATE` gave for free; the SERIALIZABLE runner additionally covers the absent-row
/// phantom a row lock cannot (a concurrent INSERT of an open proposal this read didn't see). The
/// partial-unique index makes this at most one row. `None` ⇒ no open proposal (it was accepted/rejected, or
/// never existed) — the approve/reject arm turns that into a typed `CONFLICT`/`DENIED`/idempotent outcome.
pub(super) async fn read_open_proposal(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    base: Generation,
) -> Result<Option<OpenProposal>> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let cid = commit.0.as_slice();
    let base_epoch = u64_to_i64(base.epoch)?;
    let base_seq = u64_to_i64(base.seq)?;
    let row = sqlx::query!(
        r#"SELECT id AS "id!", proposer AS "proposer!" FROM proposals
           WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = $3
             AND base_epoch = $4 AND base_seq = $5 AND status = 'open'
           FOR UPDATE"#,
        ws_s,
        skill_s,
        cid,
        base_epoch,
        base_seq,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(OpenProposal {
            id: r.id,
            proposer: Principal::parse(&r.proposer).map_err(AuthorityError::integrity)?,
        })),
    }
}

/// Resolve the proposal of `(ws, skill, commit, base)` and LOCK the resolved row (`SELECT … FOR UPDATE`),
/// preferring `open`, then `accepted`, then `rejected` — so the reject/withdraw path can classify its
/// target: reject an `open` one, refuse an `accepted` one, treat an already-`rejected` one as an idempotent
/// no-op. The `FOR UPDATE` serializes a concurrent approve-vs-reject on the same row (reject never touches
/// `current`, so it cannot rely on the pointer CAS); the SERIALIZABLE runner covers the phantom case. `None`
/// ⇒ no proposal of this candidate+base ever existed (nothing to reject). (Once a candidate is accepted,
/// `current` advances past `base`, so no NEW proposal can open at the same base — at most one open-or-accepted
/// row coexists with any number of prior rejected resubmits, which this ordering resolves unambiguously.)
pub(super) async fn resolve_proposal(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
    commit: CommitId,
    base: Generation,
) -> Result<Option<(String, ProposalStatus)>> {
    let ws_s = ws.as_str();
    let skill_s = skill.as_str();
    let cid = commit.0.as_slice();
    let base_epoch = u64_to_i64(base.epoch)?;
    let base_seq = u64_to_i64(base.seq)?;
    let row = sqlx::query!(
        r#"SELECT id AS "id!", status AS "status!" FROM proposals
           WHERE workspace_id = $1 AND skill_id = $2 AND commit_id = $3
             AND base_epoch = $4 AND base_seq = $5
           ORDER BY (status = 'open') DESC, (status = 'accepted') DESC LIMIT 1
           FOR UPDATE"#,
        ws_s,
        skill_s,
        cid,
        base_epoch,
        base_seq,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some((r.id, parse_status(&r.status)?))),
    }
}

/// Transition a proposal's stored status (`open → accepted | rejected`), stamping the resolving principal,
/// the resolution time, and — on a session reject — the mandatory reason (`None` everywhere else: an
/// accept has no reason field, and device rejects deliberately carry none).
pub(super) async fn set_proposal_status(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    id: &str,
    status: ProposalStatus,
    resolved_by: &Principal,
    resolved_reason: Option<&str>,
    resolved_at: &str,
) -> Result<()> {
    let ws_s = ws.as_str();
    let status_s = status.as_str();
    let resolver = resolved_by.as_str();
    sqlx::query!(
        "UPDATE proposals SET status = $3, resolved_by = $4, resolved_reason = $5, resolved_at = $6 \
         WHERE workspace_id = $1 AND id = $2",
        ws_s,
        id,
        status_s,
        resolver,
        resolved_reason,
        resolved_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// Record an approval audit row (idempotent under a replayed approve — one row per (candidate, base, reviewer)).
pub(super) async fn insert_approval(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    commit: CommitId,
    base: Generation,
    reviewer: &Principal,
    at: &str,
) -> Result<()> {
    let ws_s = ws.as_str();
    let cid = commit.0.as_slice();
    let base_epoch = u64_to_i64(base.epoch)?;
    let base_seq = u64_to_i64(base.seq)?;
    let reviewer_s = reviewer.as_str();
    sqlx::query!(
        "INSERT INTO approvals (workspace_id, commit_id, base_epoch, base_seq, reviewer, at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (workspace_id, commit_id, base_epoch, base_seq, reviewer) DO NOTHING",
        ws_s,
        cid,
        base_epoch,
        base_seq,
        reviewer_s,
        at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

fn parse_status(s: &str) -> Result<ProposalStatus> {
    match s {
        "open" => Ok(ProposalStatus::Open),
        "accepted" => Ok(ProposalStatus::Accepted),
        "rejected" => Ok(ProposalStatus::Rejected),
        _ => Err(AuthorityError::integrity(BadProposalStatus)),
    }
}

fn u64_to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

fn i64_to_u64(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| AuthorityError::integrity(GenerationOutOfRange))
}

#[derive(Debug, thiserror::Error)]
#[error("a stored proposal status is not a known value")]
struct BadProposalStatus;

#[derive(Debug, thiserror::Error)]
#[error("a stored generation is out of the safe-integer range")]
struct GenerationOutOfRange;
