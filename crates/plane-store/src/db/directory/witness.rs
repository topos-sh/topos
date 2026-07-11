//! The directory's implementation of custody's access-witness seam — plus the directory-owned
//! principal probes the session legs read from the pool.
//!
//! Every fact here comes from a DIRECTORY table (`device_registry`, `roster`, `workspace_member`,
//! `workspace_policy`, `read_token`); custody consumes them only through the
//! [`AccessWitness`](crate::db::custody::witness::AccessWitness) trait, whose in-transaction methods
//! run inside custody's own `SERIALIZABLE` transactions — so a policy-row write committed before a
//! byte op is seen by that op's re-verification (revoke-blocks-promotion), with no duplicated
//! enforcement anywhere.

use sqlx::{Postgres, Transaction};

use crate::db::custody::witness::{AccessWitness, DeviceIdentity, SessionWriteGate};
use crate::db::{Db, ReadLane, blob32};
use crate::error::{AuthorityError, Result};
use crate::governance::Role;
use crate::id::{Principal, SkillId, WorkspaceId};

impl AccessWitness for Db {
    async fn device(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        device_key_id: &str,
    ) -> Result<Option<DeviceIdentity>> {
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
                // The stored public key's width is still CHECK-bound; re-validate it here so a corrupt
                // row surfaces as an integrity fault, not a silently-served device.
                blob32(&r.public_key)?;
                let principal =
                    Principal::parse(&r.principal).map_err(AuthorityError::integrity)?;
                Ok(Some(DeviceIdentity {
                    principal,
                    revoked: r.revoked != 0,
                }))
            }
        }
    }

    async fn rostered(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<bool> {
        roster_exists(&mut **tx, ws, skill, principal).await
    }

    async fn confirmed_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<bool> {
        workspace_member_confirmed(&mut **tx, ws, principal).await
    }

    async fn session_write_gate(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<SessionWriteGate> {
        // The role matrix, in ONE place: a confirmed owner|reviewer seat may drive a session
        // review/revert write; a confirmed plain member is entitled to the durable typed role denial;
        // anyone unproven in THIS workspace (no seat, merely invited) gets only a synthesized one.
        match super::governance::read_member_role(tx, ws, principal).await? {
            Some((role, status)) if status == "confirmed" => {
                if role == Role::Owner.as_str() || role == Role::Reviewer.as_str() {
                    Ok(SessionWriteGate::Authorized)
                } else {
                    Ok(SessionWriteGate::RoleDenied)
                }
            }
            _ => Ok(SessionWriteGate::Unproven),
        }
    }

    async fn review_required(
        &self,
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

    async fn seat_roster(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
    ) -> Result<()> {
        insert_roster(&mut **tx, ws, skill, principal).await
    }

    async fn read_gate(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        principal: &Principal,
        lane: ReadLane,
    ) -> Result<bool> {
        match lane {
            ReadLane::SkillRoster => roster_exists(self.pool(), ws, skill, principal).await,
            ReadLane::WorkspaceMember => {
                workspace_member_confirmed(self.pool(), ws, principal).await
            }
        }
    }
}

impl Db {
    /// A CONFIRMED `workspace_member` row exists — the session legs' pool-level preamble probe (the same
    /// predicate the [`ReadLane::WorkspaceMember`] gate runs; exposed separately so a preamble can deny
    /// BEFORE any per-skill work).
    pub(crate) async fn confirmed_member(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<bool> {
        workspace_member_confirmed(self.pool(), ws, principal).await
    }

    /// The principal's `(role, status)` on this workspace, or `None` for no seat — a POOL read the session
    /// revert leg uses as a cheap pre-stage owner|reviewer fence (it constructs a forward commit before the
    /// transaction, so an unauthorized member must be turned away BEFORE that git work; the in-txn gate stays
    /// authoritative). Mirrors the in-txn [`governance::read_member_role`](super::governance::read_member_role)
    /// query against the pool.
    pub(crate) async fn member_role(
        &self,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<Option<(String, String)>> {
        let (ws_s, prin) = (ws.as_str(), principal.as_str());
        let row = sqlx::query!(
            r#"SELECT role AS "role!", status AS "status!" FROM workspace_member
               WHERE workspace_id = $1 AND principal = $2"#,
            ws_s,
            prin,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| (r.role, r.status)))
    }

    /// Whether the workspace's review-required policy is on — the cheap POOL preflight read (the
    /// in-transaction witness read is authoritative; this one only saves a doomed ingest). Absent row
    /// ⇒ off (the default).
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

    /// Upsert the workspace's `review_required` policy (the write the reads above consult). The single
    /// home for the policy row; `Authority::set_review_required` is the public op, and the test-fixtures
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

    /// Resolve a read token's sha256 to its `(workspace, skill, principal)` scope — the read-credential
    /// resolver. **The one lookup NOT bound on `workspace_id`:** the token IS what resolves the workspace,
    /// so this probes the globally-unique `token_sha256` primary key (O(1)) and ESTABLISHES the binding
    /// every subsequent query carries. Only the hash is stored, never the plaintext. The row's strings were
    /// validated when the token was minted, so a re-parse failure is store corruption (an integrity fault),
    /// not a client error. `None` ⇒ no such token.
    pub(crate) async fn lookup_read_token(
        &self,
        token_sha256: &[u8; 32],
        now: i64,
    ) -> Result<Option<(WorkspaceId, SkillId, Principal)>> {
        let key = token_sha256.as_slice();
        let row = sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", skill_id AS "skill_id!", principal AS "principal!"
               FROM read_token WHERE token_sha256 = $1 AND (expires_at IS NULL OR expires_at > $2)"#,
            key,
            now,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some((
                WorkspaceId::parse(&r.workspace_id).map_err(AuthorityError::integrity)?,
                SkillId::parse(&r.skill_id).map_err(AuthorityError::integrity)?,
                Principal::parse(&r.principal).map_err(AuthorityError::integrity)?,
            ))),
        }
    }
}

/// Shared roster-existence probe (used by both the pool-level read gate and the in-transaction write
/// gate). Generic over the executor so the identical query serves both.
async fn roster_exists<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    skill: &SkillId,
    principal: &Principal,
) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws = ws.as_str();
    let skill = skill.as_str();
    let principal = principal.as_str();
    let row = sqlx::query!(
        "SELECT principal FROM roster WHERE workspace_id = $1 AND skill_id = $2 AND principal = $3 LIMIT 1",
        ws,
        skill,
        principal,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some())
}

/// A CONFIRMED `workspace_member` row exists for this principal (the workspace-level RBAC roster,
/// distinct from the per-skill read `roster`). The query text is byte-identical to
/// `enroll::read_member_status`, so the committed `.sqlx` cache already covers it.
async fn workspace_member_confirmed<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    principal: &Principal,
) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws_s = ws.as_str();
    let principal = principal.as_str();
    let row = sqlx::query!(
        r#"SELECT status AS "status!" FROM workspace_member WHERE workspace_id = $1 AND principal = $2"#,
        ws_s,
        principal,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(matches!(row, Some(r) if r.status == "confirmed"))
}

/// Self-insert a per-skill roster row — the genesis standup's one write (a first publish rosters its own
/// author for the skill it creates, inside the same transaction as the pointer). The INSERT text is
/// byte-identical to `enroll::redeem_run`'s roster grant, so the committed `.sqlx` cache already covers it;
/// `ON CONFLICT DO NOTHING` keeps a concurrent standup / governance roster mutation convergent.
async fn insert_roster<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    skill: &SkillId,
    principal: &Principal,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws_s = ws.as_str();
    let sk = skill.as_str();
    let prin = principal.as_str();
    sqlx::query!(
        "INSERT INTO roster (workspace_id, skill_id, principal) VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, skill_id, principal) DO NOTHING",
        ws_s,
        sk,
        prin,
    )
    .execute(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}
