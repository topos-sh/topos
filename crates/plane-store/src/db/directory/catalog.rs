//! The skill-lifecycle SQL — the raw-`sqlx` half of archive / unarchive / delete / purge. A child
//! of `mod db`; no `sqlx` type crosses the boundary.
//!
//! The POLICY (owner gate, state machine, rename-on-archive with the counter, proposal auto-close +
//! author notices) lives in the guarded `topos_*` functions from migration 0015 — one implementation
//! for every tier. What lives HERE is each op's transaction shape plus the CUSTODY halves the
//! functions cannot own: `delete` un-roots every one of the skill's `commit_object` edges and drops
//! its `current` pointer; `purge` un-roots ONE version's edges — in the SAME transaction as the row
//! policy, so the shipped GC's keep-set (any `commit_object` edge ∪ live lease ∪ open proposal root)
//! reclaims exactly the newly-unrooted bytes on its next pass, and a racing GC claim serializes
//! against the un-root instead of glimpsing a half-purged skill.

use crate::catalog::{LifecycleOutcome, PurgeOutcome};
use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, Principal, WorkspaceId};

use super::channels::resolve_skill_name;

impl Db {
    /// Archive: rename + free the base name + unplace everywhere + auto-close proposals (all in the
    /// function), returning the archived name for the caller's disclosure.
    pub(crate) async fn archive_skill_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        actor: &Principal,
        date_label: &str,
        now: i64,
        created_at: &str,
    ) -> Result<LifecycleOutcome> {
        let ws_s = ws.as_str();
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = sqlx::query_scalar!(
                r#"SELECT topos_archive_skill($1, $2, $3, $4, $5, $6) AS "outcome!""#,
                ws_s,
                &skill_id,
                actor.as_str(),
                date_label,
                now,
                created_at,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(match code.as_str() {
                "archived" => LifecycleOutcome::Archived {
                    archived_name: read_catalog_name(&mut tx, ws, &skill_id).await?,
                },
                "not_active" => LifecycleOutcome::NotActive,
                "owner_role_required" => LifecycleOutcome::OwnerRoleRequired,
                "unknown_skill" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_archive_skill", other)),
            })
        })
    }

    /// Unarchive: rename back if the base name is free, else the typed refusal.
    pub(crate) async fn unarchive_skill_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        actor: &Principal,
    ) -> Result<LifecycleOutcome> {
        let ws_s = ws.as_str();
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = sqlx::query_scalar!(
                r#"SELECT topos_unarchive_skill($1, $2, $3) AS "outcome!""#,
                ws_s,
                &skill_id,
                actor.as_str(),
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(match code.as_str() {
                "unarchived" => LifecycleOutcome::Unarchived {
                    name: read_catalog_name(&mut tx, ws, &skill_id).await?,
                },
                "name_taken" => LifecycleOutcome::NameTaken,
                "not_archived" => LifecycleOutcome::NotArchived,
                "owner_role_required" => LifecycleOutcome::OwnerRoleRequired,
                "unknown_skill" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_unarchive_skill", other)),
            })
        })
    }

    /// Delete (archive-first): tombstone the catalog row (the function), then the custody half —
    /// un-root EVERY commit's `commit_object` edges and drop the `current` pointer, so the next GC
    /// pass reclaims all content bytes no other skill's live versions share. Provenance
    /// (`skill_commit`) and the audit trail survive; deletion cannot recall device copies.
    pub(crate) async fn delete_skill_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        actor: &Principal,
        now: i64,
    ) -> Result<LifecycleOutcome> {
        let ws_s = ws.as_str();
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = sqlx::query_scalar!(
                r#"SELECT topos_delete_skill($1, $2, $3, $4) AS "outcome!""#,
                ws_s,
                &skill_id,
                actor.as_str(),
                now,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(match code.as_str() {
                "deleted" => {
                    sqlx::query!(
                        "DELETE FROM commit_object WHERE workspace_id = $1 AND commit_id IN \
                         (SELECT commit_id FROM skill_commit WHERE workspace_id = $1 AND skill_id = $2)",
                        ws_s,
                        &skill_id,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(AuthorityError::internal)?;
                    sqlx::query!(
                        "DELETE FROM current WHERE workspace_id = $1 AND skill_id = $2",
                        ws_s,
                        &skill_id,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(AuthorityError::internal)?;
                    LifecycleOutcome::Deleted
                }
                "not_archived" => LifecycleOutcome::NotArchived,
                "owner_role_required" => LifecycleOutcome::OwnerRoleRequired,
                "unknown_skill" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_delete_skill", other)),
            })
        })
    }

    /// Purge ONE version's bytes: the row policy (refuse-while-current, the version tombstone,
    /// dependent-proposal closure + author notices) in the function, then the custody half — un-root
    /// exactly that commit's edges. Only blobs unreachable from any live version drop out (shared
    /// objects stay rooted by the other commits' edges); the hash stays in history as the tombstone.
    pub(crate) async fn purge_version_txn(
        &self,
        ws: &WorkspaceId,
        skill_name: &str,
        commit: CommitId,
        actor: &Principal,
        now: i64,
        created_at: &str,
    ) -> Result<PurgeOutcome> {
        let ws_s = ws.as_str();
        let cid = commit.0.as_slice();
        run_serializable!(self, tx, {
            let skill_id = resolve_skill_name(&mut tx, ws, skill_name).await?;
            let code = sqlx::query_scalar!(
                r#"SELECT topos_purge_version_rows($1, $2, $3, $4, $5, $6) AS "outcome!""#,
                ws_s,
                &skill_id,
                cid,
                actor.as_str(),
                now,
                created_at,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok(match code.as_str() {
                "purged" => {
                    sqlx::query!(
                        "DELETE FROM commit_object WHERE workspace_id = $1 AND commit_id = $2",
                        ws_s,
                        cid,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(AuthorityError::internal)?;
                    PurgeOutcome::Purged
                }
                "is_current" => PurgeOutcome::IsCurrent,
                "already_purged" => PurgeOutcome::AlreadyPurged,
                "owner_role_required" => PurgeOutcome::OwnerRoleRequired,
                "unknown_version" | "unknown_skill" => return Err(AuthorityError::NotFound),
                other => return Err(unexpected("topos_purge_version_rows", other)),
            })
        })
    }
}

/// The catalog name after a lifecycle transition (re-read inside the same transaction — archive
/// renamed it; unarchive renamed it back).
async fn read_catalog_name(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: &WorkspaceId,
    skill_id: &str,
) -> Result<String> {
    let ws_s = ws.as_str();
    sqlx::query_scalar!(
        r#"SELECT name AS "name!" FROM catalog WHERE workspace_id = $1 AND skill_id = $2"#,
        ws_s,
        skill_id,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)
}

fn unexpected(function: &'static str, outcome: &str) -> AuthorityError {
    AuthorityError::internal(UnexpectedLifecycleOutcome {
        function,
        outcome: outcome.to_owned(),
    })
}

#[derive(Debug, thiserror::Error)]
#[error("guarded policy function {function} answered {outcome:?}, outside its contract")]
struct UnexpectedLifecycleOutcome {
    function: &'static str,
    outcome: String,
}
