//! The version/pointer SQL — the one serializable commit transaction (version rows + the
//! generation-fenced CAS), the purge, and the bundle/workspace row reclaims.
//!
//! **The commit transaction** writes what a version IS — the `version` row, its consent digest, its
//! reachability edges — and, for the publish/pointer/revert flows, runs the pointer CAS in the SAME
//! transaction, so a lost race rolls the whole write back (no half-committed version lingers after a
//! CONFLICT). The caller holds the candidate's committed promotion lease across this transaction and
//! releases it only after commit, so the GC keep-set covers the objects continuously across the
//! lease→edge handoff (no reclaim window).
//!
//! **The idempotent-CAS rule** (every mover): if the pointer already sits at `expected + 1` AND
//! points at the exact target version this request names, the move already happened — answer success
//! with the live state instead of CONFLICT. This is what makes app-side retries after a crash safe
//! without any receipt machinery vault-side. Any other mismatch is the typed
//! [`AuthorityError::Conflict`] carrying the live `(generation, version_id)`.

use sqlx::{Postgres, Transaction};

use crate::db::Db;
use crate::error::{AuthorityError, LivePointer, Result};
use crate::id::{BundleId, CommitId, ObjectId, OpId, WorkspaceId};

/// The live pointer row, as the CAS and the reads see it.
#[derive(Debug, Clone)]
pub(crate) struct PointerRow {
    pub version_id: CommitId,
    pub generation: u64,
    pub moved_at_ms: i64,
    pub moved_by: String,
}

/// What one commit transaction did.
#[derive(Debug, Clone)]
pub(crate) struct CommitTxnOutcome {
    /// The version row already existed (an identical candidate re-committed — the idempotent success).
    pub deduped: bool,
    /// The pointer state after the move (present only when a CAS was requested).
    pub pointer: Option<PointerRow>,
    /// Whether a requested CAS resolved through the idempotent-replay carve-out (the pointer already
    /// sat at `expected + 1` naming this exact version).
    pub replayed: bool,
}

/// What a version-commit transaction is asked to do with the pointer.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PointerAction {
    /// Leave the pointer alone (the propose path: a version that `current` does not point at).
    None,
    /// Compare-and-set the pointer to the committed version. `None` = genesis (create the pointer at
    /// generation 1); `Some(g)` = the pointer must sit at generation `g` (and, for a candidate with a
    /// declared parent, name that parent — the same-bundle lineage rule).
    Cas(Option<u64>),
}

/// One candidate's identity facts, as the commit transaction records them.
#[derive(Debug, Clone)]
pub(crate) struct VersionFacts<'a> {
    pub version_id: CommitId,
    pub parent: Option<CommitId>,
    pub attribution: &'a str,
    pub bundle_digest_hex: &'a str,
    /// The distinct object ids of the candidate's tree (the reachability edges + the availability set).
    pub object_ids: &'a [ObjectId],
    /// The ingest op whose committed lease roots the objects (checked live inside the transaction).
    pub op_id: &'a OpId,
}

impl Db {
    /// The ONE commit transaction: probe → (insert version + digest + edges) → optional CAS.
    /// See the module docs for the idempotent-CAS rule; a lost CAS returns
    /// [`AuthorityError::Conflict`] and the rollback removes this transaction's version rows.
    pub(crate) async fn commit_version(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        facts: &VersionFacts<'_>,
        action: PointerAction,
        now: i64,
    ) -> Result<CommitTxnOutcome> {
        run_serializable!(self, tx, {
            commit_version_txn(&mut tx, ws, bundle, facts, action, now).await
        })
    }

    /// CAS the pointer to an EXISTING version (the approve path). The target must exist in this
    /// bundle and be un-purged, and every object it reaches must be `present` (a belt — a committed
    /// version's objects are present by construction).
    pub(crate) async fn move_pointer(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
        expected: Option<u64>,
        attribution: &str,
        now: i64,
    ) -> Result<CommitTxnOutcome> {
        run_serializable!(self, tx, {
            move_pointer_txn(&mut tx, ws, bundle, version, expected, attribution, now).await
        })
    }

    /// The purge transaction: stamp `purged_at` on the version (dropping its reachability edges out
    /// of the GC keep-set) and denylist the blobs UNIQUE to it (no other non-purged version in the
    /// workspace reaches them). Returns the unique blob set for the caller's targeted reclaim.
    /// Refusals: an unknown version is [`AuthorityError::NotFound`]; a pointed-at version is
    /// [`AuthorityError::PointedAt`]; an already-purged version is the idempotent empty success.
    pub(crate) async fn purge_version(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
        reason: &str,
        now: i64,
    ) -> Result<Vec<ObjectId>> {
        run_serializable!(self, tx, {
            purge_version_txn(&mut tx, ws, bundle, version, reason, now).await
        })
    }

    /// Drop every row of one bundle (pointer, digests, edges, versions, upload audit) — the app
    /// already decided the deletion; the vault reclaims. Returns the number of version rows dropped.
    /// The caller runs a GC pass afterward to reclaim the newly-unrooted bytes.
    pub(crate) async fn delete_bundle_rows(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
    ) -> Result<u64> {
        run_serializable!(self, tx, delete_bundle_rows_txn(&mut tx, ws, bundle).await)
    }

    /// Drop every row of one workspace across all custody tables. The caller removes the physical
    /// stores afterward.
    pub(crate) async fn delete_workspace_rows(&self, ws: &WorkspaceId) -> Result<()> {
        run_serializable!(self, tx, delete_workspace_rows_txn(&mut tx, ws).await)
    }
}

/// Read the pointer row inside the transaction.
async fn read_pointer_txn(
    tx: &mut Transaction<'_, Postgres>,
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
    .fetch_optional(&mut **tx)
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

/// The version row's purge state: `None` = no row; `Some(false)` = live; `Some(true)` = purged.
async fn version_state_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
) -> Result<Option<bool>> {
    let ws_s = ws.as_str();
    let b_s = bundle.as_str();
    let v_s = version.to_hex();
    let row = sqlx::query!(
        r#"SELECT (purged_at IS NOT NULL) AS "purged!" FROM version
           WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3"#,
        ws_s,
        b_s,
        v_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.map(|r| r.purged))
}

/// The body of [`Db::commit_version`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn commit_version_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    bundle: &BundleId,
    facts: &VersionFacts<'_>,
    action: PointerAction,
    now: i64,
) -> Result<CommitTxnOutcome> {
    let ws_s = ws.as_str();
    let b_s = bundle.as_str();
    let v_hex = facts.version_id.to_hex();

    // Probe: an identical candidate re-committed is the idempotent success; a PURGED version's hash
    // stays purged (its bytes are gone by decision — refuse the re-introduction typed).
    let deduped = match version_state_txn(tx, ws, bundle, facts.version_id).await? {
        Some(true) => return Err(AuthorityError::TargetPurged),
        Some(false) => true,
        None => false,
    };

    if !deduped {
        // The declared parent must be a version of THIS bundle (the same-bundle sanity fence — the
        // git repo is per-workspace, so the repo alone cannot hold this line).
        if let Some(parent) = facts.parent
            && version_state_txn(tx, ws, bundle, parent).await?.is_none()
        {
            return Err(AuthorityError::RejectedUpload(
                "the declared parent is not a version of this bundle".to_owned(),
            ));
        }

        // Availability: every candidate object `present` and none denylisted, and the ingest's lease
        // COMMITTED (the durable proof the migrate finished) — all inside the serializable snapshot.
        let oids: Vec<Vec<u8>> = facts.object_ids.iter().map(|o| o.0.to_vec()).collect();
        let present = sqlx::query!(
            r#"SELECT count(*) AS "n!" FROM object_presence
               WHERE workspace_id = $1 AND status = 'present' AND object_id = ANY($2)"#,
            ws_s,
            &oids,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        if present.n != i64::try_from(facts.object_ids.len()).map_err(AuthorityError::internal)? {
            return Err(AuthorityError::RejectedUpload(
                "a candidate object is not present in the store".to_owned(),
            ));
        }
        let tombed = sqlx::query!(
            r#"SELECT count(*) AS "n!" FROM tombstones
               WHERE workspace_id = $1 AND blob_id = ANY($2)"#,
            ws_s,
            &oids,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        if tombed.n != 0 {
            return Err(AuthorityError::RejectedUpload(
                "a candidate blob is on the denylist".to_owned(),
            ));
        }
        let cid = facts.version_id.0.as_slice();
        let op_s = facts.op_id.as_str();
        let lease = sqlx::query!(
            r#"SELECT 1::int8 AS "one!: i64" FROM promotion_lease
               WHERE workspace_id = $1 AND op_id = $2 AND commit_id = $3 AND expires_at IS NULL"#,
            ws_s,
            op_s,
            cid,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        if lease.is_none() {
            return Err(AuthorityError::internal(LeaseNotCommitted));
        }

        // The version row: version_id = commit_id (a version IS its commit today; the schema carries
        // both columns so the identities could ever diverge without a schema change). first_parent
        // is persisted so the promote path can fence lineage without re-reading the commit frame.
        let parent_hex = facts.parent.map(|p| p.to_hex());
        sqlx::query!(
            "INSERT INTO version (workspace_id, bundle_id, version_id, commit_id, first_parent, author_display, created_at) \
             VALUES ($1, $2, $3, $3, $4, $5, to_timestamp($6::double precision / 1000.0)) \
             ON CONFLICT (workspace_id, bundle_id, version_id) DO NOTHING",
            ws_s,
            b_s,
            v_hex,
            parent_hex.as_deref(),
            facts.attribution,
            now as f64,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        sqlx::query!(
            "INSERT INTO version_digest (workspace_id, bundle_id, version_id, bundle_digest) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (workspace_id, bundle_id, version_id) DO NOTHING",
            ws_s,
            b_s,
            v_hex,
            facts.bundle_digest_hex,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        sqlx::query!(
            "INSERT INTO version_object (workspace_id, bundle_id, version_id, object_id) \
             SELECT $1, $2, $3, UNNEST($4::bytea[]) \
             ON CONFLICT (workspace_id, bundle_id, version_id, object_id) DO NOTHING",
            ws_s,
            b_s,
            v_hex,
            &oids,
        )
        .execute(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
    }

    let (pointer, replayed) = match action {
        PointerAction::None => (None, false),
        PointerAction::Cas(expected) => {
            let (row, replayed) = cas_txn(
                tx,
                ws,
                bundle,
                facts.version_id,
                expected,
                facts.parent,
                true,
                facts.attribution,
                now,
            )
            .await?;
            (Some(row), replayed)
        }
    };

    Ok(CommitTxnOutcome {
        deduped,
        pointer,
        replayed,
    })
}

/// The pointer compare-and-set — the ONE mover every flow terminates in. `enforce_lineage` is set by
/// the publish path (the new commit's first parent must be the currently pointed version when
/// `expected` is `Some`); the approve path moves to an existing version with no lineage assert (the
/// app decided the review).
#[allow(clippy::too_many_arguments)]
async fn cas_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    bundle: &BundleId,
    target: CommitId,
    expected: Option<u64>,
    parent: Option<CommitId>,
    enforce_lineage: bool,
    attribution: &str,
    now: i64,
) -> Result<(PointerRow, bool)> {
    let ws_s = ws.as_str();
    let b_s = bundle.as_str();
    let live = read_pointer_txn(tx, ws, bundle).await?;

    match (live, expected) {
        // Genesis: create the pointer at generation 1.
        (None, None) => {
            let v_hex = target.to_hex();
            sqlx::query!(
                "INSERT INTO current_pointer (workspace_id, bundle_id, version_id, generation, moved_by_display, moved_at) \
                 VALUES ($1, $2, $3, 1, $4, to_timestamp($5::double precision / 1000.0))",
                ws_s,
                b_s,
                v_hex,
                attribution,
                now as f64,
            )
            .execute(&mut **tx)
            .await
            .map_err(AuthorityError::internal)?;
            Ok((
                PointerRow {
                    version_id: target,
                    generation: 1,
                    moved_at_ms: now,
                    moved_by: attribution.to_owned(),
                },
                false,
            ))
        }
        // Genesis-idempotent: the pointer exists at generation 1 naming exactly this version — the
        // create already happened (a crashed caller's retry). Success with the live state.
        (Some(row), None) if row.generation == 1 && row.version_id == target => Ok((row, true)),
        (Some(row), None) => Err(AuthorityError::Conflict(Some(LivePointer {
            generation: row.generation,
            version_id: row.version_id,
        }))),
        (None, Some(_)) => Err(AuthorityError::Conflict(None)),
        (Some(row), Some(g)) => {
            if row.generation == g {
                // The same-bundle lineage fence (publish only): the new commit's first parent must
                // be the version the pointer names at the expected generation.
                if enforce_lineage && parent != Some(row.version_id) {
                    return Err(AuthorityError::RejectedUpload(
                        "the candidate's first parent is not the currently pointed version"
                            .to_owned(),
                    ));
                }
                let v_hex = target.to_hex();
                let next = i64::try_from(g + 1).map_err(AuthorityError::internal)?;
                sqlx::query!(
                    "UPDATE current_pointer SET version_id = $3, generation = $4, \
                         moved_by_display = $5, moved_at = to_timestamp($6::double precision / 1000.0) \
                     WHERE workspace_id = $1 AND bundle_id = $2",
                    ws_s,
                    b_s,
                    v_hex,
                    next,
                    attribution,
                    now as f64,
                )
                .execute(&mut **tx)
                .await
                .map_err(AuthorityError::internal)?;
                Ok((
                    PointerRow {
                        version_id: target,
                        generation: g + 1,
                        moved_at_ms: now,
                        moved_by: attribution.to_owned(),
                    },
                    false,
                ))
            } else if row.generation == g + 1 && row.version_id == target {
                // The idempotent-CAS carve-out: the exact move this request names already landed.
                Ok((row, true))
            } else {
                Err(AuthorityError::Conflict(Some(LivePointer {
                    generation: row.generation,
                    version_id: row.version_id,
                })))
            }
        }
    }
}

/// The body of [`Db::move_pointer`], extracted for the macro.
async fn move_pointer_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
    expected: Option<u64>,
    attribution: &str,
    now: i64,
) -> Result<CommitTxnOutcome> {
    match version_state_txn(tx, ws, bundle, version).await? {
        None => return Err(AuthorityError::NotFound),
        Some(true) => return Err(AuthorityError::TargetPurged),
        Some(false) => {}
    }
    // Availability belt: a committed version's objects are present by construction, but a crash-lost
    // store (or a mid-flight purge of a SHARED blob) must fail typed here, never as a follower's
    // integrity fault after the move.
    let ws_s = ws.as_str();
    let b_s = bundle.as_str();
    let v_hex = version.to_hex();
    let missing = sqlx::query!(
        r#"SELECT count(*) AS "n!" FROM version_object vo
           WHERE vo.workspace_id = $1 AND vo.bundle_id = $2 AND vo.version_id = $3
             AND NOT EXISTS (
                 SELECT 1 FROM object_presence op
                 WHERE op.workspace_id = vo.workspace_id AND op.object_id = vo.object_id
                   AND op.status = 'present')"#,
        ws_s,
        b_s,
        v_hex,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if missing.n != 0 {
        return Err(AuthorityError::RejectedUpload(
            "a target object is not present in the store".to_owned(),
        ));
    }
    // The lineage fence, enforced here as it is on the publish path: promoting an existing version
    // (the approve path) is refused unless the candidate's first parent is exactly the version the
    // pointer currently names. Without it, approving a proposal whose base advanced since it was
    // opened would silently fast-forward over the intervening version, discarding it. A mismatch is
    // the same typed CONFLICT (carrying the live pointer) a stale generation raises — the
    // reviewer's remedy is identical: re-review against the moved current. The parent is read from
    // the row (persisted at commit); a genesis candidate (NULL parent) may only promote onto an
    // empty pointer.
    let parent_hex = sqlx::query_scalar!(
        "SELECT first_parent FROM version \
         WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3",
        ws_s,
        b_s,
        v_hex,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let parent = match parent_hex.as_deref() {
        None => None,
        Some(hex) => Some(
            CommitId::parse_hex(hex).ok_or_else(|| AuthorityError::internal(BadStoredParent))?,
        ),
    };
    match read_pointer_txn(tx, ws, bundle).await? {
        // The pointer already names the target: a replay (idempotent) or a same-target conflict —
        // cas_txn decides on generation; the lineage fence does not apply to a non-move.
        Some(live) if live.version_id == version => {}
        // A real move onto a different version: the candidate's first parent MUST be the version
        // the pointer currently names, else the base has advanced — CONFLICT with the live pointer.
        Some(live) if parent != Some(live.version_id) => {
            return Err(AuthorityError::Conflict(Some(LivePointer {
                generation: live.generation,
                version_id: live.version_id,
            })));
        }
        // A candidate that declares a parent cannot be the bundle's genesis version.
        None if parent.is_some() => {
            return Err(AuthorityError::Conflict(None));
        }
        _ => {}
    }
    // Lineage is proven; cas_txn does the generation CAS + the move (and the idempotent-CAS replay).
    let (row, replayed) = cas_txn(
        tx,
        ws,
        bundle,
        version,
        expected,
        None,
        false,
        attribution,
        now,
    )
    .await?;
    Ok(CommitTxnOutcome {
        deduped: true,
        pointer: Some(row),
        replayed,
    })
}

/// The body of [`Db::purge_version`], extracted for the macro.
async fn purge_version_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
    reason: &str,
    now: i64,
) -> Result<Vec<ObjectId>> {
    match version_state_txn(tx, ws, bundle, version).await? {
        None => return Err(AuthorityError::NotFound),
        Some(true) => return Ok(Vec::new()), // already purged — idempotent
        Some(false) => {}
    }
    if let Some(pointer) = read_pointer_txn(tx, ws, bundle).await?
        && pointer.version_id == version
    {
        return Err(AuthorityError::PointedAt);
    }

    let ws_s = ws.as_str();
    let b_s = bundle.as_str();
    let v_hex = version.to_hex();

    // The blobs UNIQUE to this version: reached by it and by no OTHER non-purged version anywhere in
    // the workspace (dedup is workspace-wide, so a blob shared with another bundle stays).
    let unique = sqlx::query!(
        r#"SELECT vo.object_id AS "object_id!: Vec<u8>" FROM version_object vo
           WHERE vo.workspace_id = $1 AND vo.bundle_id = $2 AND vo.version_id = $3
             AND NOT EXISTS (
                 SELECT 1 FROM version_object vo2
                 JOIN version v2
                   ON v2.workspace_id = vo2.workspace_id AND v2.bundle_id = vo2.bundle_id
                  AND v2.version_id = vo2.version_id
                 WHERE vo2.workspace_id = $1 AND vo2.object_id = vo.object_id
                   AND NOT (vo2.bundle_id = $2 AND vo2.version_id = $3)
                   AND v2.purged_at IS NULL)"#,
        ws_s,
        b_s,
        v_hex,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let unique: Vec<ObjectId> = unique
        .into_iter()
        .map(|r| super::lifecycle::object_id_from_row(r.object_id))
        .collect::<Result<_>>()?;

    sqlx::query!(
        "UPDATE version SET purged_at = to_timestamp($4::double precision / 1000.0) \
         WHERE workspace_id = $1 AND bundle_id = $2 AND version_id = $3",
        ws_s,
        b_s,
        v_hex,
        now as f64,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    // The denylist: ingest and the install CAS both refuse these blobs from now on, and the GC
    // finalize lands them on the terminal `unavailable`. The purge attribution is recorded verbatim
    // as the tombstone's reason (the who/why evidence beside the version row's `purged_at`).
    let bids: Vec<Vec<u8>> = unique.iter().map(|o| o.0.to_vec()).collect();
    sqlx::query!(
        "INSERT INTO tombstones (workspace_id, blob_id, reason, at) \
         SELECT $1, UNNEST($2::bytea[]), $3, $4 \
         ON CONFLICT (workspace_id, blob_id) DO NOTHING",
        ws_s,
        &bids,
        reason,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // An already-`absent` blob has no bytes to reclaim — land it on the terminal directly. A
    // `present` one is left for the caller's targeted acquire → unlink → finalize (the fence owns the
    // physical bytes; `deleting` is never interrupted).
    sqlx::query!(
        "UPDATE object_presence SET status = 'unavailable', status_updated_at = $3 \
         WHERE workspace_id = $1 AND object_id = ANY($2) AND status = 'absent'",
        ws_s,
        &bids,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    Ok(unique)
}

/// The body of [`Db::delete_bundle_rows`], extracted for the macro. Children first (the FKs); the
/// upload audit rows go with the bundle.
async fn delete_bundle_rows_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    bundle: &BundleId,
) -> Result<u64> {
    let ws_s = ws.as_str();
    let b_s = bundle.as_str();
    sqlx::query!(
        "DELETE FROM current_pointer WHERE workspace_id = $1 AND bundle_id = $2",
        ws_s,
        b_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "DELETE FROM version_object WHERE workspace_id = $1 AND bundle_id = $2",
        ws_s,
        b_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "DELETE FROM version_digest WHERE workspace_id = $1 AND bundle_id = $2",
        ws_s,
        b_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let dropped = sqlx::query!(
        "DELETE FROM version WHERE workspace_id = $1 AND bundle_id = $2",
        ws_s,
        b_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "DELETE FROM upload WHERE workspace_id = $1 AND bundle_id = $2",
        ws_s,
        b_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(dropped.rows_affected())
}

/// The body of [`Db::delete_workspace_rows`], extracted for the macro. Children first (the FKs).
async fn delete_workspace_rows_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
) -> Result<()> {
    let ws_s = ws.as_str();
    for stmt in [
        "DELETE FROM current_pointer WHERE workspace_id = $1",
        "DELETE FROM version_object WHERE workspace_id = $1",
        "DELETE FROM version_digest WHERE workspace_id = $1",
        "DELETE FROM version WHERE workspace_id = $1",
        "DELETE FROM upload WHERE workspace_id = $1",
        "DELETE FROM promotion_lease WHERE workspace_id = $1", // cascades promotion_lease_object
        "DELETE FROM object_presence WHERE workspace_id = $1",
        "DELETE FROM tombstones WHERE workspace_id = $1",
    ] {
        sqlx::query(stmt)
            .bind(ws_s)
            .execute(&mut **tx)
            .await
            .map_err(AuthorityError::internal)?;
    }
    Ok(())
}

/// Parse a stored version-id hex column. A malformed stored id is corruption (the writes only ever
/// store the canonical 64-hex spelling).
pub(in crate::db) fn parse_stored_version(s: &str) -> Result<CommitId> {
    CommitId::parse_hex(s).ok_or_else(|| AuthorityError::integrity(BadStoredVersionId))
}

#[derive(Debug, thiserror::Error)]
#[error("stored version id is not 64 lowercase hex characters")]
struct BadStoredVersionId;

#[derive(Debug, thiserror::Error)]
#[error("the ingest lease is not committed — the commit must not proceed")]
struct LeaseNotCommitted;

#[derive(Debug, thiserror::Error)]
#[error("stored first_parent is not 64 lowercase hex characters")]
struct BadStoredParent;
