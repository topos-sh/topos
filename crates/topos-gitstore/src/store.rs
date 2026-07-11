//! The write side: init/open, import a bundle as git objects, snapshot it as a commit (re-verifying its
//! `version_id`), and report the exact paths the client must fsync for crash-safe durability.

use std::path::{Path, PathBuf};

use gix::objs::tree::EntryKind;

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::{self, Commit};

use crate::error::GitstoreError;
use crate::{VERSION_REF_PREFIX, error::VerifyError};

/// Fixed git committer identity for sidecar commits — keeps the git commit object (and thus its SHA-1
/// OID) reproducible. It is **not** topos identity: `version_id` is the kernel `commit_id`, which never
/// commits to git time or email.
const TOPOS_COMMITTER_NAME: &str = "topos";
const TOPOS_COMMITTER_EMAIL: &str = "topos@localhost";

/// One file handed to [`Store::write_bundle`]: its bundle-relative forward-slash path, mode, and raw
/// bytes. The caller (the client scanner) has already applied the filesystem-level reject rules.
#[derive(Debug, Clone, Copy)]
pub struct ImportFile<'a> {
    pub path: &'a str,
    pub mode: FileMode,
    pub bytes: &'a [u8],
}

/// The result of importing a bundle: the git tree holding the bytes, plus the kernel `bundle_digest`
/// computed over those exact bytes (the consent hash).
#[derive(Debug, Clone)]
pub struct TreeHandle {
    pub tree_oid: gix::ObjectId,
    pub bundle_digest: [u8; 32],
}

/// The set of paths the client must fsync (through its own fault-injectable fs seam) to make a write
/// durable **before** any JSON doc references it. `topos-gitstore` only *names* these — it never fsyncs,
/// so durability stays injectable in the client and this crate keeps no `~/.topos/` policy.
///
/// **The crash-safety contract:** everything a commit makes reachable — its blobs, its trees, the commit
/// object itself, and its version ref — must be durable before any sidecar doc records that version as
/// applied/base (or otherwise relies on rendering it). Each write path names its OWN set — the versions
/// it just wrote, via [`Store::version_durability`], accumulated across a multi-version op with
/// [`WriteBatch::extend`] — so the per-op fsync cost is bounded by what the op wrote, never by the
/// store's lifetime history. Over-syncing an already-durable path is a harmless no-op (duplicates in the
/// batch are the fsync loop's to dedup); the full-tree [`Store::durability_set`] remains only for a
/// fresh staging store, where the whole tree IS the op's writes.
#[derive(Debug, Clone, Default)]
pub struct WriteBatch {
    /// Loose object files + ref files whose contents must reach disk.
    pub files: Vec<PathBuf>,
    /// Directories whose entries changed (object shards, the objects dir, the ref dirs) — fsync each so
    /// the new directory entries are durable, not just the file contents.
    pub dirs: Vec<PathBuf>,
}

impl WriteBatch {
    /// Fold `other` into this batch — the accumulator a multi-write op (e.g. an ancestor backfill) uses
    /// to name everything it created, fsynced once at the end. Duplicate paths are tolerated: re-syncing
    /// is a harmless no-op, and the client's fsync loop dedups before paying for each call.
    pub fn extend(&mut self, other: WriteBatch) {
        self.files.extend(other.files);
        self.dirs.extend(other.dirs);
    }
}

/// A path-parameterized embedded-git object store (one bare repo per skill).
#[derive(Debug)]
pub struct Store {
    repo: gix::Repository,
    git_dir: PathBuf,
}

impl Store {
    /// Initialize a fresh **bare** repo at `git_dir` (no worktree, no index — the harness skill dir
    /// stays plain files with no `.git`).
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] if the repo cannot be created.
    pub fn init(git_dir: &Path) -> Result<Self, GitstoreError> {
        let repo = gix::init_bare(git_dir).map_err(gix_err)?;
        let git_dir = repo.git_dir().to_path_buf();
        Ok(Self { repo, git_dir })
    }

    /// Open an existing store at `git_dir`.
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] if the repo cannot be opened.
    pub fn open(git_dir: &Path) -> Result<Self, GitstoreError> {
        let repo = gix::open(git_dir).map_err(gix_err)?;
        let git_dir = repo.git_dir().to_path_buf();
        Ok(Self { repo, git_dir })
    }

    pub(crate) fn repo(&self) -> &gix::Repository {
        &self.repo
    }

    /// The store's git directory (the bare repo root). `pub(crate)` so the sibling `fence` module can
    /// address loose objects + refs by path for the durable install / unlink primitives.
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Import a bundle: validate every path through the kernel, write one git blob per file, and build a
    /// git tree mirroring the bundle paths + modes. Returns the tree handle (with the kernel
    /// `bundle_digest`). Writes objects but **no** commit/ref yet.
    ///
    /// # Errors
    /// [`GitstoreError::Reject`] if a path fails the canonical rules; [`GitstoreError::Gix`] on a write
    /// failure.
    pub fn write_bundle(&self, files: &[ImportFile<'_>]) -> Result<TreeHandle, GitstoreError> {
        // Validate + compute the consent digest FIRST (so a rejected bundle writes the least), through
        // the one kernel implementation — re-runs check_path + the collision rules.
        let entries: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.to_owned(),
                mode: f.mode,
                content_sha256: digest::sha256(f.bytes),
            })
            .collect();
        let bundle_digest = digest::bundle_digest(&entries).map_err(GitstoreError::Reject)?;

        let mut editor = self.repo.empty_tree().edit().map_err(gix_err)?;
        for f in files {
            let blob_oid = self.repo.write_blob(f.bytes).map_err(gix_err)?.detach();
            let kind = match f.mode {
                FileMode::Regular => EntryKind::Blob,
                FileMode::Executable => EntryKind::BlobExecutable,
            };
            editor.upsert(f.path, kind, blob_oid).map_err(gix_err)?;
        }
        let tree_oid = editor.write().map_err(gix_err)?.detach();
        Ok(TreeHandle {
            tree_oid,
            bundle_digest,
        })
    }

    /// Snapshot a tree as a commit and record it under `refs/topos/versions/<version_id>`.
    ///
    /// Re-derives the `version_id` from `(parents, tree.bundle_digest, author, message)` through the
    /// kernel `commit_id` and **refuses to write a ref unless it matches** — a caller cannot mint a ref
    /// that lies about its own identity. Parent `version_id`s are mapped to their git commits via their
    /// refs; a missing parent is a typed error.
    ///
    /// # Errors
    /// [`GitstoreError::VersionMismatch`] if the supplied id is not the kernel `commit_id`;
    /// [`GitstoreError::MissingParent`] if a parent ref is absent; [`GitstoreError::Gix`] on a write
    /// failure.
    pub fn commit(
        &self,
        version_id: [u8; 32],
        parents: &[[u8; 32]],
        tree: &TreeHandle,
        author: &str,
        message: &str,
    ) -> Result<gix::ObjectId, GitstoreError> {
        // Map parent version_ids -> git commits (fail closed on a missing parent).
        let mut parent_git: Vec<gix::ObjectId> = Vec::with_capacity(parents.len());
        for p in parents {
            let oid = self
                .resolve_version(p)
                .map_err(|e| GitstoreError::Gix(format!("{e}")))?
                .ok_or(GitstoreError::MissingParent)?;
            parent_git.push(oid);
        }

        // Re-derive the version_id and refuse a lying ref.
        let frame = Commit {
            parents,
            tree: tree.bundle_digest,
            author,
            message,
        };
        let recomputed = identity::commit_id(&frame).map_err(|_| GitstoreError::VersionMismatch)?;
        if recomputed != version_id {
            return Err(GitstoreError::VersionMismatch);
        }

        // Write the git commit with a fixed, reproducible committer/author frame.
        let time = gix::date::Time::new(0, 0);
        let author_sig = gix::actor::Signature {
            name: author.into(),
            email: TOPOS_COMMITTER_EMAIL.into(),
            time,
        };
        let committer_sig = gix::actor::Signature {
            name: TOPOS_COMMITTER_NAME.into(),
            email: TOPOS_COMMITTER_EMAIL.into(),
            time,
        };
        let mut buf_a = gix::date::parse::TimeBuf::default();
        let mut buf_c = gix::date::parse::TimeBuf::default();
        let commit = self
            .repo
            .new_commit_as(
                committer_sig.to_ref(&mut buf_c),
                author_sig.to_ref(&mut buf_a),
                message,
                tree.tree_oid,
                parent_git.iter().copied(),
            )
            .map_err(gix_err)?;
        let commit_oid = commit.id;

        self.set_version_ref(&version_id, commit_oid)?;
        Ok(commit_oid)
    }

    /// The full set of loose objects + topos version refs currently in the store, with their parent
    /// directories — what the client fsyncs to make a **fresh staging store** durable. Scope: ONLY a
    /// just-created store (`add`'s staged import; the `follow` baseline's empty init), where the whole
    /// tree IS this op's writes — including the repo metadata (`HEAD`, `config`, the `objects/`/`refs/`
    /// dirs) `init_bare` created outside any fs seam (a doc that names a commit must not become durable
    /// while the store it lives in can't even be opened). A store carrying history must NOT use this —
    /// its cost grows with every version ever written; a write onto an open store names its own set via
    /// [`Store::version_durability`] instead.
    ///
    /// # Errors
    /// [`GitstoreError::Io`] if the store directory cannot be read.
    pub fn durability_set(&self) -> Result<WriteBatch, GitstoreError> {
        let mut batch = WriteBatch::default();
        collect_tree(&self.git_dir, &mut batch)?;
        Ok(batch)
    }

    /// The durability set of ONE version this store holds: its version-ref file (+ the ref dir chain),
    /// its commit object, and every tree + blob object reachable from its tree — with the parent dirs
    /// whose entries changed. This is exactly what a [`Store::write_bundle`] + [`Store::commit`] pair
    /// created for that version, so a write path onto an open store fsyncs THIS (accumulating one batch
    /// per written version via [`WriteBatch::extend`]) instead of the whole store: the per-op fsync cost
    /// is bounded by the versions the op wrote, never by lifetime history. Objects shared with older
    /// versions are re-named (already durable — a harmless no-op), so the set is self-contained without
    /// tracking write novelty. Parent versions are NOT walked here — this set names ONE version's
    /// writes. A caller that must guarantee a parent is durable (a present parent may sit in the crash
    /// window between its write and its fsync, recorded nowhere) accumulates the parent's own set too,
    /// one call per version.
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] if the version ref is absent (the write being made durable should have
    /// just created it) or an object read fails.
    pub fn version_durability(&self, version_id: &[u8; 32]) -> Result<WriteBatch, GitstoreError> {
        let commit_oid = self
            .resolve_version(version_id)
            .map_err(|e| GitstoreError::Gix(format!("{e}")))?
            .ok_or_else(|| {
                GitstoreError::Gix("the version to make durable is not present".into())
            })?;
        let commit = self
            .repo
            .find_object(commit_oid)
            .map_err(gix_err)?
            .try_into_commit()
            .map_err(gix_err)?;
        let tree_oid = commit.tree_id().map_err(gix_err)?.detach();

        let mut batch = WriteBatch::default();
        let objects = self.git_dir.join("objects");
        push_loose(&mut batch, &objects, commit_oid);
        collect_tree_objects(&self.repo, tree_oid, &objects, true, 0, &mut batch)?;
        batch.dirs.push(objects);
        push_version_ref(&mut batch, &self.git_dir, version_id);
        Ok(batch)
    }

    /// Resolve a `version_id` to its git commit OID, or `None` if no such version ref exists.
    pub(crate) fn resolve_version(
        &self,
        version_id: &[u8; 32],
    ) -> Result<Option<gix::ObjectId>, VerifyError> {
        let name = version_ref_name(version_id);
        match self.repo.try_find_reference(&name).map_err(verify_gix)? {
            None => Ok(None),
            Some(mut r) => {
                let id = r.peel_to_id().map_err(verify_gix)?;
                Ok(Some(id.detach()))
            }
        }
    }

    fn set_version_ref(
        &self,
        version_id: &[u8; 32],
        commit_oid: gix::ObjectId,
    ) -> Result<(), GitstoreError> {
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        let name = version_ref_name(version_id);
        self.repo
            .edit_reference(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: "topos version".into(),
                    },
                    // A re-commit of identical content yields the identical git OID, so `Any` is an
                    // idempotent no-op; the prior `version_id` re-derivation already foreclosed a lie.
                    expected: PreviousValue::Any,
                    new: gix::refs::Target::Object(commit_oid),
                },
                name: name
                    .try_into()
                    .map_err(|e| GitstoreError::Gix(format!("{e}")))?,
                deref: false,
            })
            .map_err(gix_err)?;
        Ok(())
    }
}

/// The full ref name for a version: `refs/topos/versions/<version_id lowercase hex>`.
pub(crate) fn version_ref_name(version_id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(VERSION_REF_PREFIX.len() + 64);
    s.push_str(VERSION_REF_PREFIX);
    s.push_str(&digest::to_hex(version_id));
    s
}

fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>, GitstoreError> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| GitstoreError::Io(format!("{e}")))? {
        out.push(e.map_err(|e| GitstoreError::Io(format!("{e}")))?);
    }
    Ok(out)
}

/// Collect every file + directory under `dir` (recursively) into the durability batch — the client
/// fsyncs them all so the store is fully reconstructable after a crash.
fn collect_tree(dir: &Path, batch: &mut WriteBatch) -> Result<(), GitstoreError> {
    batch.dirs.push(dir.to_path_buf());
    for entry in read_dir(dir)? {
        let path = entry.path();
        if path.is_dir() {
            collect_tree(&path, batch)?;
        } else if path.is_file() {
            batch.files.push(path);
        }
    }
    Ok(())
}

/// The tree-nesting bound for the durability walk — the same bound the read-side walks apply, so a
/// version deep enough to be refused here was already unrenderable.
const MAX_TREE_DEPTH: usize = 64;

/// Recursively push every **tree** object reachable from `tree_oid` — and, when `include_blobs`, every
/// blob's loose path too — into the batch. The walk covers ONE version's tree only, so its size bounds
/// the durability set. (The server fence excludes blobs: their durability is the per-object install's;
/// an offloaded blob's bytes are not in git at all. The client write path includes them.)
pub(crate) fn collect_tree_objects(
    repo: &gix::Repository,
    tree_oid: gix::ObjectId,
    objects: &Path,
    include_blobs: bool,
    depth: usize,
    batch: &mut WriteBatch,
) -> Result<(), GitstoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(GitstoreError::Gix("tree nesting too deep".into()));
    }
    push_loose(batch, objects, tree_oid);
    let tree = repo
        .find_object(tree_oid)
        .map_err(gix_err)?
        .try_into_tree()
        .map_err(gix_err)?;
    for entry in tree.iter() {
        let entry = entry.map_err(gix_err)?;
        let oid = entry.oid().to_owned();
        match entry.mode().kind() {
            EntryKind::Tree => {
                collect_tree_objects(repo, oid, objects, include_blobs, depth + 1, batch)?;
            }
            _ if include_blobs => push_loose(batch, objects, oid),
            _ => {}
        }
    }
    Ok(())
}

/// Push one loose object's durability paths: the object file + its shard dir. (The `objects/` parent is
/// pushed once by the caller.)
pub(crate) fn push_loose(batch: &mut WriteBatch, objects: &Path, oid: gix::ObjectId) {
    let path = loose_path_in(objects, oid.as_slice());
    if let Some(shard) = path.parent() {
        batch.dirs.push(shard.to_path_buf());
    }
    batch.files.push(path);
}

/// Push a version-ref file + every ref dir up to (and including) the git dir, so a freshly-created ref
/// path (`refs/topos/versions/…`) is durable entry-by-entry.
pub(crate) fn push_version_ref(batch: &mut WriteBatch, git_dir: &Path, version_id: &[u8; 32]) {
    let ref_rel = version_ref_name(version_id); // refs/topos/versions/<hex>
    let ref_file = git_dir.join(&ref_rel);
    let mut dir = ref_file.parent();
    while let Some(d) = dir {
        batch.dirs.push(d.to_path_buf());
        if d == git_dir {
            break;
        }
        dir = d.parent();
    }
    batch.files.push(ref_file);
}

/// Build a loose-object path under an `objects/` dir from a raw git-oid byte slice.
pub(crate) fn loose_path_in(objects: &Path, oid: &[u8]) -> PathBuf {
    let hex = hex_lower(oid);
    objects.join(&hex[0..2]).join(&hex[2..])
}

/// Lowercase hex of a byte slice (no dependency; the kernel's hex is sized for 32-byte ids).
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub(crate) fn gix_err<E: std::fmt::Display>(e: E) -> GitstoreError {
    GitstoreError::Gix(format!("{e}"))
}

fn verify_gix<E: std::fmt::Display>(e: E) -> VerifyError {
    VerifyError::Gix(format!("{e}"))
}
