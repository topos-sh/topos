//! The sealed authority facade — the crate's one public type.

use std::path::{Path, PathBuf};

use topos_gitstore::Store;

use crate::error::{AuthorityError, Result};
use crate::id::{ObjectId, Principal, SkillId, WorkspaceId};
use crate::lineage::{CandidateCommit, LineageDecision};
use crate::sqlite::Db;
use crate::upload::{CandidateUpload, UploadReceipt};

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
}

impl Authority {
    /// Open the authority over a SQLite database file and a git-store root directory (both created if
    /// absent, with the schema migrated).
    ///
    /// # Errors
    /// [`AuthorityError::Internal`] if the store root cannot be created or the database cannot be
    /// opened or migrated.
    pub async fn open_sqlite(db_path: &Path, git_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(git_root).map_err(AuthorityError::internal)?;
        let db = Db::open(db_path).await?;
        Ok(Self {
            db,
            git_root: git_root.to_path_buf(),
        })
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

    // ── pub(crate) internals the port modules drive ──────────────────────────────────────────────

    /// The SQLite backend handle (raw SQL stays inside `mod sqlite`).
    pub(crate) fn db(&self) -> &Db {
        &self.db
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
}
