//! The directory's implementation of custody's access-witness seam — plus the directory-owned
//! principal probes the session legs read from the pool.
//!
//! Every fact here comes from a DIRECTORY table (`device_registry`, `roster`, `workspace_member`,
//! `workspace_policy`); custody consumes them only through the
//! [`AccessWitness`](crate::db::custody::witness::AccessWitness) trait, whose in-transaction methods
//! run inside custody's own `SERIALIZABLE` transactions — so a policy-row write committed before a
//! byte op is seen by that op's re-verification (revoke-blocks-promotion), with no duplicated
//! enforcement anywhere.

use sqlx::{Postgres, Transaction};

use crate::db::custody::witness::{AccessWitness, DeviceIdentity, SessionWriteGate};
use crate::db::{Db, blob32};
use crate::error::{AuthorityError, Result};
use crate::governance::Role;
use crate::id::{Principal, SkillId, WorkspaceId};

impl AccessWitness for Db {
    async fn device(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        credential_sha256: &[u8; 32],
    ) -> Result<Option<DeviceIdentity>> {
        device_by_credential(&mut **tx, ws, credential_sha256).await
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

    async fn read_gate(&self, ws: &WorkspaceId, principal: &Principal) -> Result<bool> {
        workspace_member_confirmed(self.pool(), ws, principal).await
    }
}

/// Shared credential-resolution probe (the in-transaction witness read and its pool twin run the
/// identical query; the governance preamble in [`super::governance`] runs it in ITS transaction too).
/// **The one lookup guarded by the partial-unique `device_registry_by_credential`
/// index**: the presented secret's sha256 probes it O(1), bound to the caller's claimed workspace so
/// a cross-workspace credential is the same miss as an unknown one. A revoked row still resolves —
/// the callers separately deny fresh work on it (the flag rides out on [`DeviceIdentity`]).
pub(super) async fn device_by_credential<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    credential_sha256: &[u8; 32],
) -> Result<Option<DeviceIdentity>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let ws_s = ws.as_str();
    let cred = credential_sha256.as_slice();
    let row = sqlx::query!(
        r#"SELECT device_key_id AS "device_key_id!", public_key AS "public_key!: Vec<u8>",
                  principal AS "principal!", revoked AS "revoked!: i64"
           FROM device_registry WHERE workspace_id = $1 AND credential_sha256 = $2"#,
        ws_s,
        cred,
    )
    .fetch_optional(executor)
    .await
    .map_err(AuthorityError::internal)?;
    match row {
        None => Ok(None),
        Some(r) => {
            // Stored values are validated on the way in, so a re-parse failure is store corruption.
            // The stored public key's width is still CHECK-bound; re-validate it here so a corrupt
            // row surfaces as an integrity fault, not a silently-served device.
            blob32(&r.public_key)?;
            let principal = Principal::parse(&r.principal).map_err(AuthorityError::integrity)?;
            Ok(Some(DeviceIdentity {
                device_key_id: r.device_key_id,
                principal,
                revoked: r.revoked != 0,
            }))
        }
    }
}

impl Db {
    /// A CONFIRMED `workspace_member` row exists — the session legs' pool-level preamble probe (the same
    /// predicate the [`read_gate`](AccessWitness::read_gate) runs; exposed separately so a preamble can deny
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

    /// Resolve a presented workspace credential over the POOL for the READ lane: the credential's
    /// sha256 → its non-revoked registry row's principal. The device read lane's authentication —
    /// the member gate + reachability run after it, all misses folding to the caller's uniform
    /// `NotFound`. Unlike the write path (which must replay a since-revoked device's stored receipt
    /// before denying), a read has no replay surface, so `revoked` folds to a miss right here.
    pub(crate) async fn resolve_read_credential(
        &self,
        ws: &WorkspaceId,
        credential_sha256: &[u8; 32],
    ) -> Result<Option<DeviceIdentity>> {
        match device_by_credential(self.pool(), ws, credential_sha256).await? {
            Some(identity) if !identity.revoked => Ok(Some(identity)),
            _ => Ok(None),
        }
    }

    /// Resolve a presented workspace credential over the POOL for the WRITE lane's pre-transaction
    /// machinery (the stable-replay probes and the preflight typed failures need the acting
    /// `device_key_id` before any durable write, and an unauthenticated caller must mint nothing
    /// durable). `revoked` is NOT folded here — a since-revoked device must still reach its replay
    /// probes; the in-transaction resolve + revoked check stay the authority.
    pub(crate) async fn resolve_device_credential(
        &self,
        ws: &WorkspaceId,
        credential_sha256: &[u8; 32],
    ) -> Result<Option<DeviceIdentity>> {
        device_by_credential(self.pool(), ws, credential_sha256).await
    }
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
