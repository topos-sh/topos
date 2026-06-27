//! The object-lifecycle SQL — the fenced `object_presence` state machine, promotion leases, the upload
//! quarantine, and tombstones. Raw `sqlx` stays here (a child of `mod sqlite`); every method takes the
//! validated id newtypes + an explicit `now` and returns plain domain values, so no `sqlx` type crosses
//! the module boundary and no caller can run an unbound query. The database is the sole authority for an
//! object's byte status; the git store always trails it.

use sqlx::Row as _;

use super::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, OpId, WorkspaceId};

/// The width of a git object id (SHA-1), the physical locator stored in `object_presence.git_oid`.
pub(crate) const GIT_OID_LEN: usize = 20;

/// An object's byte status as the database records it. A missing row is [`ObjectStatus::Absent`] — the
/// bytes were never installed, or a GC reclaimed them — so "no row" and `status = 'absent'` are one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectStatus {
    /// The bytes are durably installed and verifiable — the only readable/reusable state.
    Present,
    /// A GC has claimed the object for unlink — a non-resurrectable fence (never returns to present).
    Deleting,
    /// The bytes are not installed (no row, or a GC finalized the unlink).
    Absent,
    /// Terminal: the bytes are denylisted and may never be re-added.
    Unavailable,
}

/// The outcome of a migrate's install transition (the `absent → present` CAS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallOutcome {
    /// This call installed the object (a fresh row, or a reclaimed `absent` row brought back to present).
    Installed,
    /// The object was already present — a concurrent migrate won, or a prior version still holds it (reuse).
    AlreadyPresent,
    /// The object is mid-unlink; the caller must wait for `absent` (outside any write transaction) and retry.
    Deleting,
    /// The object is denylisted — the install is refused.
    Unavailable,
}

/// The outcome of a GC claim step (the guarded `present → deleting` CAS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimOutcome {
    /// The object was claimed for unlink; `git_oid` locates the loose object the unlink step will remove.
    Claimed { git_oid: [u8; GIT_OID_LEN] },
    /// The object was spared — it is reachable from a commit, named by a live lease, or not present.
    Spared,
}

impl Db {
    // ── object_presence: the fenced state machine ─────────────────────────────────────────────────────

    /// The current byte status of an object (a pool read; no transaction). Drives a migrate's
    /// reuse-vs-install decision and the deleting-wait poll — both OUTSIDE any write transaction.
    pub(crate) async fn object_status(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<ObjectStatus> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT status AS "status!" FROM object_presence WHERE workspace_id = ?1 AND object_id = ?2"#,
            ws_s,
            oid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(ObjectStatus::Absent),
            Some(r) => parse_status(&r.status),
        }
    }

    /// The install transition: `absent → present`, set ONLY after the caller has durably installed the
    /// bytes at their final path. One immediate-write transaction: reject a denylisted blob; then the
    /// guarded upsert (the `WHERE status = 'absent'` cannot fire on a `deleting` row, so resurrection is
    /// impossible by construction); then, if the upsert was suppressed, classify the blocking state so the
    /// caller can reuse / wait / reject. `git_oid` is the physical locator; `size` is operational only.
    pub(crate) async fn install_object(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        git_oid: &[u8; GIT_OID_LEN],
        size: i64,
        now: i64,
    ) -> Result<InstallOutcome> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let goid = git_oid.as_slice();

        let mut tx = self.begin_immediate().await?;

        // A denylisted blob is never (re-)introduced — the bytes the caller wrote stay an unreferenced
        // orphan (harmless). This is the best-effort early guard; the serializing check lands with the
        // pointer-move write.
        let tomb = sqlx::query!(
            "SELECT blob_id FROM tombstones WHERE workspace_id = ?1 AND blob_id = ?2",
            ws_s,
            oid,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        if tomb.is_some() {
            tx.rollback().await.map_err(AuthorityError::internal)?;
            return Ok(InstallOutcome::Unavailable);
        }

        // The guarded CAS: insert a fresh present row, OR bring an `absent` row back to present. A row in
        // present/deleting/unavailable is left untouched (the DO UPDATE WHERE fails → RETURNING is empty).
        let installed = sqlx::query!(
            r#"
            INSERT INTO object_presence (workspace_id, object_id, status, location, size, git_oid, status_updated_at)
            VALUES (?1, ?2, 'present', 'git', ?3, ?4, ?5)
            ON CONFLICT (workspace_id, object_id) DO UPDATE SET
                status            = 'present',
                location          = 'git',
                size              = excluded.size,
                git_oid           = excluded.git_oid,
                status_updated_at = excluded.status_updated_at
            WHERE object_presence.status = 'absent'
            RETURNING object_id AS "object_id!: Vec<u8>"
            "#,
            ws_s,
            oid,
            size,
            goid,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;

        let outcome = if installed.is_some() {
            InstallOutcome::Installed
        } else {
            // The upsert was suppressed — read the blocking state IN the same transaction (the write lock
            // is held, so no time-of-check/time-of-use gap) and classify it. RETURNING alone cannot tell
            // present/deleting/unavailable apart (all yield an empty result).
            match self.locked_status(&mut tx, ws, object_id).await? {
                ObjectStatus::Present => InstallOutcome::AlreadyPresent,
                ObjectStatus::Deleting => InstallOutcome::Deleting,
                ObjectStatus::Unavailable => InstallOutcome::Unavailable,
                // The upsert would have inserted/updated an absent/no-row case, so a suppressed-yet-absent
                // result is a store fault.
                ObjectStatus::Absent => {
                    tx.rollback().await.map_err(AuthorityError::internal)?;
                    return Err(AuthorityError::integrity(SuppressedButAbsent));
                }
            }
        };
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(outcome)
    }

    /// The GC claim step: `present → deleting`, **guarded by the exact read-authorization surface** so a
    /// readable object is never reclaimed. One immediate-write transaction re-verifies AT DELETE TIME that
    /// the object is referenced by NO commit (the `commit_object` table is what `read_object` authorizes
    /// over) and named by NO live lease — closing the snapshot-then-delete race (a lease or a commit edge
    /// added after the candidate scan but before this claim is seen here and the object is spared).
    pub(crate) async fn claim_for_delete(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        now: i64,
    ) -> Result<ClaimOutcome> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let mut tx = self.begin_immediate().await?;
        let row = sqlx::query!(
            r#"
            UPDATE object_presence SET status = 'deleting', status_updated_at = ?3
            WHERE workspace_id = ?1 AND object_id = ?2 AND status = 'present'
              AND NOT EXISTS (
                  SELECT 1 FROM commit_object WHERE workspace_id = ?1 AND object_id = ?2)
              AND NOT EXISTS (
                  SELECT 1 FROM promotion_lease_object plo
                  JOIN promotion_lease pl
                    ON pl.workspace_id = plo.workspace_id AND pl.op_id = plo.op_id
                  WHERE plo.workspace_id = ?1 AND plo.object_id = ?2
                    AND (pl.expires_at IS NULL OR pl.expires_at > ?3))
            RETURNING git_oid AS "git_oid: Vec<u8>"
            "#,
            ws_s,
            oid,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        let outcome = match row {
            None => ClaimOutcome::Spared,
            Some(r) => {
                let git_oid = git_oid_from_row(r.git_oid)?;
                ClaimOutcome::Claimed { git_oid }
            }
        };
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(outcome)
    }

    /// The GC finalize step: `deleting → absent`, after the loose object has been unlinked OUTSIDE any
    /// transaction. Guarded on `status = 'deleting'`, so it is idempotent against a concurrent recovery.
    pub(crate) async fn finalize_delete(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        now: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "UPDATE object_presence SET status = 'absent', status_updated_at = ?3 \
             WHERE workspace_id = ?1 AND object_id = ?2 AND status = 'deleting'",
            ws_s,
            oid,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Every `present` object in the workspace — the GC candidate scan (a pool read; advisory only, since
    /// the guarded claim re-verifies each candidate). Bound on `workspace_id`: an unbound scan would
    /// silently enumerate another tenant's (content-addressed, repeatable) ids.
    pub(crate) async fn present_objects(&self, ws: &WorkspaceId) -> Result<Vec<ObjectId>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT object_id AS "object_id!: Vec<u8>" FROM object_presence
               WHERE workspace_id = ?1 AND status = 'present'"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| object_id_from_row(r.object_id))
            .collect()
    }

    /// The object ids of every STALE `deleting` row in the workspace — ones a crashed GC left behind (older
    /// than the recovery threshold). This is the recovery sweep's ADVISORY candidate list (a pool read); the
    /// authoritative one-winner claim + the git locator come from [`Self::claim_stale_for_recovery`], so two
    /// concurrent sweeps never both act on the same row.
    pub(crate) async fn stale_deleting(
        &self,
        ws: &WorkspaceId,
        older_than: i64,
    ) -> Result<Vec<ObjectId>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT object_id AS "object_id!: Vec<u8>" FROM object_presence
               WHERE workspace_id = ?1 AND status = 'deleting' AND status_updated_at < ?2"#,
            ws_s,
            older_than,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| object_id_from_row(r.object_id))
            .collect()
    }

    /// Atomically claim a STALE `deleting` row for recovery — the one-winner guard that makes the recovery
    /// sweep safe under concurrency. Bumps `status_updated_at` to now (so a second concurrent sweep no
    /// longer sees it as stale) while KEEPING it `deleting`, and returns its git locator only to the winner.
    /// A `None` result means another sweeper already claimed it (or it is no longer a stale `deleting` row)
    /// — the caller must NOT unlink. Keeping the row `deleting` across the unlink preserves the
    /// unlink-before-`absent` ordering, so a concurrent migrate cannot reinstall the bytes mid-recovery.
    pub(crate) async fn claim_stale_for_recovery(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        older_than: i64,
        now: i64,
    ) -> Result<Option<[u8; GIT_OID_LEN]>> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let mut tx = self.begin_immediate().await?;
        let row = sqlx::query!(
            r#"UPDATE object_presence SET status_updated_at = ?4
               WHERE workspace_id = ?1 AND object_id = ?2 AND status = 'deleting' AND status_updated_at < ?3
               RETURNING git_oid AS "git_oid: Vec<u8>""#,
            ws_s,
            oid,
            older_than,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        let claimed = match row {
            None => None,
            Some(r) => Some(git_oid_from_row(r.git_oid)?),
        };
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(claimed)
    }

    /// Distinct workspaces holding a stale `deleting` row — the only cross-workspace read the recovery
    /// sweep runs; each id is re-parsed and the per-workspace finalize binds it.
    pub(crate) async fn workspaces_with_stale_deleting(
        &self,
        older_than: i64,
    ) -> Result<Vec<WorkspaceId>> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT workspace_id AS "workspace_id!" FROM object_presence
               WHERE status = 'deleting' AND status_updated_at < ?1"#,
            older_than,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| reparse_workspace(&r.workspace_id))
            .collect()
    }

    // ── promotion leases (the GC roots) ───────────────────────────────────────────────────────────────

    /// Insert a promotion lease over a commit's FULL object set, BEFORE any byte migrates — so a
    /// concurrent GC's keep-set already protects every needed object (even an old, already-present one a
    /// dedup-skip would otherwise leave exposed). `expires_at` is the in-flight guard (a crashed migrate's
    /// lease lapses and becomes GC-reclaimable); a successful migrate later makes it non-expiring.
    ///
    /// On op-id reuse (a retry, or a re-run with a different candidate) the child object set is REBUILT, not
    /// merged: stale rows from a prior candidate are cleared first, so a later `commit_lease` can never pin
    /// objects the current candidate does not name.
    pub(crate) async fn insert_lease(
        &self,
        ws: &WorkspaceId,
        op_id: &OpId,
        commit_id: CommitId,
        object_ids: &[ObjectId],
        expires_at: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let op_s = op_id.as_str();
        let cid = commit_id.0.as_slice();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "INSERT INTO promotion_lease (workspace_id, op_id, commit_id, expires_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (workspace_id, op_id) DO UPDATE SET commit_id = excluded.commit_id, expires_at = excluded.expires_at",
            ws_s,
            op_s,
            cid,
            expires_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        // Clear any prior child set for this op (op-id reuse) before re-inserting, so the lease names
        // exactly this candidate's objects.
        sqlx::query!(
            "DELETE FROM promotion_lease_object WHERE workspace_id = ?1 AND op_id = ?2",
            ws_s,
            op_s,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        for obj in object_ids {
            let oid = obj.0.as_slice();
            sqlx::query!(
                "INSERT INTO promotion_lease_object (workspace_id, op_id, object_id) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (workspace_id, op_id, object_id) DO NOTHING",
                ws_s,
                op_s,
                oid,
            )
            .execute(&mut *tx)
            .await
            .map_err(AuthorityError::internal)?;
        }
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Make a lease non-expiring on migrate SUCCESS, so the migrated version stays rooted until the later
    /// pointer-move consumes it (a finite TTL would let GC reclaim a good, just-migrated version).
    pub(crate) async fn commit_lease(&self, ws: &WorkspaceId, op_id: &OpId) -> Result<()> {
        let ws_s = ws.as_str();
        let op_s = op_id.as_str();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "UPDATE promotion_lease SET expires_at = NULL WHERE workspace_id = ?1 AND op_id = ?2",
            ws_s,
            op_s,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Release a lease (and, by cascade, its object rows). Used by tests + the abandoned-migrate path; the
    /// later pointer-move releases it after handing the root to `current`.
    pub(crate) async fn release_lease(&self, ws: &WorkspaceId, op_id: &OpId) -> Result<()> {
        let ws_s = ws.as_str();
        let op_s = op_id.as_str();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "DELETE FROM promotion_lease WHERE workspace_id = ?1 AND op_id = ?2",
            ws_s,
            op_s,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    // ── upload quarantine ─────────────────────────────────────────────────────────────────────────────

    /// Record an in-flight upload's quarantine objdir (the GC scanner never touches it). `objdir` is
    /// reference metadata only — the janitor rebuilds the deletion path from the validated ids.
    pub(crate) async fn insert_quarantine(
        &self,
        ws: &WorkspaceId,
        op_id: &OpId,
        objdir: &str,
        expires_at: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let op_s = op_id.as_str();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "INSERT INTO upload_quarantine (workspace_id, op_id, objdir, expires_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (workspace_id, op_id) DO UPDATE SET objdir = excluded.objdir, expires_at = excluded.expires_at",
            ws_s,
            op_s,
            objdir,
            expires_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Drop a quarantine row (after a successful migrate, or after the janitor swept its dir).
    pub(crate) async fn delete_quarantine(&self, ws: &WorkspaceId, op_id: &OpId) -> Result<()> {
        let ws_s = ws.as_str();
        let op_s = op_id.as_str();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "DELETE FROM upload_quarantine WHERE workspace_id = ?1 AND op_id = ?2",
            ws_s,
            op_s,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// The op ids of every expired/abandoned quarantine in a workspace — the janitor re-parses each into an
    /// [`OpId`] before building any `rm -rf` path.
    pub(crate) async fn expired_quarantine_ops(
        &self,
        ws: &WorkspaceId,
        now: i64,
    ) -> Result<Vec<OpId>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT op_id AS "op_id!" FROM upload_quarantine WHERE workspace_id = ?1 AND expires_at <= ?2"#,
            ws_s,
            now,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter().map(|r| reparse_op(&r.op_id)).collect()
    }

    /// Distinct workspaces holding an expired quarantine — the only cross-workspace read the janitor runs.
    pub(crate) async fn workspaces_with_expired_quarantine(
        &self,
        now: i64,
    ) -> Result<Vec<WorkspaceId>> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT workspace_id AS "workspace_id!" FROM upload_quarantine WHERE expires_at <= ?1"#,
            now,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| reparse_workspace(&r.workspace_id))
            .collect()
    }

    // ── tombstones (denylist + the unavailable terminal) ──────────────────────────────────────────────

    /// Add a blob to the denylist and, if a row exists, drive it to the `unavailable` terminal state —
    /// never interrupting an in-flight unlink (a `deleting` row is left for the GC to finish).
    pub(crate) async fn insert_tombstone(
        &self,
        ws: &WorkspaceId,
        blob_id: ObjectId,
        reason: &str,
        now: i64,
    ) -> Result<()> {
        let ws_s = ws.as_str();
        let bid = blob_id.0.as_slice();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "INSERT INTO tombstones (workspace_id, blob_id, reason, at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (workspace_id, blob_id) DO NOTHING",
            ws_s,
            bid,
            reason,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        sqlx::query!(
            "UPDATE object_presence SET status = 'unavailable', status_updated_at = ?3 \
             WHERE workspace_id = ?1 AND object_id = ?2 AND status IN ('present', 'absent')",
            ws_s,
            bid,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(AuthorityError::internal)?;
        tx.commit().await.map_err(AuthorityError::internal)?;
        Ok(())
    }

    /// Whether a blob is denylisted (the ingest early check).
    pub(crate) async fn is_tombstoned(&self, ws: &WorkspaceId, blob_id: ObjectId) -> Result<bool> {
        let ws_s = ws.as_str();
        let bid = blob_id.0.as_slice();
        let row = sqlx::query!(
            "SELECT blob_id FROM tombstones WHERE workspace_id = ?1 AND blob_id = ?2",
            ws_s,
            bid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }

    /// Read an object's status while holding the write transaction (used inside [`Self::install_object`] to
    /// classify a suppressed upsert with no time-of-check/time-of-use gap). Generic over the executor so
    /// the same query serves the held transaction.
    async fn locked_status(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<ObjectStatus> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query(
            "SELECT status FROM object_presence WHERE workspace_id = ?1 AND object_id = ?2",
        )
        .bind(ws_s)
        .bind(oid)
        .fetch_optional(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(ObjectStatus::Absent),
            Some(r) => {
                let status: String = r.try_get("status").map_err(AuthorityError::internal)?;
                parse_status(&status)
            }
        }
    }
}

/// Parse a stored status string. A bad value is store corruption (the schema CHECK forbids it).
fn parse_status(s: &str) -> Result<ObjectStatus> {
    match s {
        "present" => Ok(ObjectStatus::Present),
        "deleting" => Ok(ObjectStatus::Deleting),
        "absent" => Ok(ObjectStatus::Absent),
        "unavailable" => Ok(ObjectStatus::Unavailable),
        _ => Err(AuthorityError::integrity(BadStatus)),
    }
}

/// Convert a stored 32-byte object-id BLOB into an [`ObjectId`], or an integrity fault on a bad width.
fn object_id_from_row(bytes: Vec<u8>) -> Result<ObjectId> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadBlobWidth))?;
    Ok(ObjectId(arr))
}

/// Convert a stored git-oid BLOB into a 20-byte array. A NULL or wrong-width locator on a row the fence is
/// acting on is store corruption (a present/deleting `git` object always has its 20-byte locator set).
fn git_oid_from_row(bytes: Option<Vec<u8>>) -> Result<[u8; GIT_OID_LEN]> {
    let bytes = bytes.ok_or_else(|| AuthorityError::integrity(MissingGitOid))?;
    bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadGitOidWidth))
}

/// Re-parse a stored workspace id (a global sweep re-validates before binding it). A bad stored id is
/// corruption, mapped to an integrity fault (mirroring `commit_owners`' handling of a stored skill id).
fn reparse_workspace(s: &str) -> Result<WorkspaceId> {
    WorkspaceId::parse(s).map_err(AuthorityError::integrity)
}

/// Re-parse a stored op id (the janitor re-validates before building any `rm -rf` path).
fn reparse_op(s: &str) -> Result<OpId> {
    OpId::parse(s).map_err(AuthorityError::integrity)
}

#[derive(Debug, thiserror::Error)]
#[error("stored object status is not a known value")]
struct BadStatus;

#[derive(Debug, thiserror::Error)]
#[error("stored content id is not 32 bytes")]
struct BadBlobWidth;

#[derive(Debug, thiserror::Error)]
#[error("a fenced object has no git locator")]
struct MissingGitOid;

#[derive(Debug, thiserror::Error)]
#[error("stored git locator is not 20 bytes")]
struct BadGitOidWidth;

#[derive(Debug, thiserror::Error)]
#[error("install upsert was suppressed yet the row is absent")]
struct SuppressedButAbsent;
