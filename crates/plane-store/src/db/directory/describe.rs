//! The describe SQL — the raw-`sqlx` half of the member-scoped read ops (membership / proposals index
//! / skill log / reach) and the two member-lane guarded writes (notices ack / invite). A child of
//! `mod db`; no `sqlx` type crosses the boundary.
//!
//! The reads are pool reads (they mint nothing durable); the two writes run their guarded `topos_*`
//! SQL function inside a `SERIALIZABLE` transaction and map its outcome codes through the SAME
//! [`unexpected`](super::channels::unexpected) helper the channel ops use (one out-of-contract
//! vocabulary for every policy call). Skill NAMES resolve to the immutable id through the catalog.

use super::channels::unexpected;
use crate::db::{Db, blob32};
use crate::describe::{InviteOutcome, LogProposal, Reach};
use crate::error::{AuthorityError, Result};
use crate::id::{Principal, SkillId, WorkspaceId};

/// The caller's membership facts (a `workspace` ⋈ `workspace_member` row) + the invite policy.
pub(crate) struct MembershipRow {
    pub(crate) name: String,
    pub(crate) display_name: String,
    pub(crate) role: String,
    pub(crate) invited_by: Option<String>,
    pub(crate) invite_policy: String,
}

/// One OPEN proposal as the workspace review-inbox read returns it (the commit message is read from
/// the store by the orchestration, not here).
pub(crate) struct ProposalIndexDbRow {
    pub(crate) skill_id: String,
    pub(crate) skill_name: String,
    pub(crate) version_id: [u8; 32],
    pub(crate) base_version_id: [u8; 32],
    pub(crate) proposer: String,
    pub(crate) created_at: String,
    /// The base no longer equals the live `current` generation.
    pub(crate) stale: bool,
}

/// A resolved catalog row (identity + status + the archived base name).
pub(crate) struct CatalogRow {
    pub(crate) skill_id: String,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) base_name: Option<String>,
}

/// One provenance row for a skill: the version id + the version-purge tombstone.
pub(crate) struct SkillCommitRow {
    pub(crate) version_id: [u8; 32],
    pub(crate) purged_at: Option<i64>,
    pub(crate) purged_by: Option<String>,
}

impl Db {
    /// The caller's membership facts — the workspace identity (name + display name), the caller's
    /// confirmed seat (role + inviter), and the invite policy through its one accessor.
    pub(crate) async fn membership_row(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<Option<MembershipRow>> {
        let (ws_s, prin) = (ws.as_str(), principal.as_str());
        let row = sqlx::query!(
            r#"SELECT w.name AS "name!", w.display_name AS "display_name!",
                      m.role AS "role!", m.invited_by AS "invited_by?",
                      topos_invite_policy($1) AS "invite_policy!"
               FROM workspace w
               JOIN workspace_member m ON m.workspace_id = w.workspace_id
               WHERE w.workspace_id = $1 AND m.principal = $2 AND m.status = 'confirmed'"#,
            ws_s,
            prin,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| MembershipRow {
            name: r.name,
            display_name: r.display_name,
            role: r.role,
            invited_by: r.invited_by,
            invite_policy: r.invite_policy,
        }))
    }

    /// Every OPEN proposal in the workspace, name-sorted, JOINed to the catalog for the skill name and
    /// to `current` for the derived `stale` flag (`base != current` — the same generation comparison
    /// the staleness predicate keys on, here surfaced as a display flag over ALL open rows).
    pub(crate) async fn open_proposals_index(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Vec<ProposalIndexDbRow>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT p.skill_id AS "skill_id!", cat.name AS "skill_name!",
                      p.commit_id AS "commit_id!: Vec<u8>", p.base_commit_id AS "base_commit_id!: Vec<u8>",
                      p.proposer AS "proposer!", p.created_at AS "created_at!",
                      p.base_epoch AS "base_epoch!: i64", p.base_seq AS "base_seq!: i64",
                      c.epoch AS "cur_epoch!: i64", c.seq AS "cur_seq!: i64"
               FROM proposals p
               JOIN catalog cat ON cat.workspace_id = p.workspace_id AND cat.skill_id = p.skill_id
               JOIN current c ON c.workspace_id = p.workspace_id AND c.skill_id = p.skill_id
               WHERE p.workspace_id = $1 AND p.status = 'open'
               ORDER BY cat.name, p.created_at, p.commit_id"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(ProposalIndexDbRow {
                    skill_id: r.skill_id,
                    skill_name: r.skill_name,
                    version_id: blob32(&r.commit_id)?,
                    base_version_id: blob32(&r.base_commit_id)?,
                    proposer: r.proposer,
                    created_at: r.created_at,
                    stale: r.base_epoch != r.cur_epoch || r.base_seq != r.cur_seq,
                })
            })
            .collect()
    }

    /// Resolve a skill NAME to its catalog row — the exact/active name first, else the freed BASE name
    /// of an ARCHIVED successor (`log <old-name>` follows the identity that vacated the name). An exact
    /// name match wins over a base-name hint (an active skill may reuse a freed name).
    pub(crate) async fn catalog_by_name_or_archived_base(
        &self,
        ws: &WorkspaceId,
        name: &str,
    ) -> Result<Option<CatalogRow>> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT skill_id AS "skill_id!", name AS "name!", status AS "status!",
                      base_name AS "base_name?"
               FROM catalog
               WHERE workspace_id = $1
                 AND (name = $2 OR (base_name = $2 AND status = 'archived'))
               ORDER BY CASE WHEN name = $2 THEN 0 ELSE 1 END
               LIMIT 1"#,
            ws_s,
            name,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| CatalogRow {
            skill_id: r.skill_id,
            name: r.name,
            status: r.status,
            base_name: r.base_name,
        }))
    }

    /// Resolve a bare skill id to its catalog row (the last `log` fallback).
    pub(crate) async fn catalog_by_id(
        &self,
        ws: &WorkspaceId,
        skill_id: &str,
    ) -> Result<Option<CatalogRow>> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT skill_id AS "skill_id!", name AS "name!", status AS "status!",
                      base_name AS "base_name?"
               FROM catalog WHERE workspace_id = $1 AND skill_id = $2"#,
            ws_s,
            skill_id,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| CatalogRow {
            skill_id: r.skill_id,
            name: r.name,
            status: r.status,
            base_name: r.base_name,
        }))
    }

    /// Every provenance row for a skill (the version id + its purge tombstone), id-ordered — the log's
    /// unordered tail + the tombstone facts the walk decorates its ordered versions with.
    pub(crate) async fn skill_commit_log(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Vec<SkillCommitRow>> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let rows = sqlx::query!(
            r#"SELECT commit_id AS "commit_id!: Vec<u8>", purged_at AS "purged_at?: i64",
                      purged_by AS "purged_by?"
               FROM skill_commit WHERE workspace_id = $1 AND skill_id = $2
               ORDER BY commit_id"#,
            ws_s,
            skill_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(SkillCommitRow {
                    version_id: blob32(&r.commit_id)?,
                    purged_at: r.purged_at,
                    purged_by: r.purged_by,
                })
            })
            .collect()
    }

    /// Every proposal row for a skill (any status), newest first — the log's proposal timeline.
    pub(crate) async fn skill_proposals_log(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Vec<LogProposal>> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let rows = sqlx::query!(
            r#"SELECT commit_id AS "commit_id!: Vec<u8>", proposer AS "proposer!", status AS "status!",
                      resolved_by AS "resolved_by?", resolved_reason AS "resolved_reason?",
                      resolved_at AS "resolved_at?", created_at AS "created_at!"
               FROM proposals WHERE workspace_id = $1 AND skill_id = $2
               ORDER BY created_at DESC, commit_id"#,
            ws_s,
            skill_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(LogProposal {
                    version_id: blob32(&r.commit_id)?,
                    proposer: r.proposer,
                    status: r.status,
                    resolved_by: r.resolved_by,
                    resolved_reason: r.resolved_reason,
                    resolved_at: r.resolved_at,
                    created_at: r.created_at,
                })
            })
            .collect()
    }

    /// A skill's audience: the confirmed members entitled to it (via the ONE person-entitlement SRF)
    /// and their non-revoked devices. The name resolves at any status; an unknown name is the miss.
    /// Two counts over `topos_person_entitled` (which already gates on a confirmed seat).
    pub(crate) async fn reach(&self, ws: &WorkspaceId, skill_name: &str) -> Result<Reach> {
        let ws_s = ws.as_str();
        let skill_id = sqlx::query_scalar!(
            r#"SELECT skill_id AS "skill_id!" FROM catalog WHERE workspace_id = $1 AND name = $2"#,
            ws_s,
            skill_name,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?
        .ok_or(AuthorityError::NotFound)?;
        let persons = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM workspace_member m
               WHERE m.workspace_id = $1 AND m.status = 'confirmed'
                 AND EXISTS (SELECT 1 FROM topos_person_entitled($1, m.principal) e
                             WHERE e.skill_id = $2)"#,
            ws_s,
            skill_id,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        let devices = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM device_registry dr
               WHERE dr.workspace_id = $1 AND dr.revoked = 0
                 AND EXISTS (SELECT 1 FROM topos_person_entitled($1, dr.principal) e
                             WHERE e.skill_id = $2)"#,
            ws_s,
            skill_id,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(Reach {
            persons: u64::try_from(persons).map_err(AuthorityError::integrity)?,
            devices: u64::try_from(devices).map_err(AuthorityError::integrity)?,
        })
    }

    /// Ack the caller's own notices by id (the read-state write) through the guarded
    /// `topos_notices_ack`. `member_required` folds to the uniform miss; `acked` is the naturally
    /// idempotent success (only the person's own unacked rows move).
    pub(crate) async fn ack_notices_txn(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
        ids: &[String],
        now: i64,
    ) -> Result<()> {
        let (ws_s, prin) = (ws.as_str(), principal.as_str());
        run_serializable!(self, tx, {
            let code = sqlx::query_scalar!(
                r#"SELECT topos_notices_ack($1, $2, $3, $4) AS "outcome!""#,
                ws_s,
                prin,
                ids,
                now,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            match code.as_str() {
                "acked" => Ok(()),
                "member_required" => Err(AuthorityError::NotFound),
                other => Err(unexpected("topos_notices_ack", other)),
            }
        })
    }

    /// Seat the folded emails as invited members (+ any channel pre-placements) through the guarded
    /// `topos_invite` — resolve-all-or-apply-none over the channels, never-demote over the seats.
    /// `member_required` folds to the uniform miss; the other codes are the caller's typed outcomes.
    pub(crate) async fn invite_txn(
        &self,
        ws: &WorkspaceId,
        actor: &Principal,
        emails: &[Principal],
        channels: &[String],
        created_at: &str,
    ) -> Result<InviteOutcome> {
        let (ws_s, actor_s) = (ws.as_str(), actor.as_str());
        let email_strs: Vec<String> = emails.iter().map(|p| p.as_str().to_owned()).collect();
        run_serializable!(self, tx, {
            let code = sqlx::query_scalar!(
                r#"SELECT topos_invite($1, $2, $3, $4, $5) AS "outcome!""#,
                ws_s,
                actor_s,
                &email_strs,
                channels,
                created_at,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(match code.as_str() {
                "invited" => InviteOutcome::Invited {
                    invited: email_strs.clone(),
                },
                "owner_role_required" => InviteOutcome::OwnerRoleRequired,
                "unknown_channel" => InviteOutcome::UnknownChannel,
                "member_required" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_invite", other)),
            })
        })
    }
}
