//! The write side: init/open, import a bundle as git objects, snapshot it as a commit (re-verifying its
//! `version_id`), and report the exact paths the client must fsync for crash-safe durability.

use std::path::{Path, PathBuf};

use gix::objs::tree::EntryKind;

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::sign::{self, Commit};

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
#[derive(Debug, Clone, Default)]
pub struct WriteBatch {
    /// Loose object files + ref files whose contents must reach disk.
    pub files: Vec<PathBuf>,
    /// Directories whose entries changed (object shards, the objects dir, the ref dirs) — fsync each so
    /// the new directory entries are durable, not just the file contents.
    pub dirs: Vec<PathBuf>,
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
        let recomputed = sign::commit_id(&frame).map_err(|_| GitstoreError::VersionMismatch)?;
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
    /// directories — what the client fsyncs to make the latest write durable. Over-syncing an
    /// already-durable object is a harmless no-op; this errs toward completeness for a fresh staging
    /// store (the only place `add` writes this increment).
    ///
    /// # Errors
    /// [`GitstoreError::Io`] if the store directory cannot be read.
    pub fn durability_set(&self) -> Result<WriteBatch, GitstoreError> {
        // The WHOLE git directory: not just the loose objects + version refs, but the repo metadata
        // (`HEAD`, `config`, the `objects/`/`refs/` dirs) `init_bare` created outside any fs seam. A doc
        // that names a commit must not become durable while the store it lives in can't even be opened.
        let mut batch = WriteBatch::default();
        collect_tree(&self.git_dir, &mut batch)?;
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

pub(crate) fn gix_err<E: std::fmt::Display>(e: E) -> GitstoreError {
    GitstoreError::Gix(format!("{e}"))
}

fn verify_gix<E: std::fmt::Display>(e: E) -> VerifyError {
    VerifyError::Gix(format!("{e}"))
}
