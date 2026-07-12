//! The directory's implementation of custody's access-witness seam — plus the directory-owned
//! principal probes the session legs read from the pool.
//!
//! Every fact here comes from a DIRECTORY table (`device_registry`, `workspace_member`, `catalog`,
//! `channels`, `workspace_policy`) or a guarded `topos_*` policy function; custody consumes them only
//! through the [`AccessWitness`](crate::db::custody::witness::AccessWitness) trait, whose
//! in-transaction methods run inside custody's own `SERIALIZABLE` transactions — so a policy-row
//! write committed before a byte op is seen by that op's re-verification (revoke-blocks-promotion),
//! with no duplicated enforcement anywhere. The policy WRITES the pointer-move makes (catalog
//! registration, channel placement, verdict notices) route through the same guarded SQL functions
//! every other tier calls — one policy implementation, in the database.

use sqlx::{Postgres, Transaction};

use crate::db::custody::witness::{
    AccessWitness, ActorRole, DeviceIdentity, GenesisRegistration, PlacementDecision,
    SessionWriteGate, SkillGate,
};
use crate::db::{Db, blob32};
use crate::error::{AuthorityError, Result};
use crate::governance::Role;
use crate::id::{CommitId, Principal, SkillId, WorkspaceId};

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

    async fn member_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        principal: &Principal,
    ) -> Result<Option<ActorRole>> {
        match super::governance::read_member_role(tx, ws, principal).await? {
            Some((role, status)) if status == "confirmed" => Ok(Some(parse_role(&role)?)),
            _ => Ok(None),
        }
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

    async fn skill_gate(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<SkillGate> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        // One round trip: the catalog row's status (absent ⇒ unregistered) + the resolved protection
        // cascade (per-bundle pin, else the workspace default, else open — `topos_effective_protection`
        // is the one cascade implementation, shared with the delivery read).
        let row = sqlx::query!(
            r#"SELECT (SELECT status FROM catalog WHERE workspace_id = $1 AND skill_id = $2) AS "status?",
                      topos_effective_protection($1, $2) AS "protection!""#,
            ws_s,
            skill_s,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        let reviewed = row.protection == "reviewed";
        Ok(match row.status.as_deref() {
            None => SkillGate::Missing { reviewed },
            Some("active") => SkillGate::Active { reviewed },
            Some("archived") => SkillGate::Archived,
            Some("deleted") => SkillGate::Deleted,
            Some(other) => {
                return Err(AuthorityError::integrity(UnknownCatalogStatus(
                    other.to_owned(),
                )));
            }
        })
    }

    async fn register_publish(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        display_name: Option<&str>,
        author: &Principal,
        to_channel: Option<&str>,
        created_at: &str,
    ) -> Result<GenesisRegistration> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let minted = mint_catalog_name(display_name, skill);
        // Probe both uniqueness axes at once: an existing row for THIS skill (already registered —
        // idempotent; keep ITS name) vs. the minted name taken by ANOTHER skill (a typed refusal:
        // two identities cannot share one name).
        let existing = sqlx::query!(
            r#"SELECT skill_id AS "skill_id!", name AS "name!" FROM catalog
               WHERE workspace_id = $1 AND (skill_id = $2 OR name = $3)"#,
            ws_s,
            skill_s,
            &minted,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        let name = match existing.iter().find(|r| r.skill_id == skill_s) {
            Some(own) => own.name.clone(),
            None => {
                if !existing.is_empty() {
                    return Ok(GenesisRegistration::NameTaken { name: minted });
                }
                sqlx::query!(
                    "INSERT INTO catalog (workspace_id, skill_id, name, display_name, status, created_at) \
                     VALUES ($1, $2, $3, $4, 'active', $5)",
                    ws_s,
                    skill_s,
                    &minted,
                    display_name,
                    created_at,
                )
                .execute(&mut **tx)
                .await
                .map_err(AuthorityError::internal)?;
                minted
            }
        };
        // Advisory display name: last-writer-wins among publishes that express one (the fresh INSERT
        // above already carried it; this covers the already-registered arm).
        if let Some(dn) = display_name {
            set_catalog_display_name(&mut **tx, ws, skill, dn).await?;
        }
        // Every workspace is born with `everyone`; converge here for fixture-seeded workspaces too.
        sqlx::query!(
            "SELECT topos_ensure_everyone($1, $2) AS \"ok\"",
            ws_s,
            created_at
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        // The placement: the requested `--to` channel, else the `everyone` default (a brand-new
        // skill must land SOMEWHERE discoverable — that is the whole "nobody knows it's there" fix).
        let placement = place_via_function(
            tx,
            ws,
            skill,
            to_channel.unwrap_or("everyone"),
            author,
            created_at,
        )
        .await?;
        // The author's self-follow: an author follows what they create (a DIRECT follow — it
        // survives any channel dropping the skill).
        let followed = sqlx::query!(
            r#"SELECT topos_follow_skill($1, $2, $3, NULL, $4) AS "outcome!""#,
            ws_s,
            author.as_str(),
            skill_s,
            created_at,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        if followed.outcome != "followed" {
            return Err(AuthorityError::internal(PolicyFunctionInvariant {
                function: "topos_follow_skill",
                outcome: followed.outcome,
            }));
        }
        Ok(GenesisRegistration::Registered { name, placement })
    }

    async fn place_skill(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        channel: &str,
        actor: &Principal,
        created_at: &str,
    ) -> Result<PlacementDecision> {
        place_via_function(tx, ws, skill, channel, actor, created_at).await
    }

    async fn set_display_name(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        display_name: &str,
    ) -> Result<()> {
        set_catalog_display_name(&mut **tx, ws, skill, display_name).await
    }

    async fn notify_verdict(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ws: &WorkspaceId,
        skill: &SkillId,
        version: CommitId,
        recipient: &Principal,
        outcome: &str,
        reason: Option<&str>,
        actor: &Principal,
        created_at: &str,
    ) -> Result<()> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let vid = version.0.as_slice();
        sqlx::query!(
            "INSERT INTO notices (workspace_id, id, principal, kind, skill_id, version_id, actor, outcome, reason, created_at) \
             VALUES ($1, gen_random_uuid()::TEXT, $2, 'verdict', $3, $4, $5, $6, $7, $8)",
            ws_s,
            recipient.as_str(),
            skill_s,
            vid,
            actor.as_str(),
            outcome,
            reason,
            created_at,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        Ok(())
    }

    async fn read_gate(&self, ws: &WorkspaceId, principal: &Principal) -> Result<bool> {
        workspace_member_confirmed(self.pool(), ws, principal).await
    }
}

/// Run the guarded placement function and map its outcome vocabulary to custody's. Codes that the
/// transaction's earlier gates make unreachable (a non-member actor, an unknown/inactive skill) are
/// internal invariant breaches, never user outcomes.
async fn place_via_function(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    skill: &SkillId,
    channel: &str,
    actor: &Principal,
    created_at: &str,
) -> Result<PlacementDecision> {
    let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
    let row = sqlx::query!(
        r#"SELECT topos_channel_place($1, $2, $3, $4, $5) AS "outcome!""#,
        ws_s,
        channel,
        skill_s,
        actor.as_str(),
        created_at,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(match row.outcome.as_str() {
        "placed" => PlacementDecision::Placed {
            channel: channel.to_owned(),
        },
        "created" => PlacementDecision::Created {
            channel: channel.to_owned(),
        },
        "curated_role_required" => PlacementDecision::RoleDenied {
            channel: channel.to_owned(),
        },
        "bad_name" => PlacementDecision::BadName {
            channel: channel.to_owned(),
        },
        other => {
            return Err(AuthorityError::internal(PolicyFunctionInvariant {
                function: "topos_channel_place",
                outcome: other.to_owned(),
            }));
        }
    })
}

async fn set_catalog_display_name<'e, E>(
    executor: E,
    ws: &WorkspaceId,
    skill: &SkillId,
    display_name: &str,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
    sqlx::query!(
        "UPDATE catalog SET display_name = $3 WHERE workspace_id = $1 AND skill_id = $2",
        ws_s,
        skill_s,
        display_name,
    )
    .execute(executor)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// Mint a catalog name from the advisory display name (folded to the agent-skills charset:
/// lowercase letters, digits, hyphens), else from the skill id (`_` → `-`). Mirrors the 0015
/// backfill's derivation. Never empty; capped at the birth length.
pub(crate) fn mint_catalog_name(display_name: Option<&str>, skill: &SkillId) -> String {
    const MAX_BIRTH_NAME: usize = 64;
    let fold = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut last_hyphen = true; // suppress a leading hyphen
        for c in s.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                out.push(c);
                last_hyphen = false;
            } else if !last_hyphen {
                out.push('-');
                last_hyphen = true;
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        out.truncate(MAX_BIRTH_NAME);
        while out.ends_with('-') {
            out.pop();
        }
        out
    };
    let from_display = display_name.map(fold).filter(|s| !s.is_empty());
    from_display
        .or_else(|| Some(fold(skill.as_str())).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "skill".to_owned())
}

/// Map a stored role string to the witness vocabulary; an unknown spelling is store corruption
/// (the column is CHECK-bound).
fn parse_role(role: &str) -> Result<ActorRole> {
    match role {
        "owner" => Ok(ActorRole::Owner),
        "reviewer" => Ok(ActorRole::Reviewer),
        "member" => Ok(ActorRole::Member),
        other => Err(AuthorityError::integrity(UnknownRole(other.to_owned()))),
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

    /// Upsert the workspace's `review_required` policy — the PROTECTION DEFAULT the per-bundle
    /// cascade falls back to (`topos_effective_protection`): flipping it moves every bundle without
    /// an explicit per-bundle pin, existing ones included. The single home for the policy row;
    /// `Authority::set_review_required` is the public op, and the test-fixtures
    /// `seed_review_required` shim delegates to it.
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

    /// The skill's resolved effective protection (the per-bundle pin, else the workspace default,
    /// else open) as the boolean the read surfaces disclose — a POOL read over the ONE cascade
    /// implementation (`topos_effective_protection`), shared with the in-txn gate.
    pub(crate) async fn effective_protection_reviewed(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<bool> {
        let (ws_s, skill_s) = (ws.as_str(), skill.as_str());
        let row = sqlx::query!(
            r#"SELECT topos_effective_protection($1, $2) AS "protection!""#,
            ws_s,
            skill_s,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.protection == "reviewed")
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

/// A CONFIRMED `workspace_member` row exists for this principal (the workspace-level RBAC roster).
/// The query text is byte-identical to `enroll::read_member_status`, so the committed `.sqlx` cache
/// already covers it.
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

#[derive(Debug, thiserror::Error)]
#[error("a catalog row carries an unknown status {0:?}")]
struct UnknownCatalogStatus(String);

#[derive(Debug, thiserror::Error)]
#[error("a workspace_member row carries an unknown role {0:?}")]
struct UnknownRole(String);

#[derive(Debug, thiserror::Error)]
#[error(
    "guarded policy function {function} answered {outcome:?} where the transaction's gates make that unreachable"
)]
struct PolicyFunctionInvariant {
    function: &'static str,
    outcome: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_catalog_name_folds_display_names_and_falls_back_to_the_skill_id() {
        let sid = SkillId::parse("topos_0af3c9d2").unwrap();
        // Display names fold to the agent-skills charset (lowercase letters, digits, hyphens).
        assert_eq!(
            mint_catalog_name(Some("Deploy Guide"), &sid),
            "deploy-guide"
        );
        assert_eq!(
            mint_catalog_name(Some("deploy_guide"), &sid),
            "deploy-guide"
        );
        assert_eq!(mint_catalog_name(Some("deploy"), &sid), "deploy");
        // Punctuation collapses to single hyphens; leading/trailing hyphens are trimmed.
        assert_eq!(mint_catalog_name(Some("--A  (b)!C--"), &sid), "a-b-c");
        // An unusable display name falls back to the skill id fold.
        assert_eq!(mint_catalog_name(Some("!!!"), &sid), "topos-0af3c9d2");
        assert_eq!(mint_catalog_name(None, &sid), "topos-0af3c9d2");
        // The birth cap holds.
        let long = "x".repeat(100);
        assert_eq!(mint_catalog_name(Some(&long), &sid).len(), 64);
    }
}
