//! The sealed authority facade — the crate's one public type.

use std::path::{Path, PathBuf};

use topos_gitstore::{LocalLargeStore, Store};

use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, OpId, Principal, SkillId, WorkspaceId};
use crate::lineage::{CandidateCommit, LineageDecision};
use crate::set_current::{DeviceSignedOp, SetCurrentReceipt};
use crate::signer::PlaneSigner;
use crate::sqlite::Db;
use crate::upload::{CandidateUpload, UploadReceipt};

/// The default size at/above which a file blob is offloaded to the large-object store (≈ 1 MiB). Git
/// packs/dedups small text-shaped blobs well but degrades on large binaries; below this they stay in git.
pub(crate) const DEFAULT_LARGE_THRESHOLD: u64 = 1 << 20;

/// The default per-blob hard reject cap (≈ 100 MiB): a blob larger than this is refused at ingest before
/// any bytes are staged.
pub(crate) const DEFAULT_LARGE_REJECT_CAP: u64 = 100 << 20;

/// The plane's per-workspace storage authority — the **only** public type in this crate.
///
/// Every raw SQL statement and raw git-object read is private; the only operations are authorized:
/// [`read_object`](Self::read_object), [`upload_candidate`](Self::upload_candidate), and
/// [`check_lineage`](Self::check_lineage). It owns one SQLite database (the per-workspace provenance,
/// reachability, roster, and pointer rows, every one bound on `workspace_id`) and a confined root under
/// which each workspace gets its own git object store. Cross-workspace isolation is that database
/// binding — never the directory.
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
    /// The in-process plane signer — the ONLY private-key holder, loaded by
    /// [`with_plane_key`](Self::with_plane_key). Absent until configured: the pointer-move requires it (a
    /// typed precondition), while every other operation (read/upload/lineage/lifecycle) never signs.
    signer: Option<PlaneSigner>,
}

impl Authority {
    /// Open the authority over a SQLite database file, a git-store root, and a large-object-store root (all
    /// created if absent, with the schema migrated). The size-routing threshold + reject cap default to
    /// the crate's `DEFAULT_LARGE_THRESHOLD` / `DEFAULT_LARGE_REJECT_CAP`; override with
    /// [`with_large_limits`](Self::with_large_limits).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if a store root cannot be created or the database cannot be opened or
    /// migrated.
    pub async fn open_sqlite(db_path: &Path, git_root: &Path, large_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        std::fs::create_dir_all(large_root).map_err(AuthorityError::internal)?;
        let db = Db::open(db_path).await?;
        Ok(Self {
            db,
            git_root: git_root.to_path_buf(),
            large_root: large_root.to_path_buf(),
            large_threshold: DEFAULT_LARGE_THRESHOLD,
            large_reject_cap: DEFAULT_LARGE_REJECT_CAP,
            signer: None,
        })
    }

    /// Load (or, on first run, generate + persist `0600`) the plane signing key from `path`, enabling the
    /// pointer-move. The key is read once here — never per-op, never inside a transaction. Self-host needs
    /// zero config (the key is generated on first run); an operator may pre-place a 32-byte seed at `path`
    /// instead. At-rest encryption / KMS is the named next step.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the key file cannot be read/created/validated.
    pub fn with_plane_key(mut self, path: &Path) -> Result<Self> {
        self.signer = Some(PlaneSigner::load_or_generate(path)?);
        Ok(self)
    }

    /// Override the size-routing threshold + per-blob reject cap (operational config — neither ever enters
    /// a manifest, digest, or id). A consuming server wires these from its config; tests use it to force a
    /// placement (a tiny threshold routes ordinary bytes to the large store, proving identity is the same
    /// whichever store holds them).
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

    /// Read one object's bytes through the skill-scoped access rule.
    ///
    /// The bytes are returned only if `principal` is rostered for `skill` **and** some commit of that
    /// skill reaches `object_id`. Every not-entitled and not-found case — not rostered, the skill does
    /// not reach the object, or the object does not exist — returns the single [`AuthorityError::NotFound`],
    /// byte-for-byte indistinguishable, so a caller can never probe which skills or objects exist.
    ///
    /// # Errors
    /// [`AuthorityError::NotFound`] when not entitled / not reachable / nonexistent;
    /// [`AuthorityError::Integrity`] if the authority's provenance and its object store have diverged
    /// (corruption — reachable only *after* entitlement was proven, so it discloses nothing);
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn read_object(
        &self,
        principal: &Principal,
        ws: &WorkspaceId,
        skill: &SkillId,
        object_id: ObjectId,
    ) -> Result<Vec<u8>> {
        crate::read::read_object(self, principal, ws, skill, object_id).await
    }

    /// Upload a full candidate bundle: every file's bytes are re-hashed server-side (no client id is
    /// trusted, and there is no reference-by-id), the canonical rules are applied to the uploaded
    /// bytes, the objects are written to the per-workspace store, and — only after the authoritative
    /// roster check, in one transaction — the commit's provenance and reachability are recorded. This
    /// moves no pointer. The receipt is a pure function of the uploaded tree, identical whether the
    /// bytes were new or already present (dedup is invisible).
    ///
    /// # Errors
    /// [`AuthorityError::Denied`] if the principal is not rostered for the skill, or the candidate
    /// would adopt a commit owned by another skill; [`AuthorityError::RejectedUpload`] if the bytes
    /// violate the canonical rules or name a parent the workspace does not hold;
    /// [`AuthorityError::Internal`] on a store or database fault.
    pub async fn upload_candidate(
        &self,
        principal: &Principal,
        ws: &WorkspaceId,
        skill: &SkillId,
        candidate: CandidateUpload,
    ) -> Result<UploadReceipt> {
        crate::upload::upload_candidate(self, principal, ws, skill, candidate).await
    }

    /// Evaluate the cross-skill lineage predicate over a candidate set (read-only this increment; the
    /// pointer-move write enforces it transactionally later).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn check_lineage(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        candidates: &[CandidateCommit],
    ) -> Result<LineageDecision> {
        crate::lineage::check_lineage(self, ws, skill, candidates).await
    }

    /// Publish a candidate as the skill's new `current` — a direct publish, or **genesis** for the first
    /// version. The full backbone flow: a review-required preflight (uploads nothing if gated) → ingest +
    /// migrate (the crash-safe quarantine → lease → install → record) → the one pure-DB pointer-move
    /// transaction (compare-and-set, sign, re-root, durable receipt). Returns the durable, replayable
    /// receipt; a retry with the same `op_id` + bound identity returns it byte-identically.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`]/[`AuthorityError::Integrity`] on a store fault; the plane key must be
    /// configured ([`with_plane_key`](Self::with_plane_key)).
    #[allow(clippy::too_many_arguments)]
    pub async fn publish(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        op_id: &OpId,
        candidate: CandidateUpload,
        device: DeviceSignedOp,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        // A direct publish must be signed as exactly that. Forwarding an arbitrary device op (e.g. a
        // `Revert`-labelled candidate of new bytes) would skip the direct-publish review gate while still
        // reaching the promote path — a review bypass. Reject anything but `PublishDirect` BEFORE ingesting
        // (so a misuse uploads/migrates/leases nothing).
        if !matches!(device.op, topos_core::sign::DeviceOp::PublishDirect) {
            return crate::set_current::reject_non_publish_op(
                self, ws, skill, op_id, &device, created_at,
            )
            .await;
        }
        if let Some(receipt) = crate::set_current::publish_preflight(
            self,
            ws,
            skill,
            device.op,
            &device.device_key_id,
            op_id,
            None,
            None,
            device.expected,
            created_at,
        )
        .await?
        {
            return Ok(receipt);
        }
        let staged = crate::lifecycle::ingest(self, ws, op_id, candidate, now).await?;
        crate::lifecycle::migrate(self, ws, &staged, now).await?;
        crate::set_current::publish(self, ws, skill, &staged, &device, created_at, now).await
    }

    /// Revert the skill's `current` to a known-good prior version — a **forward** commit `{tree: good.tree,
    /// parents: [current]}` that advances `seq` (the pointer never moves backward). Bypasses the review gate
    /// (it restores already-consented bytes — the in-v0 safety net).
    ///
    /// # Errors
    /// As [`publish`](Self::publish); plus a git-store fault constructing the forward commit.
    #[allow(clippy::too_many_arguments)]
    pub async fn revert(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
        good: CommitId,
        device: DeviceSignedOp,
        author: &str,
        message: &str,
        op_id: &OpId,
        created_at: &str,
        now: i64,
    ) -> Result<SetCurrentReceipt> {
        crate::set_current::revert(
            self, ws, skill, good, &device, author, message, op_id, created_at, now,
        )
        .await
    }

    /// The plane's raw 32-byte Ed25519 **public** key — for a follower to pin the trust root out-of-band.
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no plane key is configured.
    pub fn plane_public_key(&self) -> Result<[u8; 32]> {
        Ok(self.plane_signer()?.public_key())
    }

    /// The plane's signing key id (the `key_id` in a signed pointer + an OK receipt).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if no plane key is configured.
    pub fn plane_key_id(&self) -> Result<String> {
        Ok(self.plane_signer()?.key_id().to_owned())
    }

    /// Read back a skill's signed `current` record — the serialized `SignedCurrentRecord` a follower's
    /// pointer fetch returns. `None` until the pointer has been moved (signed).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] on a database fault.
    pub async fn read_signed_record(
        &self,
        ws: &WorkspaceId,
        skill: &SkillId,
    ) -> Result<Option<Vec<u8>>> {
        self.db.read_signed_record(ws, skill).await
    }

    // ── pub(crate) internals the port modules drive ──────────────────────────────────────────────

    /// The SQLite backend handle (raw SQL stays inside `mod sqlite`).
    pub(crate) fn db(&self) -> &Db {
        &self.db
    }

    /// The configured plane signer, or a typed precondition fault — the pointer-move requires a key.
    pub(crate) fn plane_signer(&self) -> Result<&PlaneSigner> {
        self.signer
            .as_ref()
            .ok_or_else(|| AuthorityError::internal(NoPlaneKey))
    }

    /// The per-workspace git-store directory — one component under the confined root. `WorkspaceId` is
    /// a validated path-safe id (no separators, no `..`), so this can never escape `git_root`.
    fn workspace_git_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.git_root.join(ws.as_str())
    }

    /// The per-op upload-quarantine directory: `git_root/<ws>.quarantine/<op_id>`. `WorkspaceId` forbids
    /// `.`, so `<ws>.quarantine` can never collide with a real workspace store dir (`git_root/<ws>`), and
    /// it is a SIBLING of that store — so the GC scanner, which walks only `git_root/<ws>/`, never sees a
    /// quarantine. Both ids are validated path-safe newtypes, so the path can never escape `git_root`.
    /// (Used by the not-yet-wired lifecycle ops, so unreferenced in a non-test production build.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn workspace_quarantine_dir(
        &self,
        ws: &WorkspaceId,
        op_id: &crate::id::OpId,
    ) -> PathBuf {
        self.git_root
            .join(format!("{}.quarantine", ws.as_str()))
            .join(op_id.as_str())
    }

    /// Open the per-workspace git store for reading. A failure here is reached only after the database
    /// authorized the read, so a missing/un-openable store is a provenance/store divergence (corruption).
    pub(crate) fn open_store(&self, ws: &WorkspaceId) -> Result<Store> {
        Store::open(&self.workspace_git_dir(ws)).map_err(AuthorityError::integrity)
    }

    /// Open-or-create the per-workspace git store for an upload's object write (the bare repo is created
    /// on a workspace's first upload).
    ///
    /// Open first, then create, then open again on a failed create: two concurrent first-time uploads to
    /// the same workspace can both observe the directory as absent, and bare-repo `init` is not an
    /// idempotent open-or-create — so the loser of the creation race falls back to opening what the winner
    /// just made instead of failing. (A finer-grained guard against a writer racing *mid*-init lands with
    /// the broader concurrency work.)
    pub(crate) fn store_for_write(&self, ws: &WorkspaceId) -> Result<Store> {
        let dir = self.workspace_git_dir(ws);
        match Store::open(&dir) {
            Ok(store) => Ok(store),
            Err(_) => match Store::init(&dir) {
                Ok(store) => Ok(store),
                Err(_) => Store::open(&dir).map_err(AuthorityError::internal),
            },
        }
    }

    /// The per-workspace large-object store handle, rooted at `large_root/<ws>/`. `WorkspaceId` is a
    /// validated, path-safe id (no separators, no `..`), so the root can never escape `large_root` and one
    /// workspace's handle can never name another's bytes — cross-workspace isolation is the path itself, and
    /// byte-identical content in two workspaces is two distinct physical objects (no cross-workspace dedup).
    /// Construction stays inside this crate, so nothing outside the authority can fetch a large object by
    /// bare hash. Infallible: the store creates its directories lazily on the first `put`.
    pub(crate) fn large_store(&self, ws: &WorkspaceId) -> LocalLargeStore {
        LocalLargeStore::new(self.large_root.join(ws.as_str()))
    }
}

/// The pointer-move was attempted with no plane signing key configured (a precondition fault, not a
/// protocol outcome — wired as an internal error so no key state crosses the public boundary).
#[derive(Debug, thiserror::Error)]
#[error("no plane signing key configured (call with_plane_key)")]
struct NoPlaneKey;
