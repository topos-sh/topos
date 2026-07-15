//! The custody pool reads — the pointer record, version rows, reachability probes, and the log
//! joins. Autocommit reads at the pool's default isolation; nothing here writes.

use std::collections::HashMap;

use crate::db::Db;
use crate::db::custody::pointer::{PointerRow, parse_stored_version};
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, ObjectId, WorkspaceId};

/// One version row's display facts (the log/read joins).
#[derive(Debug, Clone)]
pub(crate) struct VersionRow {
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
                      (extract(epoch FROM purged_at) * 1000.0)::bigint AS "purged_at_ms"
               FROM version WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3"#,
            ws_s,
            b_s,
            v_s,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.map(|r| VersionRow {
            author_display: r.author_display,
            created_at_ms: r.created_at_ms,
            purged_at_ms: r.purged_at_ms,
        }))
    }

    /// The display facts of MANY versions of one bundle at once (the log's one batched join),
    /// keyed by version id.
    pub(crate) async fn read_version_rows(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        versions: &[CommitId],
    ) -> Result<HashMap<CommitId, VersionRow>> {
        let ws_s = ws.as_str();
        let b_s = bundle.as_str();
        let ids: Vec<String> = versions.iter().map(CommitId::to_hex).collect();
        let rows = sqlx::query!(
            r#"SELECT version_id AS "version_id!", author_display AS "author_display!",
                      (extract(epoch FROM created_at) * 1000.0)::bigint AS "created_at_ms!",
                      (extract(epoch FROM purged_at) * 1000.0)::bigint AS "purged_at_ms"
               FROM version
               WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = ANY($3)"#,
            ws_s,
            b_s,
            &ids,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        let mut map = HashMap::with_capacity(rows.len());
        for r in rows {
            map.insert(
                parse_stored_version(&r.version_id)?,
                VersionRow {
                    author_display: r.author_display,
                    created_at_ms: r.created_at_ms,
                    purged_at_ms: r.purged_at_ms,
                },
            );
        }
        Ok(map)
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

#[derive(Debug, thiserror::Error)]
#[error("stored bundle digest is not 64 lowercase hex characters")]
struct BadStoredDigest;
