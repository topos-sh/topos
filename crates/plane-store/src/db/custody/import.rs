//! The `import-local` SQL — the backfill upserts and the completeness-verification reads the
//! one-shot cutover runs. Offline-only (the vault is stopped), but the writes still ride the
//! serializable runner for the same rollback/retry discipline.

use sqlx::{Postgres, Transaction};

use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{ObjectId, WorkspaceId};

/// What one version backfill did.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BackfillOutcome {
    /// Rows that gained the locator/message this call (0 on an idempotent rerun).
    pub updated: u64,
    /// A row for this version is already backfilled to a DIFFERENT locator — a divergence the
    /// import refuses to paper over.
    pub conflicted: bool,
}

impl Db {
    /// A presence row's recorded `git_oid` for `object_id` (any status). `None` = no row (an
    /// orphan large file); `Some(None)` = a row with no locator (corruption — every fenced row
    /// records one).
    pub(crate) async fn import_object_git_oid(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<Option<Option<[u8; 20]>>> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT git_oid AS "git_oid: Vec<u8>" FROM object_presence
               WHERE workspace_id = $1 AND object_id = $2"#,
            ws_s,
            oid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| r.git_oid.and_then(|b| b.try_into().ok())))
    }

    /// Flip a converted object's `location` to the store value (idempotent; only pre-import
    /// values flip).
    pub(crate) async fn import_flip_large_location(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<()> {
        run_serializable!(self, tx, { import_flip_txn(&mut tx, ws, object_id).await })
    }

    /// Backfill one version's `git_commit_oid` + `message` across every bundle that carries it
    /// (the version id is content-derived; two bundles holding the same version share the same
    /// commit object). Idempotent; a row already set to a DIFFERENT locator is reported, never
    /// overwritten.
    pub(crate) async fn import_backfill_version(
        &self,
        ws: &WorkspaceId,
        version_hex: &str,
        git_commit_oid: &[u8; 20],
        message: &str,
    ) -> Result<BackfillOutcome> {
        run_serializable!(self, tx, {
            import_backfill_txn(&mut tx, ws, version_hex, git_commit_oid, message).await
        })
    }

    /// Every non-purged version with its commit locator, as `(bundle, version, locator)` — the
    /// deep-verification walk's roots.
    pub(crate) async fn import_live_versions(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Vec<(String, String, [u8; 20])>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT bundle_id AS "bundle_id!", version_id AS "version_id!",
                      git_commit_oid AS "git_commit_oid!: Vec<u8>"
               FROM version
               WHERE workspace_id = $1 AND purged_at IS NULL AND git_commit_oid IS NOT NULL
               ORDER BY bundle_id, version_id"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    r.bundle_id,
                    r.version_id,
                    r.git_commit_oid
                        .try_into()
                        .map_err(|_| AuthorityError::integrity(BadWidth))?,
                ))
            })
            .collect()
    }

    /// Every non-purged version row still missing its commit locator, as `(bundle, version)`.
    pub(crate) async fn import_unbackfilled(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Vec<(String, String)>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT bundle_id AS "bundle_id!", version_id AS "version_id!" FROM version
               WHERE workspace_id = $1 AND purged_at IS NULL AND git_commit_oid IS NULL
               ORDER BY bundle_id, version_id"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(rows
            .into_iter()
            .map(|r| (r.bundle_id, r.version_id))
            .collect())
    }

    /// The distinct commit locators of the workspace's non-purged versions (existence-checked
    /// against the store).
    pub(crate) async fn import_version_locators(&self, ws: &WorkspaceId) -> Result<Vec<[u8; 20]>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT DISTINCT git_commit_oid AS "git_commit_oid!: Vec<u8>" FROM version
               WHERE workspace_id = $1 AND purged_at IS NULL AND git_commit_oid IS NOT NULL"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                r.git_commit_oid
                    .try_into()
                    .map_err(|_| AuthorityError::integrity(BadWidth))
            })
            .collect()
    }

    /// The distinct store locators of every PRESENT blob some non-purged version reaches
    /// (existence-checked against the store).
    pub(crate) async fn import_live_blob_locators(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Vec<[u8; 20]>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT DISTINCT op.git_oid AS "git_oid!: Vec<u8>" FROM object_presence op
               WHERE op.workspace_id = $1 AND op.status = 'present' AND op.git_oid IS NOT NULL
                 AND EXISTS (
                     SELECT 1 FROM version_object vo
                     JOIN version v
                       ON v.workspace_id = vo.workspace_id AND v.bundle_id = vo.bundle_id
                      AND v.version_id = vo.version_id
                     WHERE vo.workspace_id = op.workspace_id AND vo.object_id = op.object_id
                       AND v.purged_at IS NULL)"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                r.git_oid
                    .try_into()
                    .map_err(|_| AuthorityError::integrity(BadWidth))
            })
            .collect()
    }

    /// Reachability edges of non-purged versions whose object has NO present row — must be zero
    /// on a whole vault.
    pub(crate) async fn import_edges_without_present(&self, ws: &WorkspaceId) -> Result<i64> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT count(*) AS "n!" FROM version_object vo
               JOIN version v
                 ON v.workspace_id = vo.workspace_id AND v.bundle_id = vo.bundle_id
                AND v.version_id = vo.version_id
               WHERE vo.workspace_id = $1 AND v.purged_at IS NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM object_presence op
                     WHERE op.workspace_id = vo.workspace_id AND op.object_id = vo.object_id
                       AND op.status = 'present')"#,
            ws_s,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.n)
    }

    /// Present rows still recording a pre-import `location` — must be zero after conversion.
    pub(crate) async fn import_non_store_present(&self, ws: &WorkspaceId) -> Result<i64> {
        let ws_s = ws.as_str();
        let row = sqlx::query!(
            r#"SELECT count(*) AS "n!" FROM object_presence
               WHERE workspace_id = $1 AND status = 'present' AND location <> 'git'"#,
            ws_s,
        )
        .fetch_one(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.n)
    }
}

async fn import_flip_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    object_id: ObjectId,
) -> Result<()> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    sqlx::query!(
        "UPDATE object_presence SET location = 'git' \
         WHERE workspace_id = $1 AND object_id = $2 AND location <> 'git'",
        ws_s,
        oid,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

async fn import_backfill_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    version_hex: &str,
    git_commit_oid: &[u8; 20],
    message: &str,
) -> Result<BackfillOutcome> {
    let ws_s = ws.as_str();
    let oid = git_commit_oid.as_slice();
    let updated = sqlx::query!(
        "UPDATE version SET git_commit_oid = $3, message = COALESCE(message, $4) \
         WHERE workspace_id = $1 AND version_id = $2 AND git_commit_oid IS NULL",
        ws_s,
        version_hex,
        oid,
        message,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?
    .rows_affected();
    let conflicted = sqlx::query!(
        r#"SELECT count(*) AS "n!" FROM version
           WHERE workspace_id = $1 AND version_id = $2 AND git_commit_oid IS DISTINCT FROM $3"#,
        ws_s,
        version_hex,
        oid,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?
    .n > 0;
    Ok(BackfillOutcome {
        updated,
        conflicted,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("a stored locator has the wrong width")]
struct BadWidth;
