//! Custody's read authorizations — the REACH half of the gate/reach split, composed with the
//! directory's principal gate through the access-witness seam.
//!
//! Each authorization runs the witness's [`read_gate`](AccessWitness::read_gate) (WHO may ask — the
//! ONE membership predicate: a CONFIRMED `workspace_member` row, shared by the device and session
//! lanes) and then ONE principal-free reachability statement over custody's own tables (what
//! the skill makes readable — the `skill_in_workspace` half: every statement binds `(workspace_id,
//! skill_id)`, so a skill outside the workspace reaches nothing).
//! The gate and the reach are two statements — a principal removed between them completes one in-flight
//! read, the same accepted window as the authorize-then-fetch TOCTOU
//! [`crate::custody::read::read_object`] already re-guards (and re-runs on a miss).

use crate::db::custody::witness::AccessWitness;
use crate::db::{Db, blob32};
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

impl Db {
    /// The object-read authorization: the membership gate, then the principal-free reachability
    /// witness ([`Self::object_witness`]). Returns the **witness** commit id iff the gate admits the
    /// principal AND the skill makes the object readable. An empty result is the single
    /// not-entitled/not-found signal (gate-denied, skill-doesn't-reach, and object-nonexistent are
    /// indistinguishable).
    pub(crate) async fn authorize_object_read(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        object_id: ObjectId,
    ) -> Result<Option<CommitId>> {
        if !self.read_gate(ws, principal).await? {
            return Ok(None);
        }
        self.object_witness(ws, skill, object_id).await
    }

    /// The principal-free object-reachability witness — the reach half of the split (the lane gate has
    /// already admitted the caller). Two disjoint arms over the SAME (workspace-bound, skill-scoped)
    /// envelope:
    /// - **trunk**: `∃ c: skill_commit(w,s,c) ∧ commit_object(w,c,object_id)` — any accepted version of
    ///   the skill reaches the object.
    /// - **proposal**: `∃ p: proposal_object(w,p,object_id) ∧ p.skill=s ∧ p.status='open' ∧ p.base ==
    ///   current(w,s)` — an OPEN, NON-STALE proposal of the skill roots the object. This arm shares its
    ///   `open ∧ non-stale` predicate **verbatim** with the two GC keep-checks
    ///   ([`claim_for_delete`](Self::claim_for_delete) / [`claim_stale_for_recovery`](Self::claim_stale_for_recovery)),
    ///   so a reclaimed object is never still readable and a readable object is never reclaimed — the
    ///   keep-set == read-authorization invariant holds for pending proposals exactly as it does for the
    ///   trunk. The predicate is duplicated, not shared as one SQL string (`query!` cannot compose a literal,
    ///   and the bind-parameter numbering differs per call site); there are **FIVE** verbatim copies of
    ///   `open ∧ base == current` — this witness's proposal arm, [`Self::version_readable`]'s proposal arm,
    ///   the two GC keep-checks ([`Self::claim_for_delete`] /
    ///   [`Self::claim_stale_for_recovery`]), and the proposals listing
    ///   ([`Self::open_proposal_rows`]) — and a dedicated equivalence test pins the three
    ///   object-keyed copies (this arm + the two GC keep-checks) together against drift, while behavioral
    ///   tests pin the version-read and the listing copies to the same staleness semantics. A reclaimed object
    ///   that briefly outlives this check on a concurrent read is handled by
    ///   [`crate::custody::read::read_object`]'s re-authorize-on-miss guard (404, never Integrity).
    ///
    /// Every table is bound on `workspace_id`, so no fact can cross a tenant.
    async fn object_witness(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        object_id: ObjectId,
    ) -> Result<Option<CommitId>> {
        let ws = ws.as_str();
        let skill = skill.as_str();
        let object = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"
            SELECT w.commit_id AS "commit_id!: Vec<u8>" FROM (
                SELECT sc.commit_id AS commit_id
                FROM skill_commit  sc
                JOIN commit_object co ON co.workspace_id = sc.workspace_id AND co.commit_id = sc.commit_id
                WHERE sc.workspace_id = $1 AND sc.skill_id = $2 AND co.object_id = $3
              UNION ALL
                SELECT p.commit_id AS commit_id
                FROM proposal_object po
                JOIN proposals p  ON p.workspace_id = po.workspace_id AND p.id = po.proposal_id
                JOIN current    c  ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                WHERE po.workspace_id = $1 AND po.object_id = $3 AND p.skill_id = $2
                  AND p.status = 'open' AND c.epoch = p.base_epoch AND c.seq = p.base_seq
            ) w
            LIMIT 1
            "#,
            ws,
            skill,
            object,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| commit_id_from_row(&r.commit_id)).transpose()
    }

    /// Gather the owning skill of each given commit id that has provenance in this workspace (absent
    /// ids — no provenance in any skill — are simply not returned). The cross-skill lineage predicate
    /// turns this into its membership facts; keeping the read here keeps `sqlx` out of that pure logic.
    pub(crate) async fn commit_owners(
        &self,
        ws: &WorkspaceId,
        commit_ids: &[CommitId],
    ) -> Result<Vec<(CommitId, SkillId)>> {
        let ws_s = ws.as_str();
        let mut out = Vec::new();
        // One bound lookup per id (the candidate-and-parents set is tiny). A per-id `query!` keeps
        // compile-time checking and the offline metadata; a dynamic `IN (..)` list would forfeit both.
        for &id in commit_ids {
            let cid = id.0.as_slice();
            let row = sqlx::query!(
                r#"SELECT skill_id AS "skill_id!" FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2"#,
                ws_s,
                cid,
            )
            .fetch_optional(self.pool())
            .await
            .map_err(AuthorityError::internal)?;
            if let Some(row) = row {
                // A stored skill_id is always pre-validated on the way in, so a re-parse failure here is
                // store corruption — map it to an integrity fault, not the boundary `InvalidId` (mirroring
                // `commit_id_from_row`'s handling of a bad-width BLOB).
                let skill = SkillId::parse(&row.skill_id).map_err(AuthorityError::integrity)?;
                out.push((id, skill));
            }
        }
        Ok(out)
    }

    /// The version-read authorization — the R1 gate the version-metadata route runs, mirroring
    /// [`Self::authorize_object_read`]'s gate/reach split but anchored on a VERSION (`commit_id`) rather
    /// than an object: the membership gate, then the principal-free
    /// [`Self::version_readable`]. `false` collapses gate-denied and not-reachable into the caller's one
    /// indistinguishable not-found.
    pub(crate) async fn authorize_version_read(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        version_id: CommitId,
    ) -> Result<bool> {
        if !self.read_gate(ws, principal).await? {
            return Ok(false);
        }
        self.version_readable(ws, skill, version_id).await
    }

    /// The principal-free version-reachability test — the reach half of the version read (the lane gate
    /// has already admitted the caller). `true` iff the version is readable through EITHER:
    /// - **trunk**: the version is owned by the skill (`skill_commit`) AND has ≥1 `commit_object` edge — the
    ///   accepted-trunk test (every accepted version roots ≥1 object, so a non-empty edge set is exact), OR
    /// - **proposal**: an OPEN, NON-STALE proposal of the skill whose `commit_id` is this version. This arm
    ///   reuses the SAME `status='open' ∧ (base_epoch, base_seq) == current.(epoch, seq)` staleness predicate
    ///   the object-reach arm ([`Self::object_witness`]) and the two GC keep-checks
    ///   ([`Self::claim_for_delete`] / [`Self::claim_stale_for_recovery`]) use — here anchored on
    ///   `proposals.commit_id`, not `proposal_object.object_id` (the bind shape differs, so it is the 4th copy
    ///   of the literal — the proposals listing [`Self::open_proposal_rows`] is the 5th — not a shared
    ///   string; the behavioral proposal-version tests pin it to the same
    ///   staleness semantics). It deliberately does NOT authorize on bare `skill_commit`, which also names
    ///   unaccepted/rejected proposal candidates — that would leak a never-accepted version's metadata (the
    ///   `commit_object` ≥1-edge join is load-bearing; a rejected-candidate-404 test pins it).
    ///
    /// Every table is bound on `workspace_id`, so no fact can cross a tenant.
    async fn version_readable(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        version_id: CommitId,
    ) -> Result<bool> {
        let ws = ws.as_str();
        let skill = skill.as_str();
        let cid = version_id.0.as_slice();
        let row = sqlx::query!(
            r#"
            SELECT 1::int8 AS "ok!: i64" FROM (
                SELECT 1 AS ok
                FROM skill_commit  sc
                JOIN commit_object co ON co.workspace_id = sc.workspace_id AND co.commit_id = sc.commit_id
                WHERE sc.workspace_id = $1 AND sc.skill_id = $2 AND sc.commit_id = $3
              UNION ALL
                SELECT 1 AS ok
                FROM proposals p
                JOIN current   c ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                WHERE p.workspace_id = $1 AND p.skill_id = $2
                  AND p.commit_id = $3 AND p.status = 'open'
                  AND c.epoch = p.base_epoch AND c.seq = p.base_seq
            ) w
            LIMIT 1
            "#,
            ws,
            skill,
            cid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }
}

/// Convert a stored 32-byte BLOB into a [`CommitId`] via the shared [`blob32`].
pub(in crate::db) fn commit_id_from_row(bytes: &[u8]) -> Result<CommitId> {
    Ok(CommitId(blob32(bytes)?))
}
