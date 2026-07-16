//! The sealed authority facade — the crate's one public type.

use std::path::{Path, PathBuf};
use std::time::Duration;

use topos_gitstore::{LocalLargeStore, Store};

use crate::commit::{BundleDeleteReport, CommittedVersion, PointerState, PurgeReport};
use crate::db::Db;
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, ObjectId, OpId, WorkspaceId};
use crate::read::{CurrentInfo, LogEntry, VersionMeta, WorkspaceStorage};
use crate::upload::CandidateUpload;

/// The default size at/above which a file blob is offloaded to the large-object store (≈ 1 MiB). Git
/// packs/dedups small text-shaped blobs well but degrades on large binaries; below this they stay in git.
pub(crate) const DEFAULT_LARGE_THRESHOLD: u64 = 1 << 20;

/// The default per-blob hard reject cap (≈ 100 MiB): a blob larger than this is refused at ingest before
/// any bytes are staged.
pub(crate) const DEFAULT_LARGE_REJECT_CAP: u64 = 100 << 20;

/// The default cap on a log read (the composing server may ask for fewer, never more per request).
pub const DEFAULT_LOG_LIMIT: usize = 50;

/// Connection-pool tuning for the Postgres backend — plain owned data (no `sqlx` type crosses it), so a
/// composing server sets it without naming the driver. `None` on a field keeps the default: sqlx's
/// `max_connections = 10` / `acquire_timeout = 30s`, and the server's own statement/lock/idle values.
/// Each `Some` timeout is applied as a connection-level `SET` on every pooled connection.
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    /// Max pooled connections (sqlx's default 10 when `None`). A write holds one connection for the
    /// whole `run_serializable!` retry loop, so raise it for a busy plane.
    pub max_connections: Option<u32>,
    /// How long `acquire` waits for a free pooled connection before failing (sqlx's default 30s when `None`).
    pub acquire_timeout: Option<Duration>,
    /// Per-statement server ceiling (`statement_timeout`). `None` ⇒ unset (the server default). Opt in for a
    /// hard runaway-query ceiling, but keep it above the slowest legitimate whole-bundle render.
    pub statement_timeout: Option<Duration>,
    /// Lock-wait ceiling (`lock_timeout`). `None` ⇒ unset (the server default).
    pub lock_timeout: Option<Duration>,
    /// How long a transaction may sit idle — open but running no statement — before the server aborts it
    /// (`idle_in_transaction_session_timeout`), bounding an abandoned/stuck txn that would otherwise pin row
    /// locks. `None` ⇒ unset; a modest value is safe here because every write txn is pure-DB and short.
    pub idle_in_transaction_timeout: Option<Duration>,
}

/// The vault's byte-custody authority — the **only** public type in this crate.
///
/// PURE BYTE CUSTODY: versions (content-addressed), the generation-fenced `current` pointer, the
/// object stores, and the GC fence. The vault has ONE caller (the composing server fronting the
/// product app) and treats every request as PRE-AUTHORIZED — no identity, no membership, no policy
/// lives here. Requests carry opaque `(workspace_id, bundle_id, …)` strings plus attribution
/// display strings stored verbatim; the vault validates SHAPE (charset/length), never meaning.
///
/// Every raw SQL statement and raw git-object read is private; it owns one Postgres schema (every
/// row bound on `workspace_id`) and a confined root under which each workspace gets its own git
/// object store + large-object store. Cross-workspace isolation is that database binding — never
/// the directory.
#[derive(Debug)]
pub struct Authority {
    db: Db,
    git_root: PathBuf,
    /// The confined root under which each workspace gets its **own** large-object store (a sibling of
    /// `git_root`); big blobs are offloaded here at migrate. Per-workspace subdirs are the hard tenant
    /// boundary (no cross-workspace dedup), exactly like `git_root`.
    large_root: PathBuf,
    /// Size at/above which a file blob is offloaded to the large-object store (operational config; never
    /// enters any id/digest).
    large_threshold: u64,
    /// Per-blob hard reject cap, enforced at ingest.
    large_reject_cap: u64,
}

impl Authority {
    /// Open the authority over a Postgres `database_url`, a git-store root, and a large-object-store root
    /// (the roots created if absent; the schema migrated on the database).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created or the database cannot be opened or
    /// migrated.
    pub async fn open(database_url: &str, git_root: &Path, large_root: &Path) -> Result<Self> {
        Self::open_with_pool(database_url, git_root, large_root, PoolConfig::default()).await
    }

    /// Open the authority exactly like [`open`](Self::open) but with explicit connection-pool tuning.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created or the database cannot be opened or
    /// migrated.
    pub async fn open_with_pool(
        database_url: &str,
        git_root: &Path,
        large_root: &Path,
        pool: PoolConfig,
    ) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        std::fs::create_dir_all(large_root).map_err(AuthorityError::internal)?;
        let db = Db::connect(database_url, &pool).await?;
        Ok(Self {
            db,
            git_root: git_root.to_path_buf(),
            large_root: large_root.to_path_buf(),
            large_threshold: DEFAULT_LARGE_THRESHOLD,
            large_reject_cap: DEFAULT_LARGE_REJECT_CAP,
        })
    }

    /// Build the authority over an **already-open** `PgPool` (the schema assumed already migrated) plus the
    /// two store roots. The injection seam for `#[sqlx::test]` — which provisions a fresh per-test database,
    /// runs the migrations, and hands over the pool — and for an out-of-crate e2e harness that provisions its
    /// own per-test database the same way. Test / `test-fixtures` only: it is the sole place a `sqlx` type
    /// (`PgPool`) crosses this boundary, and it is compiled out of the production build.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn from_pool(pool: sqlx::PgPool, git_root: &Path, large_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        std::fs::create_dir_all(large_root).map_err(AuthorityError::internal)?;
        Ok(Self {
            db: Db::from_pool(pool),
            git_root: git_root.to_path_buf(),
            large_root: large_root.to_path_buf(),
            large_threshold: DEFAULT_LARGE_THRESHOLD,
            large_reject_cap: DEFAULT_LARGE_REJECT_CAP,
        })
    }

    /// Override the size-routing threshold + per-blob reject cap (operational config — neither ever enters
    /// a manifest, digest, or id). A consuming server wires these from its config; tests use a tiny
    /// threshold to force a placement.
    #[must_use]
    pub fn with_large_limits(mut self, threshold: u64, reject_cap: u64) -> Self {
        self.large_threshold = threshold;
        self.large_reject_cap = reject_cap;
        self
    }

    /// The size at/above which a file blob is offloaded to the large-object store.
    pub(crate) fn large_threshold(&self) -> u64 {
        self.large_threshold
    }

    /// The per-blob hard reject cap enforced at ingest.
    pub(crate) fn large_reject_cap(&self) -> u64 {
        self.large_reject_cap
    }

    // ── the write surface ─────────────────────────────────────────────────────────────────────────

    /// Ingest + commit a candidate WITHOUT moving the pointer (the propose path). The candidate's
    /// bytes are server-rehashed (no client id trusted), deduped invisibly, installed under a
    /// promotion lease, and recorded as a version. Committing an identical candidate twice returns
    /// the same ids (success, `deduped`).
    ///
    /// # Errors
    /// [`AuthorityError::RejectedUpload`] on a canonical-rule/parent/size/denylist refusal;
    /// [`AuthorityError::TargetPurged`] if the candidate re-derives a purged version's id;
    /// [`AuthorityError::InvalidId`] on a malformed attribution;
    /// [`AuthorityError::Integrity`]/[`AuthorityError::Internal`] on store/database faults.
    pub async fn commit_version(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        candidate: CandidateUpload,
        now: i64,
    ) -> Result<CommittedVersion> {
        crate::commit::commit_version(self, ws, bundle, candidate, now).await
    }

    /// Ingest + commit + CAS pointer move, one flow (the direct publish path). `expected_generation`
    /// `None` = genesis (creates the pointer at generation 1); `Some(g)` = the CAS, under the
    /// same-bundle lineage fence (the candidate's first parent must be the currently pointed
    /// version). The idempotent-CAS rule makes an app-side retry after a crash safe (see
    /// [`PointerState::replayed`]).
    ///
    /// # Errors
    /// [`AuthorityError::Conflict`] (carrying the live pointer) on a lost CAS — the refused write
    /// leaves no version row behind; otherwise as [`commit_version`](Self::commit_version).
    pub async fn publish(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        candidate: CandidateUpload,
        expected_generation: Option<u64>,
        now: i64,
    ) -> Result<(CommittedVersion, PointerState)> {
        crate::commit::publish(self, ws, bundle, candidate, expected_generation, now).await
    }

    /// CAS the pointer to an EXISTING version (the approve path). No bytes move.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on an unknown target; [`AuthorityError::TargetPurged`] on a
    /// purged one; [`AuthorityError::Conflict`] on a lost CAS; [`AuthorityError::InvalidId`] on a
    /// malformed attribution; [`AuthorityError::Internal`] on a database fault.
    pub async fn move_pointer(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
        expected_generation: Option<u64>,
        attribution: &str,
        now: i64,
    ) -> Result<PointerState> {
        crate::commit::move_pointer(
            self,
            ws,
            bundle,
            version,
            expected_generation,
            attribution,
            now,
        )
        .await
    }

    /// The revert: a FORWARD commit `{tree: target.tree, parents: [current]}` + the CAS (the pointer
    /// never moves backward). `message` is the caller's forward-commit message, recorded verbatim —
    /// the commit frame's inputs are the wire's, so a client that pre-derives the forward id can
    /// verify the move landed on exactly that version.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on an unknown target; [`AuthorityError::TargetPurged`] on a
    /// purged one (typed, before any staging); [`AuthorityError::Conflict`] on a lost CAS;
    /// [`AuthorityError::InvalidId`] on a malformed attribution; the rest as
    /// [`publish`](Self::publish).
    #[allow(clippy::too_many_arguments)]
    pub async fn revert(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        to_version: CommitId,
        expected_generation: u64,
        attribution: &str,
        message: &str,
        now: i64,
    ) -> Result<(CommittedVersion, PointerState)> {
        crate::commit::revert(
            self,
            ws,
            bundle,
            to_version,
            expected_generation,
            attribution,
            message,
            now,
        )
        .await
    }

    /// The byte purge: refuse if pointed-at; tombstone the blobs unique to the version; stamp
    /// `purged_at`; reclaim the bytes inline. The version row (the hash) stays. Idempotent: purging
    /// an already-purged version is an empty success.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on an unknown version; [`AuthorityError::PointedAt`] when
    /// `current` names it; [`AuthorityError::InvalidId`] on a malformed attribution;
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on faults.
    pub async fn purge_version(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
        attribution: &str,
        now: i64,
    ) -> Result<PurgeReport> {
        crate::commit::purge_version(self, ws, bundle, version, attribution, now).await
    }

    /// Bundle GC on app instruction (the app already decided the deletion): reclaim every version +
    /// unreferenced bytes, drop the rows. Idempotent (deleting an unknown bundle drops zero rows).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on faults.
    pub async fn delete_bundle(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        now: i64,
    ) -> Result<BundleDeleteReport> {
        crate::commit::delete_bundle(self, ws, bundle, now).await
    }

    /// Workspace reclaim: drop every custody row of the workspace and remove its physical stores.
    /// Idempotent.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a fault.
    pub async fn delete_workspace(&self, ws: &WorkspaceId) -> Result<()> {
        crate::commit::delete_workspace(self, ws).await
    }

    // ── the read surface ──────────────────────────────────────────────────────────────────────────

    /// Read a bundle's `current` pointer: the pointed version, the CAS generation, the move
    /// attribution + time, and the pointed version's consent digest. `None` until a pointer exists.
    ///
    /// # Errors
    /// [`AuthorityError::Integrity`] if the pointed version has no digest row (corruption, never a
    /// not-found); [`AuthorityError::Internal`] on a database fault.
    pub async fn read_current(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
    ) -> Result<Option<CurrentInfo>> {
        crate::read::read_current(self, ws, bundle).await
    }

    /// Read one object's bytes through the bundle-scoped reachability rule — served only when some
    /// live (non-purged) version of `bundle` reaches it, re-verified against the id that named it.
    /// **No object is ever served by bare hash.**
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] when unreachable/nonexistent (or legitimately reclaimed mid-read);
    /// [`AuthorityError::Integrity`] on verify-on-read corruption; [`AuthorityError::Internal`] on a
    /// database fault.
    pub async fn read_object(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        object_id: ObjectId,
    ) -> Result<Vec<u8>> {
        crate::read::read_object(self, ws, bundle, object_id).await
    }

    /// Read a version's metadata + file listing (no blob bytes): id, parents, attribution, message,
    /// the consent digest, and the per-file `(path, mode, object_id)` leaves.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] on an unknown or purged version; [`AuthorityError::Integrity`] on
    /// a bookkeeping/store divergence; [`AuthorityError::Internal`] on a database fault.
    pub async fn read_version(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        version: CommitId,
    ) -> Result<VersionMeta> {
        crate::read::read_version(self, ws, bundle, version).await
    }

    /// The first-parent commit chain from `current`, capped at `limit` — version ids + messages +
    /// attributions + timestamps (a purged version stays listed with its purge stamp).
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] when the bundle has no pointer; [`AuthorityError::Integrity`] on
    /// a broken chain; [`AuthorityError::Internal`] on a database fault.
    pub async fn log(
        &self,
        ws: &WorkspaceId,
        bundle: &BundleId,
        limit: usize,
    ) -> Result<Vec<LogEntry>> {
        crate::read::log(self, ws, bundle, limit).await
    }

    /// Every workspace's stored byte total, ordered by workspace id — the operational accounting
    /// read (opaque workspace ids in, numbers out; the caller joins them to whatever they mean).
    /// Counts `present` objects ONLY: `deleting`/`absent`/`unavailable` bytes are not custody the
    /// product should bill or display. A workspace holding no present object is absent.
    ///
    /// # Errors
    /// [`AuthorityError::Integrity`] on a malformed stored row; [`AuthorityError::Internal`] on a
    /// database fault.
    pub async fn storage_stats(&self) -> Result<Vec<WorkspaceStorage>> {
        crate::read::storage_stats(self).await
    }

    // ── maintenance (the composing server schedules these) ───────────────────────────────────────

    /// Every workspace currently holding objects — the composing server's GC-scheduling enumeration
    /// (ids only; recovery + janitor enumerate cross-workspace internally).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn workspaces(&self) -> Result<Vec<WorkspaceId>> {
        self.db.workspaces_with_objects().await
    }

    /// Run one GC pass over a workspace (acquire → unlink → finalize per unrooted object). Returns the
    /// number of objects reclaimed.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on faults.
    pub async fn run_gc(&self, ws: &WorkspaceId, now: i64) -> Result<usize> {
        crate::gc::run_gc(self, ws, now).await
    }

    /// Run the recovery sweep (finalize stale `deleting` rows a crashed GC left) across all
    /// workspaces. Returns the number recovered.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on faults.
    pub async fn run_recovery(&self, now: i64) -> Result<usize> {
        crate::gc::recovery_sweep(self, now).await
    }

    /// Run the quarantine janitor (sweep expired/abandoned staging dirs) across all workspaces.
    /// Returns the number swept.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on faults.
    pub async fn run_janitor(&self, now: i64) -> Result<usize> {
        crate::gc::quarantine_janitor(self, now).await
    }

    // ── crate-internal plumbing ───────────────────────────────────────────────────────────────────

    /// The database handle — crate-private (no `sqlx` type crosses out of `mod db`).
    pub(crate) fn db(&self) -> &Db {
        &self.db
    }

    /// The per-workspace git-store directory — one component under the confined root. `WorkspaceId`
    /// is a validated path-safe id (no separators, no leading dot), so this can never escape
    /// `git_root`.
    pub(crate) fn workspace_git_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.git_root.join(ws.as_str())
    }

    /// The per-workspace quarantine ROOT: `git_root/.quarantine/<ws>`. The `.quarantine` component
    /// is reserved by the id shape rule (no id may start with a dot), so it can never collide with a
    /// workspace store dir — and it is a SIBLING of those dirs, so nothing walking a workspace store
    /// ever sees a quarantine.
    pub(crate) fn workspace_quarantine_root(&self, ws: &WorkspaceId) -> PathBuf {
        self.git_root.join(".quarantine").join(ws.as_str())
    }

    /// The per-op upload-quarantine directory: `git_root/.quarantine/<ws>/<op_id>`. Both ids are
    /// validated path-safe newtypes, so the path can never escape `git_root`.
    pub(crate) fn workspace_quarantine_dir(&self, ws: &WorkspaceId, op_id: &OpId) -> PathBuf {
        self.workspace_quarantine_root(ws).join(op_id.as_str())
    }

    /// Open the per-workspace git store for reading. A failure here is reached only after the database
    /// said the bytes exist, so a missing/un-openable store is a bookkeeping/store divergence (corruption).
    pub(crate) fn open_store(&self, ws: &WorkspaceId) -> Result<Store> {
        Store::open(&self.workspace_git_dir(ws)).map_err(AuthorityError::integrity)
    }

    /// The per-workspace large-object store directory (`large_root/<ws>`).
    pub(crate) fn workspace_large_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.large_root.join(ws.as_str())
    }

    /// The per-workspace large-object store handle, rooted at `large_root/<ws>/`. `WorkspaceId` is a
    /// validated, path-safe id, so the root can never escape `large_root` and one workspace's handle can
    /// never name another's bytes — cross-workspace isolation is the path itself, and byte-identical
    /// content in two workspaces is two distinct physical objects (no cross-workspace dedup).
    /// Construction stays inside this crate, so nothing outside the authority can fetch a large object by
    /// bare hash. Infallible: the store creates its directories lazily on the first `put`.
    pub(crate) fn large_store(&self, ws: &WorkspaceId) -> LocalLargeStore {
        LocalLargeStore::new(self.workspace_large_dir(ws))
    }
}

/// Open-or-create a bare per-workspace git store at `dir` — the write-path open. A free fn so a
/// blocking-pool closure can call it with an owned dir.
///
/// Creation is serialized under an in-process lock: two concurrent first-time writers can both observe
/// the directory as absent, and bare-repo `init` is neither an idempotent open-or-create nor atomic (a
/// racer can open a repo mid-init and fail) — write sections genuinely run in parallel on the
/// blocking pool, so a bare open→init would race. Under the lock the loser re-opens what the winner
/// completed; the fast path (the store already exists) takes no lock at all. The lock covers ONE
/// process; the `or_else(open)` below covers the rest.
pub(crate) fn open_or_init_store(dir: &Path) -> Result<Store> {
    if let Ok(store) = Store::open(dir) {
        return Ok(store);
    }
    static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _creation = INIT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match Store::open(dir) {
        Ok(store) => Ok(store),
        // The CROSS-PROCESS creation race: two plane processes sharing one git volume (a rolling
        // deploy's overlap, a second replica on a shared mount) can both attempt first-time creation,
        // and the in-process mutex is no help there. If our `init` lost to another process's completed
        // `init`, fall back to opening what that process created rather than failing the write.
        Err(_) => Store::init(dir)
            .or_else(|_| Store::open(dir))
            .map_err(AuthorityError::internal),
    }
}

/// Run one synchronous store section on tokio's **blocking pool**, so fsync-heavy git/large-object I/O
/// (bundle staging, durable installs and commits, verify-on-read byte fetches up to the reject cap) never
/// pins an async worker thread. The closure takes **owned** inputs and opens the non-`Send` gix `Store`
/// inside itself (it can never cross the boundary); a pool-join fault maps to
/// [`AuthorityError::Internal`].
pub(crate) async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(AuthorityError::internal)?
}
