//! The object-lifecycle SQL — the fenced `object_presence` state machine, promotion leases, the upload
//! quarantine, and tombstones. Raw `sqlx` stays here (a child of `mod db`); every method takes the
//! validated id newtypes + an explicit `now` and returns plain domain values, so no `sqlx` type crosses
//! the module boundary and no caller can run an unbound query. The database is the sole authority for an
//! object's byte status; the git store always trails it.

use std::collections::HashMap;

use sqlx::{Postgres, Transaction};

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
    /// The object was claimed for unlink. `location` selects which store the unlink targets; `git_oid`
    /// locates the loose git object (used when `location` is [`Location::Git`]; for an offloaded object the
    /// unlink keys on the object id, and `git_oid` is the carrier value the row always carries).
    Claimed {
        location: Location,
        git_oid: [u8; GIT_OID_LEN],
    },
    /// The object was spared — it is reachable from a commit, named by a live lease, or not present.
    Spared,
}

/// Which physical store holds an object's bytes. The database is the sole authority for this; only the
/// physical fetch/install/unlink dispatches on it — reachability (`commit_object`) and access reference the
/// `object_id` regardless of where the bytes sit. `large-remote` is schema-reserved for the deferred
/// S3-compatible backend; v0 never writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Location {
    /// A loose object in the per-workspace git store (physically located by its `git_oid`).
    Git,
    /// Offloaded to the per-workspace local large-object store (physically located by its `object_id`; the
    /// `git_oid` is still recorded as the tree-entry bridge key the render walk joins on).
    LargeLocal,
}

impl Location {
    /// The stored string form (matches the `object_presence.location` CHECK constraint).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Location::Git => "git",
            Location::LargeLocal => "large-local",
        }
    }
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
            r#"SELECT status AS "status!" FROM object_presence WHERE workspace_id = $1 AND object_id = $2"#,
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

    /// The recorded physical [`Location`] of a **`present`** object (a pool read; no transaction). `None`
    /// means there is no *live* location — no row at all, one never installed, OR a non-`present` row (a
    /// reclaimed `absent`/`deleting`/`unavailable` one). The caller treats `None` as `git`. **The
    /// `status = 'present'` filter is load-bearing for reads:** after a large-local object is GC'd its row
    /// lingers as `absent` with `location = 'large-local'`; the filter makes this report no live location for
    /// it, so a read can never be routed to the deleted side-store object. Drives the migrate dedup-reuse belt
    /// (always called on a `present` row, so the filter is transparent there) and the single-object read dispatch.
    pub(crate) async fn object_location(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<Option<Location>> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT location AS "location!" FROM object_presence
               WHERE workspace_id = $1 AND object_id = $2 AND status = 'present'"#,
            ws_s,
            oid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(parse_location(&r.location)?)),
        }
    }

    /// The [`Location`] **and** git locator of a **`present`** object — `(location, git_oid)` for a present
    /// row, else `None` (no live row: a legacy git object, or one never installed / reclaimed). This drives
    /// the single-object read dispatch: the git arm reads the loose object **directly by `git_oid`** rather
    /// than walking the version's tree, so a git-resident object in a MIXED bundle — one that also contains
    /// an offloaded blob whose git object is intentionally absent — reads correctly (a whole-tree re-hash
    /// would fault on the absent offloaded sibling before reaching the requested blob). `None` falls back to
    /// the tree walk, which is safe there because a no-row object's version is all-git (legacy).
    pub(crate) async fn object_dispatch(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<Option<(Location, [u8; GIT_OID_LEN])>> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT location AS "location!", git_oid AS "git_oid: Vec<u8>"
               FROM object_presence
               WHERE workspace_id = $1 AND object_id = $2 AND status = 'present'"#,
            ws_s,
            oid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some((
                parse_location(&r.location)?,
                git_oid_from_row(r.git_oid)?,
            ))),
        }
    }

    /// The install transition: `absent → present`, set ONLY after the caller has durably installed the
    /// bytes at their final path **in the store named by `location`**. One immediate-write transaction:
    /// reject a denylisted blob; then the guarded upsert (the `WHERE status = 'absent'` cannot fire on a
    /// `deleting` row, so resurrection is impossible by construction); then, if the upsert was suppressed,
    /// classify the blocking state so the caller can reuse / wait / reject. `git_oid` is always recorded —
    /// the loose-object locator for a `git` object, and the tree-entry bridge key (for the render walk) for
    /// a `large-local` one; `size` is operational only. Routing decides `location`; the CAS, the fence, and
    /// the non-resurrectable `deleting` guard are unchanged by it.
    pub(crate) async fn install_object(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        location: Location,
        git_oid: &[u8; GIT_OID_LEN],
        size: i64,
        now: i64,
    ) -> Result<InstallOutcome> {
        run_serializable!(self, tx, {
            install_object_txn(&mut tx, self, ws, object_id, location, git_oid, size, now).await
        })
    }

    /// The GC claim step: `present → deleting`, **guarded by the exact read-authorization surface** so a
    /// readable object is never reclaimed. One immediate-write transaction re-verifies AT DELETE TIME that
    /// the object is kept by NONE of the three roots — closing the snapshot-then-delete race (a root added
    /// after the candidate scan but before this claim is seen here and the object is spared):
    /// - **no `commit_object` edge** (the accepted trunk — what `read_object`'s trunk arm authorizes over),
    /// - **no live `promotion_lease`** (an in-flight migrate's pre-rooted set), and
    /// - **no OPEN, NON-STALE proposal** rooting it via `proposal_object` (the pending-review surface). This
    ///   last `NOT EXISTS` shares its `open ∧ base == current` predicate **verbatim** with the read arm
    ///   ([`super::Db::authorize_object_read`]) and the recovery claim
    ///   ([`Self::claim_stale_for_recovery`]), so the instant a publish advances `current` (staling the
    ///   proposal) the object drops out of retention AND read in the same step — no reaper, no edge deletion,
    ///   no window (the equivalence test pins the copies together).
    pub(crate) async fn claim_for_delete(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        now: i64,
    ) -> Result<ClaimOutcome> {
        run_serializable!(
            self,
            tx,
            claim_for_delete_txn(&mut tx, ws, object_id, now).await
        )
    }

    /// The GC finalize step: `deleting → absent`, after the loose object has been unlinked OUTSIDE any
    /// transaction. Guarded on `status = 'deleting'` **and the claimant's fence token** (`status_updated_at =
    /// claim_token`, the value this actor's own claim stamped) — so it is idempotent against a concurrent
    /// recovery AND can never flip a row a recovery sweep has since re-claimed (a re-claim bumps the token):
    /// only the current owner finalizes. A superseded finalize matches no row — a harmless no-op. The token
    /// is the `now`/`older_than` the claimant passed to [`Self::claim_for_delete`] /
    /// [`Self::claim_stale_for_recovery`] (each stamps it into `status_updated_at`), so the caller already
    /// holds it — no value is threaded back out of the claim.
    pub(crate) async fn finalize_delete(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        claim_token: i64,
        now: i64,
    ) -> Result<()> {
        run_serializable!(self, tx, {
            finalize_delete_txn(&mut tx, ws, object_id, claim_token, now).await
        })
    }

    /// Whether THIS actor still owns the `deleting` claim it took — `status = 'deleting'` AND the row's
    /// `status_updated_at` still equals the `claim_token` the claimant stamped. The GC + recovery unlink
    /// steps consult this IMMEDIATELY before the physical `delete_loose_object` (with no `.await` between the
    /// check and the synchronous unlink), so a row a concurrent recovery sweep re-claimed — which bumps the
    /// token — is never also unlinked by the superseded original claimant. Exactly one actor ever removes the
    /// bytes, closing the recovery-vs-live-GC race that would otherwise unlink a row another actor finalized
    /// and a re-migrate then re-installed (a phantom-`present` byte loss). A pool read: it sees the latest
    /// committed claim.
    pub(crate) async fn confirm_deleting_owner(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        claim_token: i64,
    ) -> Result<bool> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT 1::int8 AS "one!: i64" FROM object_presence
               WHERE workspace_id = $1 AND object_id = $2 AND status = 'deleting' AND status_updated_at = $3"#,
            ws_s,
            oid,
            claim_token,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }

    /// The GC pass's ADVISORY candidate list: every `present` object the keep-set does NOT currently root —
    /// the SAME three exclusion clauses [`Self::claim_for_delete`] re-verifies (a `commit_object` trunk
    /// edge, a live promotion lease, an open-non-stale `proposal_object` root), evaluated here as one
    /// indexed SQL anti-join so a pass does work proportional to actual garbage, not to every object the
    /// workspace ever accumulated. Purely advisory (a point-in-time pool read): the guarded per-object
    /// claim remains the SOLE authority — an object rooted between this scan and the claim is spared there,
    /// and one unrooted after the scan is simply picked up by the next pass. Bound on `workspace_id`: an
    /// unbound scan would silently enumerate another tenant's (content-addressed, repeatable) ids.
    pub(crate) async fn gc_candidates(&self, ws: &WorkspaceId, now: i64) -> Result<Vec<ObjectId>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"
            SELECT op.object_id AS "object_id!: Vec<u8>" FROM object_presence op
            WHERE op.workspace_id = $1 AND op.status = 'present'
              AND NOT EXISTS (
                  SELECT 1 FROM commit_object co
                  WHERE co.workspace_id = op.workspace_id AND co.object_id = op.object_id)
              AND NOT EXISTS (
                  SELECT 1 FROM promotion_lease_object plo
                  JOIN promotion_lease pl
                    ON pl.workspace_id = plo.workspace_id AND pl.op_id = plo.op_id
                  WHERE plo.workspace_id = op.workspace_id AND plo.object_id = op.object_id
                    AND (pl.expires_at IS NULL OR pl.expires_at > $2))
              AND NOT EXISTS (
                  SELECT 1 FROM proposal_object po
                  JOIN proposals p ON p.workspace_id = po.workspace_id AND p.id = po.proposal_id
                  JOIN current   c ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                  WHERE po.workspace_id = op.workspace_id AND po.object_id = op.object_id
                    AND p.status = 'open' AND c.epoch = p.base_epoch AND c.seq = p.base_seq)
            "#,
            ws_s,
            now,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| object_id_from_row(r.object_id))
            .collect()
    }

    /// Every PRESENT **large-local** object in the workspace as `(git_oid, object_id)` — the
    /// location-dispatching render's offloaded set. Render anchors on the version's git tree structure
    /// (`(path, mode, git_oid)` per file); a tree entry whose `git_oid` is in this set is offloaded and is
    /// fetched from the large store by its `object_id`, while a git-resident leaf recovers its id by rehash
    /// with NO database dependency. Big blobs are rare, so this set is small — no `git_oid` index is needed
    /// (it uses the existing `(workspace_id, status)` index, then filters to `large-local`).
    pub(crate) async fn large_local_objects(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Vec<([u8; GIT_OID_LEN], ObjectId)>> {
        let ws_s = ws.as_str();
        let rows = sqlx::query!(
            r#"SELECT git_oid AS "git_oid: Vec<u8>", object_id AS "object_id!: Vec<u8>"
               FROM object_presence
               WHERE workspace_id = $1 AND status = 'present' AND location = 'large-local'"#,
            ws_s,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    git_oid_from_row(r.git_oid)?,
                    object_id_from_row(r.object_id)?,
                ))
            })
            .collect()
    }

    /// The PRESENT objects among exactly the given `git_oid` locators, as `git_oid -> object_id` — the
    /// version-metadata read's join table. Each tree leaf's `git_oid` (the loose-object id for a
    /// git-resident blob, the tree-entry bridge key for an offloaded one — both stored in
    /// `object_presence.git_oid`) resolves to its content id WITHOUT reading any blob bytes (pure
    /// metadata). Filtered to the requested leaves with an array bind (`git_oid = ANY($2)`) over the
    /// `(workspace_id, git_oid)` index, so the read scales with the requested version's tree — never with
    /// the workspace's lifetime present-object count. Bound on `workspace_id`.
    pub(crate) async fn objects_by_git_oids(
        &self,
        ws: &WorkspaceId,
        git_oids: &[[u8; GIT_OID_LEN]],
    ) -> Result<HashMap<[u8; GIT_OID_LEN], [u8; 32]>> {
        let ws_s = ws.as_str();
        let goids: Vec<Vec<u8>> = git_oids.iter().map(|g| g.to_vec()).collect();
        let rows = sqlx::query!(
            r#"SELECT git_oid AS "git_oid: Vec<u8>", object_id AS "object_id!: Vec<u8>"
               FROM object_presence
               WHERE workspace_id = $1 AND status = 'present' AND git_oid = ANY($2)"#,
            ws_s,
            &goids,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        let mut map = HashMap::with_capacity(rows.len());
        for r in rows {
            map.insert(
                git_oid_from_row(r.git_oid)?,
                object_id_from_row(r.object_id)?.0,
            );
        }
        Ok(map)
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
               WHERE workspace_id = $1 AND status = 'deleting' AND status_updated_at < $2"#,
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
    /// A `None` result means another sweeper already claimed it (or it is no longer a stale unrooted
    /// `deleting` row) — the caller must NOT unlink. Keeping the row `deleting` across the unlink preserves
    /// the unlink-before-`absent` ordering, so a concurrent migrate cannot reinstall the bytes mid-recovery.
    ///
    /// Re-verifies the **read-authorization surface AT DELETE TIME** — the `commit_object` trunk edge AND the
    /// open-non-stale `proposal_object` root (the same two read arms, verbatim) — exactly as
    /// [`Self::claim_for_delete`] does, so a stale `deleting` row that became read-authorized after the
    /// crashed claim is spared rather than unlinked. This re-check is DEFENSIVE: the promote/handoff (the only
    /// `commit_object` writer) holds the candidate's lease across the edge write, so a normally-migrated edge
    /// never lands on a `deleting` object — but re-verifying at delete time keeps the recovery sweep's keep-set
    /// == the read surface unconditionally, so a recovery unlink can never reclaim a now-readable object's
    /// bytes (byte loss). A spared row is left `deleting` (its `status_updated_at` un-bumped); since `deleting`
    /// is non-resurrectable the bytes stay on disk + readable, while a re-migrate of that exact content is the
    /// only blocked op (a rare, no-data-loss residual the lease→edge handoff removes).
    ///
    /// Unlike `claim_for_delete`, this deliberately does **NOT** check the promotion lease. A live lease over
    /// a `present` object means "in use, do not reclaim"; but over a *`deleting`* object it means a migrate's
    /// `install_one` is **waiting** for recovery to flip it to `absent` so it can re-copy (the migrate leased
    /// its full set *before* the wait). Sparing it on the lease would strand that waiter until the lease TTL
    /// lapses. A lease alone is not readable (the read path authorizes via `commit_object`, never a lease), so
    /// finalizing a leased-but-unedged `deleting` row loses no readable bytes — the waiter simply re-installs.
    pub(crate) async fn claim_stale_for_recovery(
        &self,
        ws: &WorkspaceId,
        object_id: ObjectId,
        older_than: i64,
        now: i64,
    ) -> Result<Option<(Location, [u8; GIT_OID_LEN])>> {
        run_serializable!(self, tx, {
            claim_stale_for_recovery_txn(&mut tx, ws, object_id, older_than, now).await
        })
    }

    /// Distinct workspaces holding a stale `deleting` row — the only cross-workspace read the recovery
    /// sweep runs; each id is re-parsed and the per-workspace finalize binds it.
    pub(crate) async fn workspaces_with_stale_deleting(
        &self,
        older_than: i64,
    ) -> Result<Vec<WorkspaceId>> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT workspace_id AS "workspace_id!" FROM object_presence
               WHERE status = 'deleting' AND status_updated_at < $1"#,
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
        run_serializable!(self, tx, {
            insert_lease_txn(&mut tx, ws, op_id, commit_id, object_ids, expires_at).await
        })
    }

    /// Make a lease non-expiring on migrate SUCCESS, so the migrated version stays rooted until the later
    /// pointer-move consumes it (a finite TTL would let GC reclaim a good, just-migrated version). A guarded
    /// CAS on the expected `commit_id` AND lease liveness (still non-expired, or already committed): a
    /// **stale** `migrate_finish` whose lease expired or was replaced under a reused op id updates no row
    /// and gets [`AuthorityError::Internal`], so it can never falsely claim a success whose objects GC may
    /// already have reclaimed — nor mark a *different* reused lease non-expiring.
    pub(crate) async fn commit_lease(
        &self,
        ws: &WorkspaceId,
        op_id: &OpId,
        commit_id: CommitId,
        now: i64,
    ) -> Result<()> {
        run_serializable!(
            self,
            tx,
            commit_lease_txn(&mut tx, ws, op_id, commit_id, now).await
        )
    }

    /// Release a lease (and, by cascade, its object rows). Used by tests + the abandoned-migrate path; the
    /// later pointer-move releases it after handing the root to `current`.
    pub(crate) async fn release_lease(&self, ws: &WorkspaceId, op_id: &OpId) -> Result<()> {
        run_serializable!(self, tx, release_lease_txn(&mut tx, ws, op_id).await)
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
        run_serializable!(self, tx, {
            insert_quarantine_txn(&mut tx, ws, op_id, objdir, expires_at).await
        })
    }

    /// Drop a quarantine row (after a successful migrate, or after the janitor swept its dir).
    pub(crate) async fn delete_quarantine(&self, ws: &WorkspaceId, op_id: &OpId) -> Result<()> {
        run_serializable!(self, tx, delete_quarantine_txn(&mut tx, ws, op_id).await)
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
            r#"SELECT op_id AS "op_id!" FROM upload_quarantine WHERE workspace_id = $1 AND expires_at <= $2"#,
            ws_s,
            now,
        )
        .fetch_all(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        rows.into_iter().map(|r| reparse_op(&r.op_id)).collect()
    }

    /// Atomically claim an EXPIRED quarantine slot for the janitor's sweep: delete the row **iff it is still
    /// expired** (`expires_at <= now`), returning whether this call won the claim. A concurrent re-ingest that
    /// reused the op id and refreshed `expires_at` into the future before this CAS is NOT claimed, so the
    /// janitor never sweeps a *visibly* refreshed quarantine (the [`Self::expired_quarantine_ops`] candidate
    /// list is point-in-time and advisory; this CAS is the guard, mirroring the GC claim). A later rm failure
    /// leaves only an orphan dir — the same low-severity, disk-only residual a lost-WAL row leaves (see
    /// `lifecycle::ingest`), never a wrongly-swept *refreshed* quarantine.
    ///
    /// Residual (narrow, liveness-only): the claim frees the `(workspace_id, op_id)` PK before the janitor's
    /// `rm -rf`, so a retry reusing that op id can re-insert and begin staging into the same id-derived path
    /// inside the claim→rm window; the rm then removes the re-staged dir and its migrate fails (and retries).
    /// This needs op-id REUSE (the norm is a fresh op id per attempt) AND a multi-threaded caller, and loses
    /// no committed bytes. The full close (a per-ingest generation so reuse can never alias a being-swept dir)
    /// lands with the wired pointer-move / large-object janitor — the same wired-future bucket as the live-GC
    /// claim→unlink window (see the `gc` module docs).
    pub(crate) async fn claim_expired_quarantine(
        &self,
        ws: &WorkspaceId,
        op_id: &OpId,
        now: i64,
    ) -> Result<bool> {
        run_serializable!(
            self,
            tx,
            claim_expired_quarantine_txn(&mut tx, ws, op_id, now).await
        )
    }

    /// Distinct workspaces holding an expired quarantine — the only cross-workspace read the janitor runs.
    pub(crate) async fn workspaces_with_expired_quarantine(
        &self,
        now: i64,
    ) -> Result<Vec<WorkspaceId>> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT workspace_id AS "workspace_id!" FROM upload_quarantine WHERE expires_at <= $1"#,
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
        run_serializable!(
            self,
            tx,
            insert_tombstone_txn(&mut tx, ws, blob_id, reason, now).await
        )
    }

    /// Whether a blob is denylisted (the ingest early check).
    pub(crate) async fn is_tombstoned(&self, ws: &WorkspaceId, blob_id: ObjectId) -> Result<bool> {
        let ws_s = ws.as_str();
        let bid = blob_id.0.as_slice();
        let row = sqlx::query!(
            "SELECT blob_id FROM tombstones WHERE workspace_id = $1 AND blob_id = $2",
            ws_s,
            bid,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(AuthorityError::internal)?;
        Ok(row.is_some())
    }

    /// Read an object's status while holding the write transaction (used inside [`Self::install_object`] to
    /// classify a suppressed upsert with no time-of-check/time-of-use gap). Uses `query!` like every other
    /// statement here, so it stays in the committed `.sqlx` compile-time drift gate.
    async fn locked_status(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ws: &WorkspaceId,
        object_id: ObjectId,
    ) -> Result<ObjectStatus> {
        let ws_s = ws.as_str();
        let oid = object_id.0.as_slice();
        let row = sqlx::query!(
            r#"SELECT status AS "status!" FROM object_presence WHERE workspace_id = $1 AND object_id = $2"#,
            ws_s,
            oid,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(AuthorityError::internal)?;
        match row {
            None => Ok(ObjectStatus::Absent),
            Some(r) => parse_status(&r.status),
        }
    }
}

/// The body of [`Db::install_object`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction). `db` is threaded in to
/// reach `Db::locked_status`, which classifies a suppressed upsert under the same held write lock.
#[allow(clippy::too_many_arguments)]
async fn install_object_txn(
    tx: &mut Transaction<'_, Postgres>,
    db: &Db,
    ws: &WorkspaceId,
    object_id: ObjectId,
    location: Location,
    git_oid: &[u8; GIT_OID_LEN],
    size: i64,
    now: i64,
) -> Result<InstallOutcome> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    let goid = git_oid.as_slice();
    let loc = location.as_str();

    // A denylisted blob is never (re-)introduced — the bytes the caller wrote stay an unreferenced
    // orphan (harmless). This is the best-effort early guard; the serializing check lands with the
    // pointer-move write.
    let tomb = sqlx::query!(
        "SELECT blob_id FROM tombstones WHERE workspace_id = $1 AND blob_id = $2",
        ws_s,
        oid,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if tomb.is_some() {
        return Ok(InstallOutcome::Unavailable);
    }

    // The guarded CAS: insert a fresh present row, OR bring an `absent` row back to present. A row in
    // present/deleting/unavailable is left untouched (the DO UPDATE WHERE fails → RETURNING is empty).
    let installed = sqlx::query!(
        r#"
            INSERT INTO object_presence (workspace_id, object_id, status, location, size, git_oid, status_updated_at)
            VALUES ($1, $2, 'present', $6, $3, $4, $5)
            ON CONFLICT (workspace_id, object_id) DO UPDATE SET
                status            = 'present',
                location          = excluded.location,
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
        loc,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;

    let outcome = if installed.is_some() {
        InstallOutcome::Installed
    } else {
        // The upsert was suppressed — read the blocking state IN the same transaction (the write lock
        // is held, so no time-of-check/time-of-use gap) and classify it. RETURNING alone cannot tell
        // present/deleting/unavailable apart (all yield an empty result).
        match db.locked_status(tx, ws, object_id).await? {
            ObjectStatus::Present => InstallOutcome::AlreadyPresent,
            ObjectStatus::Deleting => InstallOutcome::Deleting,
            ObjectStatus::Unavailable => InstallOutcome::Unavailable,
            // The upsert would have inserted/updated an absent/no-row case, so a suppressed-yet-absent
            // result is a store fault.
            ObjectStatus::Absent => {
                return Err(AuthorityError::integrity(SuppressedButAbsent));
            }
        }
    };
    Ok(outcome)
}

/// The body of [`Db::claim_for_delete`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn claim_for_delete_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    object_id: ObjectId,
    now: i64,
) -> Result<ClaimOutcome> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    let row = sqlx::query!(
        r#"
            UPDATE object_presence SET status = 'deleting', status_updated_at = $3
            WHERE workspace_id = $1 AND object_id = $2 AND status = 'present'
              AND NOT EXISTS (
                  SELECT 1 FROM commit_object WHERE workspace_id = $1 AND object_id = $2)
              AND NOT EXISTS (
                  SELECT 1 FROM promotion_lease_object plo
                  JOIN promotion_lease pl
                    ON pl.workspace_id = plo.workspace_id AND pl.op_id = plo.op_id
                  WHERE plo.workspace_id = $1 AND plo.object_id = $2
                    AND (pl.expires_at IS NULL OR pl.expires_at > $3))
              AND NOT EXISTS (
                  SELECT 1 FROM proposal_object po
                  JOIN proposals p ON p.workspace_id = po.workspace_id AND p.id = po.proposal_id
                  JOIN current   c ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                  WHERE po.workspace_id = $1 AND po.object_id = $2
                    AND p.status = 'open' AND c.epoch = p.base_epoch AND c.seq = p.base_seq)
            RETURNING git_oid AS "git_oid: Vec<u8>", location AS "location!"
            "#,
        ws_s,
        oid,
        now,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let outcome = match row {
        None => ClaimOutcome::Spared,
        // `location` selects the store the unlink targets; `git_oid` is the git locator (used for a
        // `git` object). The keep-set re-check above is storage-independent, so the fence is unchanged.
        Some(r) => ClaimOutcome::Claimed {
            location: parse_location(&r.location)?,
            git_oid: git_oid_from_row(r.git_oid)?,
        },
    };
    Ok(outcome)
}

/// The body of [`Db::finalize_delete`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn finalize_delete_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    object_id: ObjectId,
    claim_token: i64,
    now: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    sqlx::query!(
        "UPDATE object_presence SET status = 'absent', status_updated_at = $4 \
         WHERE workspace_id = $1 AND object_id = $2 AND status = 'deleting' \
           AND status_updated_at = $3",
        ws_s,
        oid,
        claim_token,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// The body of [`Db::claim_stale_for_recovery`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn claim_stale_for_recovery_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    object_id: ObjectId,
    older_than: i64,
    now: i64,
) -> Result<Option<(Location, [u8; GIT_OID_LEN])>> {
    let ws_s = ws.as_str();
    let oid = object_id.0.as_slice();
    let row = sqlx::query!(
        r#"UPDATE object_presence SET status_updated_at = $4
               WHERE workspace_id = $1 AND object_id = $2 AND status = 'deleting' AND status_updated_at < $3
                 AND NOT EXISTS (
                     SELECT 1 FROM commit_object WHERE workspace_id = $1 AND object_id = $2)
                 AND NOT EXISTS (
                     SELECT 1 FROM proposal_object po
                     JOIN proposals p ON p.workspace_id = po.workspace_id AND p.id = po.proposal_id
                     JOIN current   c ON c.workspace_id = p.workspace_id  AND c.skill_id = p.skill_id
                     WHERE po.workspace_id = $1 AND po.object_id = $2
                       AND p.status = 'open' AND c.epoch = p.base_epoch AND c.seq = p.base_seq)
               RETURNING git_oid AS "git_oid: Vec<u8>", location AS "location!""#,
        ws_s,
        oid,
        older_than,
        now,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    let claimed = match row {
        None => None,
        Some(r) => Some((parse_location(&r.location)?, git_oid_from_row(r.git_oid)?)),
    };
    Ok(claimed)
}

/// The body of [`Db::insert_lease`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn insert_lease_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &OpId,
    commit_id: CommitId,
    object_ids: &[ObjectId],
    expires_at: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let op_s = op_id.as_str();
    let cid = commit_id.0.as_slice();
    // A COMMITTED (non-expiring) lease is the durable root of an already-succeeded migrate; never
    // rewrite it or clear its child set (that would unroot the version and let GC reclaim its blobs).
    // op-id reuse against a committed lease is therefore an idempotent no-op.
    let committed = sqlx::query!(
        r#"SELECT op_id AS "op_id!" FROM promotion_lease
               WHERE workspace_id = $1 AND op_id = $2 AND expires_at IS NULL"#,
        ws_s,
        op_s,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if committed.is_some() {
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO promotion_lease (workspace_id, op_id, commit_id, expires_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id, op_id) DO UPDATE SET commit_id = excluded.commit_id, expires_at = excluded.expires_at",
        ws_s,
        op_s,
        cid,
        expires_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // Clear any prior in-flight child set for this op (op-id reuse) before re-inserting, so the lease
    // names exactly this candidate's objects.
    sqlx::query!(
        "DELETE FROM promotion_lease_object WHERE workspace_id = $1 AND op_id = $2",
        ws_s,
        op_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // The full child set in ONE set-valued statement (an UNNEST array bind) — a per-object statement per
    // blob would stretch the SERIALIZABLE conflict window linearly with bundle size on every publish/propose.
    let oids: Vec<Vec<u8>> = object_ids.iter().map(|o| o.0.to_vec()).collect();
    sqlx::query!(
        "INSERT INTO promotion_lease_object (workspace_id, op_id, object_id) \
         SELECT $1, $2, UNNEST($3::bytea[]) \
         ON CONFLICT (workspace_id, op_id, object_id) DO NOTHING",
        ws_s,
        op_s,
        &oids,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    // GC-lease MVCC fence (Postgres-specific): also WRITE the `object_presence` row of each currently
    // `present` leased object, so a concurrent GC `claim_for_delete` — which UPDATEs the same row
    // `present → deleting` — has a genuine write-write conflict with this lease transaction. Under
    // SQLite the global writer lock made a committed lease visible to any later claim; under Postgres
    // SSI a lease that wrote only `promotion_lease*` would be a lone rw-antidependency SSI does NOT
    // abort, so a GC whose snapshot predates the lease commit could reclaim a freshly-leased
    // dedup-`present` object (the migrate present-path reuses it WITHOUT re-touching the row). The
    // self-assignment still writes a new row version (Postgres never elides a no-op UPDATE) and takes
    // the row locks, so the loser aborts 40001 → the runner retries → the claim's keep-set re-check now
    // sees the live lease and spares the object. It changes no meaning: `status` stays `present`, and
    // `status_updated_at` only gates recovery-staleness of `deleting` rows. A to-be-installed (`absent`)
    // object matches nothing here and is instead protected by `install_object`'s own `absent → present`
    // write. One set-valued UPDATE (`ANY` array bind), same rows touched as the old per-object loop.
    sqlx::query!(
        "UPDATE object_presence SET status_updated_at = status_updated_at \
         WHERE workspace_id = $1 AND status = 'present' AND object_id = ANY($2)",
        ws_s,
        &oids,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// The body of [`Db::commit_lease`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn commit_lease_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &OpId,
    commit_id: CommitId,
    now: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let op_s = op_id.as_str();
    let cid = commit_id.0.as_slice();
    let row = sqlx::query!(
        r#"UPDATE promotion_lease SET expires_at = NULL
               WHERE workspace_id = $1 AND op_id = $2 AND commit_id = $3
                 AND (expires_at IS NULL OR expires_at > $4)
               RETURNING op_id AS "op_id!""#,
        ws_s,
        op_s,
        cid,
        now,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    if row.is_none() {
        return Err(AuthorityError::internal(LeaseNotLive));
    }
    Ok(())
}

/// The body of [`Db::release_lease`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn release_lease_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &OpId,
) -> Result<()> {
    let ws_s = ws.as_str();
    let op_s = op_id.as_str();
    sqlx::query!(
        "DELETE FROM promotion_lease WHERE workspace_id = $1 AND op_id = $2",
        ws_s,
        op_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// The body of [`Db::insert_quarantine`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn insert_quarantine_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &OpId,
    objdir: &str,
    expires_at: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let op_s = op_id.as_str();
    sqlx::query!(
        "INSERT INTO upload_quarantine (workspace_id, op_id, objdir, expires_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id, op_id) DO UPDATE SET objdir = excluded.objdir, expires_at = excluded.expires_at",
        ws_s,
        op_s,
        objdir,
        expires_at,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// The body of [`Db::delete_quarantine`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn delete_quarantine_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &OpId,
) -> Result<()> {
    let ws_s = ws.as_str();
    let op_s = op_id.as_str();
    sqlx::query!(
        "DELETE FROM upload_quarantine WHERE workspace_id = $1 AND op_id = $2",
        ws_s,
        op_s,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
}

/// The body of [`Db::claim_expired_quarantine`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn claim_expired_quarantine_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    op_id: &OpId,
    now: i64,
) -> Result<bool> {
    let ws_s = ws.as_str();
    let op_s = op_id.as_str();
    let row = sqlx::query!(
        r#"DELETE FROM upload_quarantine
               WHERE workspace_id = $1 AND op_id = $2 AND expires_at <= $3
               RETURNING op_id AS "op_id!""#,
        ws_s,
        op_s,
        now,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(row.is_some())
}

/// The body of [`Db::insert_tombstone`], extracted so the `run_serializable!` macro can re-run it on a
/// serialization retry (it borrows its inputs and touches only the transaction).
async fn insert_tombstone_txn(
    tx: &mut Transaction<'_, Postgres>,
    ws: &WorkspaceId,
    blob_id: ObjectId,
    reason: &str,
    now: i64,
) -> Result<()> {
    let ws_s = ws.as_str();
    let bid = blob_id.0.as_slice();
    sqlx::query!(
        "INSERT INTO tombstones (workspace_id, blob_id, reason, at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id, blob_id) DO NOTHING",
        ws_s,
        bid,
        reason,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    sqlx::query!(
        "UPDATE object_presence SET status = 'unavailable', status_updated_at = $3 \
         WHERE workspace_id = $1 AND object_id = $2 AND status IN ('present', 'absent')",
        ws_s,
        bid,
        now,
    )
    .execute(&mut **tx)
    .await
    .map_err(AuthorityError::internal)?;
    Ok(())
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

/// Parse a stored location string. `large-remote` is schema-reserved for the deferred S3-compatible backend
/// and v0 writes none, so meeting it — or any unknown value — is store corruption (the read/unlink dispatch
/// has no arm for it). The CHECK constraint already forbids anything outside the known set.
fn parse_location(s: &str) -> Result<Location> {
    match s {
        "git" => Ok(Location::Git),
        "large-local" => Ok(Location::LargeLocal),
        _ => Err(AuthorityError::integrity(BadLocation)),
    }
}

/// Convert a stored 32-byte object-id BLOB into an [`ObjectId`], or an integrity fault on a bad width.
fn object_id_from_row(bytes: Vec<u8>) -> Result<ObjectId> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuthorityError::integrity(BadBlobWidth))?;
    Ok(ObjectId(arr))
}

/// Convert a stored git-oid BLOB into a 20-byte array. A NULL or wrong-width value on a row the fence is
/// acting on is store corruption: every fenced object (git **and** large-local) records its 20-byte git oid
/// — the loose-object locator for a `git` object, and the tree-entry bridge key for a `large-local` one.
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
#[error("stored object location is not a known value")]
struct BadLocation;

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

#[derive(Debug, thiserror::Error)]
#[error(
    "the promotion lease is no longer live (expired or replaced) — the migrate must not claim success"
)]
struct LeaseNotLive;
