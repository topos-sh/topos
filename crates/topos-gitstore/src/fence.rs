//! The object-lifecycle fence primitives — the dumb byte ops the server-side garbage-collection fence
//! needs over the git object store. They hold **no database and no access control** (those live in the
//! authority crate); they only move/verify/unlink bytes and report what they made durable.
//!
//! Three operations: **stage** a candidate's blobs into a per-op quarantine object store (the GC scanner
//! never touches it); **install** one staged blob into the main store **durably** (write at the final
//! loose-object path, then fsync the object + its parent dirs — so the authority may flip the object to
//! `present` only after the bytes are guaranteed on disk); and **delete** a loose object for the GC unlink.
//! Plus **`commit_durable`**, which builds a candidate's tree from already-installed blob ids (it writes
//! NO blobs — every blob must already be fenced-installed) and records the commit + version ref durably.
//!
//! Unlike the client-facing write path (which only *names* a durability set for the client to fsync), these
//! server-side ops are **self-durable** and RETURN the path set they synced, so the durability is explicit
//! and a test can assert it. The git object store is never packed here, so every object is loose.

use std::path::{Path, PathBuf};

use gix::objs::tree::EntryKind;

use topos_core::digest::{self, FileMode, ManifestEntry};

use crate::error::GitstoreError;
use crate::store::{ImportFile, Store, TreeHandle, gix_err, version_ref_name};

/// The width of a git object id (SHA-1), the locator the authority records for a fenced object.
pub const GIT_OID_LEN: usize = 20;

/// One file staged into the quarantine: its bundle-relative path + mode, its topos `object_id`
/// (`sha256(raw bytes)`, the authority's identity), the git `git_oid` (the physical locator), and size.
#[derive(Debug, Clone)]
pub struct StagedEntry {
    pub path: String,
    pub mode: FileMode,
    pub object_id: [u8; 32],
    pub git_oid: [u8; GIT_OID_LEN],
    pub size: u64,
}

/// The result of staging a candidate bundle into a quarantine: one entry per file plus the kernel
/// `bundle_digest` over the exact bytes (the consent hash, computed before any write so a rejected bundle
/// stages nothing).
#[derive(Debug, Clone)]
pub struct StagedBundle {
    pub entries: Vec<StagedEntry>,
    pub bundle_digest: [u8; 32],
}

impl Store {
    /// Stage a candidate bundle's blobs into a per-op quarantine object store (a fresh bare repo at
    /// `quarantine_dir`, which the main-store GC scanner never walks), durably. Validates every path and
    /// computes the kernel `bundle_digest` FIRST (a rejected bundle writes nothing), then writes one blob
    /// per file and fsyncs the quarantine so the staged bytes survive a crash before migration.
    ///
    /// # Errors
    /// [`GitstoreError::Reject`] if a path fails the canonical rules; [`GitstoreError::Gix`] /
    /// [`GitstoreError::Io`] on a write or durability failure.
    pub fn stage(
        quarantine_dir: &Path,
        files: &[ImportFile<'_>],
    ) -> Result<StagedBundle, GitstoreError> {
        // Validate + compute the consent digest through the one kernel implementation (re-runs check_path
        // + the collision rules), before writing any object.
        let manifest: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.to_owned(),
                mode: f.mode,
                content_sha256: digest::sha256(f.bytes),
            })
            .collect();
        let bundle_digest = digest::bundle_digest(&manifest).map_err(GitstoreError::Reject)?;

        // Ensure the parent (`<ws>.quarantine/`) exists so `init_bare` can create the per-op leaf — the
        // main store relies on its `git_root` pre-existing the same way.
        if let Some(parent) = quarantine_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| GitstoreError::Io(format!("{e}")))?;
        }
        // Stage into a FRESH quarantine: `init_bare` rejects a non-empty directory, so a retry / re-ingest
        // that reuses the op id (the authority's quarantine row is an upsert, so reuse is a supported path)
        // would otherwise fail on the leftover repo. Clear any prior contents first — the quarantine holds
        // only this op's in-flight bytes, never anything to preserve — so the staged tree is exactly THIS
        // candidate's.
        if quarantine_dir.exists() {
            std::fs::remove_dir_all(quarantine_dir)
                .map_err(|e| GitstoreError::Io(format!("{e}")))?;
        }
        let quarantine = Store::init(quarantine_dir)?;
        let mut entries = Vec::with_capacity(files.len());
        for f in files {
            let object_id = digest::sha256(f.bytes);
            let oid = quarantine
                .repo()
                .write_blob(f.bytes)
                .map_err(gix_err)?
                .detach();
            entries.push(StagedEntry {
                path: f.path.to_owned(),
                mode: f.mode,
                object_id,
                git_oid: oid_to_array(oid)?,
                size: f.bytes.len() as u64,
            });
        }
        // Persist the quarantine durably (its bytes are the source migrate copies from). (Residual: this
        // syncs the quarantine repo's own object + dir entries but not its `<op_id>` entry in the parent
        // `<ws>.quarantine/`; under WAL + synchronous=NORMAL a crash could lose a freshly-staged quarantine
        // while its authority row survives — a transient, re-uploadable loss of IN-FLIGHT bytes, never
        // committed data, in the same deferred power-durability bucket as the upload-side fsync gaps.)
        fsync_batch(&quarantine.durability_set()?)?;
        Ok(StagedBundle {
            entries,
            bundle_digest,
        })
    }

    /// Install one staged blob from `quarantine` into this (main) store **durably**: read the blob bytes
    /// from the quarantine by git id, write them at the final loose-object path (gix's atomic
    /// temp-then-rename), then fsync the loose object + its shard + the `objects/` parent. Returns the
    /// synced path set. The authority flips the object to `present` only AFTER this returns, so a `present`
    /// row always denotes bytes durably at their final path. Writing identical content-addressed bytes is
    /// idempotent (two concurrent installs converge on the same loose object).
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] if the blob is absent/non-blob in the quarantine or the write fails;
    /// [`GitstoreError::Io`] on a durability failure.
    pub fn install_object_durable(
        &self,
        quarantine: &Store,
        git_oid: [u8; GIT_OID_LEN],
    ) -> Result<crate::store::WriteBatch, GitstoreError> {
        let oid = oid_from_array(git_oid)?;
        let object = quarantine.repo().find_object(oid).map_err(gix_err)?;
        if object.kind != gix::objs::Kind::Blob {
            return Err(GitstoreError::Gix("quarantine object is not a blob".into()));
        }
        let bytes = object.detach().data;
        // gix writes the loose object to its final path (temp + rename). write_blob returns the id of what
        // it actually wrote (the hash of `bytes`); assert it equals the requested `git_oid` so a quarantine
        // object corrupted after staging can NEVER be marked present under a locator whose path now holds
        // nothing — the bytes are verified to be at their final path before we report durability. We add the
        // fsync for durability.
        let written = self.repo().write_blob(&bytes).map_err(gix_err)?.detach();
        if written != oid {
            return Err(GitstoreError::Gix(
                "staged object failed verification on install (bytes do not match the locator)"
                    .into(),
            ));
        }
        let batch = self.loose_durability(git_oid);
        fsync_batch(&batch)?;
        Ok(batch)
    }

    /// Whether a git object is present in this store (a `gix` existence check across loose + packed). This is
    /// an **idempotency / integrity belt only** — the authority's `object_presence` row is the sole
    /// presence/dedup AUTHORITY; the store is never statted to DECIDE presence/dedup. The one sanctioned use
    /// is defensive: before a migrate roots a version over a row the DB already calls `present`, it stats the
    /// bytes and re-materializes them if a past crash (the WAL power-loss residual) removed the loose object —
    /// re-asserting "no root over gone bytes" without overriding the DB's status.
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] on an object-database fault.
    pub fn object_exists(&self, git_oid: [u8; GIT_OID_LEN]) -> Result<bool, GitstoreError> {
        let oid = oid_from_array(git_oid)?;
        Ok(self.repo().has_object(oid))
    }

    /// Build a candidate's tree in THIS (main) store from already-installed blobs — writing **no** blob
    /// bytes. Mirrors paths + modes onto the recorded git ids. Used by [`Self::commit_durable`]; never
    /// re-issues `write_blob`, so the fence is not bypassed.
    ///
    /// Uses the **low-level (plumbing) tree editor**, which — unlike `repo.empty_tree().edit()` — does NOT
    /// verify that each entry's blob object exists. That is load-bearing for the size-routed offload: an
    /// **offloaded** blob's bytes live in the large-object store, so its git object is intentionally absent
    /// here, and the high-level editor would refuse the whole tree. Identity is unaffected — `version_id` /
    /// `bundle_digest` are over real-byte sha256s, never the git tree OID — and the tree still faithfully
    /// carries every file's `(path, mode, git_oid)`, so the location-dispatching render can recover and
    /// fetch each blob from the store the database records.
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] on a tree-write failure.
    pub fn write_tree(
        &self,
        entries: &[(&str, FileMode, [u8; GIT_OID_LEN])],
    ) -> Result<gix::ObjectId, GitstoreError> {
        let repo = self.repo();
        let mut editor = gix::objs::tree::Editor::new(
            gix::objs::Tree {
                entries: Vec::new(),
            },
            &repo.objects,
            repo.object_hash(),
        );
        for (path, mode, git_oid) in entries {
            let oid = oid_from_array(*git_oid)?;
            let kind = match mode {
                FileMode::Regular => EntryKind::Blob,
                FileMode::Executable => EntryKind::BlobExecutable,
            };
            // Restore the component validation the high-level editor applied (the plumbing editor skips it):
            // reject `.git`/`.gitmodules` + their HFS+/NTFS aliases, path separators, and Windows
            // devices/illegal chars — so the fenced migrate can never record a tree the client write path
            // (which validates the same way via the high-level editor) would refuse. We pin all protections
            // on (`Options::default`) rather than reading a repo's git config: a server distributing bundles
            // to heterogeneous clients should reject every platform's aliases regardless of its own host, and
            // it agrees with the client on the security-critical `.git`/`.gitmodules` literals (rejected
            // under any options). (The kernel `check_path` covers `.`/`..`/NUL/absolute; this covers the
            // rest.) Symlinks are never written here, so `mode = None`.
            for component in path.split('/') {
                gix::validate::path::component(
                    component.into(),
                    None,
                    gix::validate::path::component::Options::default(),
                )
                .map_err(|e| GitstoreError::RejectPath(format!("{component:?}: {e}")))?;
            }
            // The path is split into components (`a/b/c`), matching the high-level editor's behavior; the
            // intermediate trees are created without requiring the leaf blob to be present.
            editor.upsert(path.split('/'), kind, oid).map_err(gix_err)?;
        }
        let tree_oid = editor
            .write(|tree| repo.write_object(tree).map(gix::Id::detach))
            .map_err(gix_err)?;
        Ok(tree_oid)
    }

    /// Read one staged blob's raw bytes from THIS (quarantine) store by its git id — the dumb byte fetch a
    /// size-routed migrate uses to copy an **offloaded** blob from the quarantine into the large-object
    /// store (the large-side analog of [`Self::install_object_durable`]'s internal read for the git side).
    /// Asserts the object is a blob; identity is re-checked by the large store's own `put`/verify-on-read.
    ///
    /// # Errors
    /// [`GitstoreError::Gix`] if the object is absent or not a blob in this store.
    pub fn read_staged_blob(&self, git_oid: [u8; GIT_OID_LEN]) -> Result<Vec<u8>, GitstoreError> {
        let oid = oid_from_array(git_oid)?;
        let object = self.repo().find_object(oid).map_err(gix_err)?;
        if object.kind != gix::objs::Kind::Blob {
            return Err(GitstoreError::Gix("staged object is not a blob".into()));
        }
        Ok(object.detach().data)
    }

    /// Record a migrated candidate's commit durably: build its tree from the installed blob ids, write the
    /// commit + the `refs/topos/versions/<version_id>` ref (re-deriving and refusing a lying `version_id`,
    /// exactly as [`Store::commit`] does), then fsync the new tree + commit objects and the version ref so
    /// the version is reconstructable after a crash. The blobs must already be fence-installed (their
    /// durability is the install's responsibility). Returns the synced path set.
    ///
    /// # Errors
    /// [`GitstoreError::VersionMismatch`] / [`GitstoreError::MissingParent`] / [`GitstoreError::Gix`] as
    /// [`Store::commit`]; [`GitstoreError::Io`] on a durability failure.
    pub fn commit_durable(
        &self,
        version_id: [u8; 32],
        parents: &[[u8; 32]],
        entries: &[(&str, FileMode, [u8; GIT_OID_LEN])],
        bundle_digest: [u8; 32],
        author: &str,
        message: &str,
    ) -> Result<crate::store::WriteBatch, GitstoreError> {
        let tree_oid = self.write_tree(entries)?;
        let commit_oid = self.commit(
            version_id,
            parents,
            &TreeHandle {
                tree_oid,
                bundle_digest,
            },
            author,
            message,
        )?;
        let batch = self.commit_durability(tree_oid, commit_oid, &version_id);
        fsync_batch(&batch)?;
        Ok(batch)
    }

    /// Delete one loose object (the GC unlink step), then fsync the shard dir so the removal is durable.
    /// Idempotent: a re-delete of an already-gone object (the recovery sweep re-running) is a no-op.
    ///
    /// # Errors
    /// [`GitstoreError::Io`] on a delete (other than not-found) or durability failure.
    pub fn delete_loose_object(
        &self,
        git_oid: [u8; GIT_OID_LEN],
    ) -> Result<crate::store::WriteBatch, GitstoreError> {
        let path = self.loose_path(git_oid);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(GitstoreError::Io(format!("{e}"))),
        }
        let mut batch = crate::store::WriteBatch::default();
        if let Some(shard) = path.parent() {
            batch.dirs.push(shard.to_path_buf());
        }
        fsync_batch(&batch)?;
        Ok(batch)
    }

    /// The loose-object path for a git id: `<git_dir>/objects/<first 2 hex>/<remaining 38 hex>`.
    fn loose_path(&self, git_oid: [u8; GIT_OID_LEN]) -> PathBuf {
        let hex = hex_lower(&git_oid);
        self.git_dir()
            .join("objects")
            .join(&hex[0..2])
            .join(&hex[2..])
    }

    /// The durability set for a freshly-installed loose object: the object file, its shard dir, and the
    /// `objects/` parent (fsynced in that order — file, then each parent whose entry changed).
    fn loose_durability(&self, git_oid: [u8; GIT_OID_LEN]) -> crate::store::WriteBatch {
        let objects = self.git_dir().join("objects");
        let path = self.loose_path(git_oid);
        let mut batch = crate::store::WriteBatch::default();
        if let Some(shard) = path.parent() {
            batch.dirs.push(shard.to_path_buf());
        }
        batch.dirs.push(objects);
        batch.files.push(path);
        batch
    }

    /// The durability set for a recorded commit: the new tree + commit loose objects and the version-ref
    /// file, plus every parent dir whose entry changed.
    fn commit_durability(
        &self,
        tree_oid: gix::ObjectId,
        commit_oid: gix::ObjectId,
        version_id: &[u8; 32],
    ) -> crate::store::WriteBatch {
        let git_dir = self.git_dir();
        let objects = git_dir.join("objects");
        let mut batch = crate::store::WriteBatch::default();
        for oid in [tree_oid, commit_oid] {
            let path = loose_path_in(&objects, oid.as_slice());
            if let Some(shard) = path.parent() {
                batch.dirs.push(shard.to_path_buf());
            }
            batch.files.push(path);
        }
        batch.dirs.push(objects);
        // The version ref file + its dir chain.
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
        batch
    }
}

/// Build a loose-object path under an `objects/` dir from a raw git-oid byte slice.
fn loose_path_in(objects: &Path, oid: &[u8]) -> PathBuf {
    let hex = hex_lower(oid);
    objects.join(&hex[0..2]).join(&hex[2..])
}

/// fsync every file then every directory in the batch (file data first, then the dir entries that
/// reference it). A directory fsync makes a newly-created or removed entry durable.
fn fsync_batch(batch: &crate::store::WriteBatch) -> Result<(), GitstoreError> {
    for f in &batch.files {
        fsync_path(f)?;
    }
    for d in &batch.dirs {
        fsync_path(d)?;
    }
    Ok(())
}

/// fsync one path (a file or a directory) by opening it and syncing. A not-found path is tolerated (a
/// just-deleted loose object's file is gone; its shard dir is what carries the durable removal).
fn fsync_path(path: &Path) -> Result<(), GitstoreError> {
    match std::fs::File::open(path) {
        Ok(f) => f.sync_all().map_err(|e| GitstoreError::Io(format!("{e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(GitstoreError::Io(format!("{e}"))),
    }
}

/// Lowercase hex of a byte slice (no dependency; the kernel's hex is sized for 32-byte ids).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// A git [`gix::ObjectId`] from a 20-byte locator. A wrong width (e.g. a sha256-OID repo) is a typed
/// error rather than a panic — these repos are sha1, so this always succeeds in practice.
fn oid_from_array(git_oid: [u8; GIT_OID_LEN]) -> Result<gix::ObjectId, GitstoreError> {
    gix::ObjectId::try_from(git_oid.as_slice())
        .map_err(|e| GitstoreError::Gix(format!("bad git oid: {e}")))
}

/// A 20-byte locator from a git [`gix::ObjectId`] (sha1). A non-20-byte id is a typed error.
fn oid_to_array(oid: gix::ObjectId) -> Result<[u8; GIT_OID_LEN], GitstoreError> {
    oid.as_slice()
        .try_into()
        .map_err(|_| GitstoreError::Gix("git oid is not 20 bytes (sha1 expected)".into()))
}
