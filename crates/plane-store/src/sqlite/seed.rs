//! Test-only staging helpers — `#[cfg(test)] pub(crate)`, **never public.**
//!
//! These insert roster / provenance / pointer rows directly so the access-port, lineage, and
//! isolation tests can stage state without the (deferred) pointer-move write. They are never `pub` —
//! a public seed would let any in-process linker grant itself read entitlement, the exact hole the
//! privacy wall closes — and `#[cfg(test)]` keeps them out of every release artifact.

use super::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};

impl Db {
    /// Stage a roster membership (the principal becomes entitled to read/upload the skill).
    pub(crate) async fn seed_roster(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()> {
        let (ws, skill, principal) = (ws.as_str(), skill.as_str(), principal.as_str());
        sqlx::query!(
            "INSERT INTO roster (workspace_id, skill_id, principal) VALUES (?1, ?2, ?3) \
             ON CONFLICT (workspace_id, skill_id, principal) DO NOTHING",
            ws,
            skill,
            principal,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage a commit's provenance (skill ownership) + reachability edges directly, with no git write —
    /// for access-join, lineage, and isolation tests that exercise the database logic in isolation. The
    /// primary key still enforces single-skill ownership, so this cannot stage a cross-skill commit.
    pub(crate) async fn seed_commit(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        objects: &[ObjectId],
    ) -> Result<()> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let cid = commit.0.as_slice();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "INSERT INTO skill_commit (workspace_id, commit_id, skill_id) VALUES (?1, ?2, ?3) \
             ON CONFLICT (workspace_id, commit_id) DO NOTHING",
            ws_s,
            cid,
            skill_s,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        for obj in objects {
            let object = obj.0.as_slice();
            sqlx::query!(
                "INSERT INTO commit_object (workspace_id, commit_id, object_id) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (workspace_id, commit_id, object_id) DO NOTHING",
                ws_s,
                cid,
                object,
            )
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
        }
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Remove a roster membership — the revocation mechanism (membership = a row exists, so revocation
    /// is row deletion). Test-only here; enrollment owns issuance + revocation later.
    pub(crate) async fn delete_roster(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()> {
        let (ws, skill, principal) = (ws.as_str(), skill.as_str(), principal.as_str());
        sqlx::query!(
            "DELETE FROM roster WHERE workspace_id = ?1 AND skill_id = ?2 AND principal = ?3",
            ws,
            skill,
            principal,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Stage the per-skill `current` pointer (created, never moved this increment; the signed record
    /// stays absent). Requires the commit's provenance to exist first (the foreign key).
    pub(crate) async fn seed_current(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        commit: CommitId,
        epoch: i64,
        seq: i64,
    ) -> Result<()> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let cid = commit.0.as_slice();
        sqlx::query!(
            "INSERT INTO current (workspace_id, skill_id, commit_id, epoch, seq, signed_record, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 0) \
             ON CONFLICT (workspace_id, skill_id) DO NOTHING",
            ws_s,
            skill_s,
            cid,
            epoch,
            seq,
        )
        .execute(&self.pool)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }
}
