//! The custody pool reads — the pointer record, version rows, reachability probes, and the log
//! joins. Autocommit reads at the pool's default isolation; nothing here writes.

use crate::db::Db;
use crate::db::custody::lifecycle::reparse_workspace;
use crate::db::custody::pointer::{PointerRow, parse_stored_version};
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, ObjectId, WorkspaceId};

/// One version row's display facts (the log/read joins). `first_parent`/`git_commit_oid`/`message`
/// ride along for the metadata read; the two de-git columns are `None` on a pre-import row.
#[derive(Debug, Clone)]
pub(crate) struct VersionRow {
    pub author_display: String,
    pub created_at_ms: i64,
    pub purged_at_ms: Option<i64>,
    pub first_parent: Option<CommitId>,
    pub git_commit_oid: Option<[u8; 20]>,
    pub message: Option<String>,
}

/// One hop of the pure-Postgres first-parent log walk.
#[derive(Debug, Clone)]
pub(crate) struct LogHop {
    pub version_id: CommitId,
    /// `None` on a pre-import row — the read surface fails closed naming `import-local`.
    pub message: Option<String>,
    pub author_display: String,
    pub created_at_ms: i64,
    pub purged_at_ms: Option<i64>,
}

impl Db {
    /// The bundle's `current` pointer row (a pool read). `None` until a pointer exists.
    pub(crate) async fn read_pointer(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
    ) -> Result<Option<PointerRow>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let row = sqlx::query!(
            r#"SELECT version_id AS "version_id!", generation AS "generation!",
                      moved_by_display AS "moved_by!",
                      (extract(epoch FROM moved_at) * 1000.0)::bigint AS "moved_at_ms!"
               FROM current_pointer WHERE workspace_id = $1 AND bundle_id = $2"#,
            ws_s,
            b_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| {
            Ok(PointerRow {
                version_id: parse_stored_version(&r.version_id)?,
                generation: u64::try_from(r.generation).map_err(AuthorityError::integrity)?,
                moved_at_ms: r.moved_at_ms,
                moved_by: r.moved_by,
            })
        })
        .transpose()
    }

    /// One version row's display facts. `None` when the version does not exist in this bundle.
    pub(crate) async fn read_version_row(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
    ) -> Result<Option<VersionRow>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let v_s = version.to_hex();
        let row = sqlx::query!(
            r#"SELECT author_display AS "author_display!",
                      (extract(epoch FROM created_at) * 1000.0)::bigint AS "created_at_ms!",
                      (extract(epoch FROM purged_at) * 1000.0)::bigint AS "purged_at_ms",
                      first_parent,
                      git_commit_oid AS "git_commit_oid: Vec<u8>",
                      message
               FROM version WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3"#,
            ws_s,
            b_s,
            v_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| {
            Ok(VersionRow {
                author_display: r.author_display,
                created_at_ms: r.created_at_ms,
                purged_at_ms: r.purged_at_ms,
                first_parent: r
                    .first_parent
                    .as_deref()
                    .map(parse_stored_version)
                    .transpose()?,
                git_commit_oid: r.git_commit_oid.map(commit_oid_from_row).transpose()?,
                message: r.message,
            })
        })
        .transpose()
    }

    /// A version row's recorded git commit locator. `None` = no such version row;
    /// `Some(None)` = a pre-import row (the caller fails closed naming `import-local`).
    pub(crate) async fn version_git_commit_oid(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
    ) -> Result<Option<Option<[u8; 20]>>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let v_s = version.to_hex();
        let row = sqlx::query!(
            r#"SELECT git_commit_oid AS "git_commit_oid: Vec<u8>"
               FROM version WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3"#,
            ws_s,
            b_s,
            v_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| r.git_commit_oid.map(commit_oid_from_row).transpose())
            .transpose()
    }

    /// The first-parent chain from `head`, capped at `limit` — ONE recursive query over the
    /// persisted `first_parent` column (history is a list; the linearity fence made the column the
    /// whole parent set). Rows come back newest-first. A chain hop that leaves the bundle simply
    /// ends the walk (the recursive join binds workspace AND bundle).
    pub(crate) async fn log_chain(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        head: CommitId,
        limit: usize,
    ) -> Result<Vec<LogHop>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let head_s = head.to_hex();
        let cap = i64::try_from(limit).map_err(AuthorityError::internal)?;
        let rows = sqlx::query!(
            r#"WITH RECURSIVE chain AS (
                   SELECT version_id, first_parent, message, author_display, created_at, purged_at,
                          1::bigint AS depth
                   FROM version
                   WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3
                 UNION ALL
                   SELECT v.version_id, v.first_parent, v.message, v.author_display, v.created_at,
                          v.purged_at, chain.depth + 1
                   FROM version v
                   JOIN chain ON v.version_id = chain.first_parent
                   WHERE v.workspace_id = $1 AND v.bundle_id = $2 AND chain.depth < $4
               )
               SELECT version_id AS "version_id!", message,
                      author_display AS "author_display!",
                      (extract(epoch FROM created_at) * 1000.0)::bigint AS "created_at_ms!",
                      (extract(epoch FROM purged_at) * 1000.0)::bigint AS "purged_at_ms",
                      depth AS "depth!"
               FROM chain ORDER BY depth"#,
            ws_s,
            b_s,
            head_s,
            cap,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok(LogHop {
                    version_id: parse_stored_version(&r.version_id)?,
                    message: r.message,
                    author_display: r.author_display,
                    created_at_ms: r.created_at_ms,
                    purged_at_ms: r.purged_at_ms,
                })
            })
            .collect()
    }

    /// The consent digest recorded for a version. `None` when the version has no digest row (an
    /// authorized read maps that to an integrity fault — every committed version records one).
    pub(crate) async fn read_bundle_digest(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
    ) -> Result<Option<[u8; 32]>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let v_s = version.to_hex();
        let row = sqlx::query!(
            r#"SELECT bundle_digest AS "bundle_digest!" FROM version_digest
               WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3"#,
            ws_s,
            b_s,
            v_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| {
            crate::id::parse_hex32(&r.bundle_digest)
                .ok_or_else(|| AuthorityError::integrity(BadStoredDigest))
        })
        .transpose()
    }

    /// The distinct objects one version reaches (its `version_object` edges) — the revert's
    /// availability + edge set for the forward commit it constructs.
    pub(crate) async fn version_objects(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
    ) -> Result<Vec<ObjectId>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let v_s = version.to_hex();
        let rows = sqlx::query!(
            r#"SELECT object_id AS "object_id!: Vec<u8>" FROM version_object
               WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3"#,
            ws_s,
            b_s,
            v_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| super::lifecycle::object_id_from_row(r.object_id))
            .collect()
    }

    /// Per-workspace stored byte totals — `SUM(size)` over the `present` rows alone, grouped by
    /// workspace and ordered by workspace id (deterministic). `size` is operational bookkeeping
    /// (accounting + size-routing), so this read is pure accounting: no byte is touched.
    pub(crate) async fn storage_stats(&self) -> Result<Vec<(WorkspaceId, u64)>> {
        let rows = sqlx::query!(
            r#"SELECT workspace_id AS "workspace_id!", SUM(size)::bigint AS "stored_bytes!"
               FROM object_presence
               WHERE status = 'present'
               GROUP BY workspace_id
               ORDER BY workspace_id"#,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    reparse_workspace(&r.workspace_id)?,
                    u64::try_from(r.stored_bytes).map_err(AuthorityError::integrity)?,
                ))
            })
            .collect()
    }

    /// The reachability witness: some NON-PURGED version of THIS bundle reaches `object_id`.
    /// Returns one such version id (the tree-walk fallback's anchor), or `None` — the read maps
    /// `None` to the uniform not-found. **No object is ever served by bare hash.**
    pub(crate) async fn object_witness(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        object_id: ObjectId,
    ) -> Result<Option<CommitId>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT vo.version_id AS "version_id!" FROM version_object vo
               JOIN version v
                 ON v.workspace_id = vo.workspace_id AND v.bundle_id = vo.bundle_id
                AND v.version_id = vo.version_id
               WHERE vo.workspace_id = $1 AND vo.bundle_id = $2 AND vo.object_id = $3
                 AND v.purged_at IS NULL
               LIMIT 1"#,
            ws_s,
            b_s,
            oid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        row.map(|r| parse_stored_version(&r.version_id)).transpose()
    }
}

/// Convert a stored 20-byte commit-locator BYTEA into an array, or an integrity fault on a bad
/// width (the CHECK constraint forbids it, so this only fires on corruption).
fn commit_oid_from_row(bytes: Vec<u8>) -> Result<[u8; 20]> {
    bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadStoredCommitOid))
}

#[derive(Debug, thiserror::Error)]
#[error("stored bundle digest is not 64 lowercase hex characters")]
struct BadStoredDigest;

#[derive(Debug, thiserror::Error)]
#[error("stored git commit locator is not 20 bytes")]
struct BadStoredCommitOid;
