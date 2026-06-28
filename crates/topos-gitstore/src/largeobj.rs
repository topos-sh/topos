//! The size-routed large-object store — **the local-filesystem backend is wired here.**
//!
//! Big file blobs (those a server-side size route picks) are physically offloaded out of the embedded git
//! object store into a sharded, content-addressed directory keyed by the **same**
//! `blob_id = sha256(raw bytes)` the manifest already carries — the git+LFS pattern **minus the pointer
//! files** (no new primitive, no `.gitattributes`). Because identity is recomputed over real bytes (the
//! kernel sha256), *which* store physically holds a blob never changes its `blob_id`, the
//! `bundle_digest`, or any `version_id` — offload is a pure placement change, client-invisible.
//!
//! This is a **dumb byte layer**: content-addressed `put`/`get`/`exists`/`delete`, verify-on-read, and a
//! crash-safe two-phase install. It holds **no access control and no database** — the skill-scoped access
//! rule and the `object_presence.location` dispatch live in the authority crate (`plane-store`), which
//! constructs **one [`LocalLargeStore`] per workspace** (rooted at a per-workspace directory) so
//! cross-workspace isolation is the path itself, never a shared store. The deferred remote backend (an
//! S3-compatible object store) is a second impl of this one trait — a no-op extraction.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use topos_core::digest;

use crate::error::GitstoreError;

/// A content-addressed byte store keyed by `blob_id = sha256(raw bytes)`, with verify-on-read and a
/// crash-safe two-phase install (`temp → fsync → recompute-sha256 == blob_id → atomic rename → fsync`).
/// The [`LocalLargeStore`] below is the v0 local-filesystem impl; a deferred S3-compatible backend is a
/// second impl of this same trait.
pub trait LargeObjectStore {
    /// Store `bytes` under `blob_id` (the caller declares `blob_id == sha256(bytes)`; the impl re-checks).
    ///
    /// # Errors
    /// [`GitstoreError::BlobIntegrity`] if `sha256(bytes) != blob_id`; [`GitstoreError::Io`] on a write or
    /// durability failure.
    fn put(&self, blob_id: [u8; 32], bytes: &[u8]) -> Result<(), GitstoreError>;

    /// Fetch the bytes for `blob_id`, **re-verifying** `sha256(bytes) == blob_id` before returning.
    ///
    /// # Errors
    /// [`GitstoreError::Io`] if the object is absent or unreadable; [`GitstoreError::BlobIntegrity`] if the
    /// stored bytes do not re-hash to `blob_id` (at-rest corruption) — a verify failure is fatal.
    fn get(&self, blob_id: [u8; 32]) -> Result<Vec<u8>, GitstoreError>;

    /// Whether `blob_id` is present. An idempotency / re-materialize belt only — **never** the presence
    /// authority (the database's `object_presence` row is).
    ///
    /// # Errors
    /// [`GitstoreError::Io`] on an I/O failure other than not-found.
    fn exists(&self, blob_id: [u8; 32]) -> Result<bool, GitstoreError>;

    /// Remove `blob_id` (the GC unlink). Idempotent: removing an already-absent object is a no-op.
    ///
    /// # Errors
    /// [`GitstoreError::Io`] on a delete (other than not-found) or durability failure.
    fn delete(&self, blob_id: [u8; 32]) -> Result<(), GitstoreError>;
}

/// A bound on unique-temp-name attempts. A `(pid, monotonic counter)` temp name is unique within a
/// process, so the only collision is a leftover temp from a crashed prior process that reused this pid and
/// counter — astronomically rare, and a handful of retries clears it.
const MAX_TEMP_ATTEMPTS: u32 = 64;

/// A per-workspace, sharded, content-addressed large-object store on the local filesystem.
///
/// Layout under the per-workspace `root`:
/// - finals:  `root/objects/<hex[0..2]>/<hex[2..4]>/<64-hex sha256>`
/// - staging: `root/tmp/<64-hex>.<pid>.<n>.tmp` — a **sibling** of `objects/`, so it is on the **same
///   filesystem** and the install's last step is an atomic same-filesystem rename (never `$TMPDIR`, which
///   could be a different filesystem and make `rename` fail `EXDEV`).
///
/// `root` is **one workspace's** confined directory: the authority joins the validated, path-safe
/// `WorkspaceId` onto a shared large-object root and hands this type the result, so a handle can never
/// name another tenant's bytes, and byte-identical content in two workspaces is two distinct physical
/// objects (no cross-workspace dedup). The shard/file path is pure lowercase hex of the 32-byte id, so it
/// can never traverse out of `root`.
///
/// **Durability** matches the git fence's convention (`sync_all` after each write/rename, plus a parent-dir
/// fsync to make a new/removed name durable). On macOS/APFS `sync_all` does not force the drive's own
/// cache (`F_FULLFSYNC` would) — the same documented power-durability residual the git fence carries; this
/// layer deliberately does not exceed it. It buffers the whole blob in memory (bounded by the ingest
/// reject cap); a streaming `put`/`get` is a named later refinement.
#[derive(Debug, Clone)]
pub struct LocalLargeStore {
    root: PathBuf,
}

impl LocalLargeStore {
    /// Bind a store to **one workspace's** confined `root`. Infallible and does no I/O — directories are
    /// created lazily on the first `put`, so a read-only handle that is never written touches no disk.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// The final shard path for a blob: `objects/<aa>/<bb>/<64-hex>` (the full hex is the filename, so the
    /// content id is readable straight off the path).
    fn final_path(&self, blob_id: &[u8; 32]) -> PathBuf {
        let hex = digest::to_hex(blob_id);
        self.objects_dir()
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }
}

impl LargeObjectStore for LocalLargeStore {
    fn put(&self, blob_id: [u8; 32], bytes: &[u8]) -> Result<(), GitstoreError> {
        // Recompute the content id from the bytes IN MEMORY and refuse a mismatch BEFORE writing anything
        // — a correct-but-mislabeled object can never reach its final path, and a failed recompute leaves
        // no temp to clean. (Mirrors the git fence's in-memory hash check on install; at-rest corruption
        // is caught separately by `get`'s verify-on-read, so a disk read-back here would be stricter than
        // the surrounding code for no extra safety.)
        if digest::sha256(bytes) != blob_id {
            return Err(GitstoreError::BlobIntegrity);
        }
        let final_path = self.final_path(&blob_id);

        // Step 1: write the bytes to a uniquely-named temp on the same filesystem, then fsync the file so
        // its data is durable before it is given its final name.
        let tmp_dir = self.tmp_dir();
        std::fs::create_dir_all(&tmp_dir).map_err(io_err)?;
        let (temp_path, mut file) = open_unique_temp(&tmp_dir, &blob_id)?;
        if let Err(e) = write_and_sync(&mut file, bytes) {
            drop(file);
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }
        drop(file);

        // Step 2: create the shard dirs, atomically rename temp → final (overwriting any prior copy with
        // these freshly-verified bytes — so a re-put self-heals a crash-lost object), then fsync the shard
        // dir chain so the new name is durable.
        let shard = final_path
            .parent()
            .expect("a shard path always has a parent dir");
        if let Err(e) = std::fs::create_dir_all(shard).map_err(io_err) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&temp_path, &final_path).map_err(io_err) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }
        // fsync the new directory chain up to AND INCLUDING the shared large-object root — so on a
        // workspace's FIRST put, the freshly-created `<ws>/objects/aa/bb` directory ENTRIES (not just the
        // renamed file's data) are durable, and the new per-workspace dir's entry in the shared root is too.
        // `self.root.parent()` is that shared root (created at startup); a missing parent falls back to the
        // per-workspace root. (Same `sync_all` convention as the git fence; macOS `F_FULLFSYNC` is the only
        // residual.)
        let durable_up_to = self.root.parent().unwrap_or(self.root.as_path());
        fsync_dir_chain(shard, durable_up_to)
    }

    fn get(&self, blob_id: [u8; 32]) -> Result<Vec<u8>, GitstoreError> {
        let bytes = std::fs::read(self.final_path(&blob_id)).map_err(io_err)?;
        // Verify-on-read: the bytes are trusted only if they re-hash to the id, so disk corruption, a
        // truncated write, or a swapped file can never be returned as authentic.
        if digest::sha256(&bytes) != blob_id {
            return Err(GitstoreError::BlobIntegrity);
        }
        Ok(bytes)
    }

    fn exists(&self, blob_id: [u8; 32]) -> Result<bool, GitstoreError> {
        match std::fs::metadata(self.final_path(&blob_id)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(e)),
        }
    }

    fn delete(&self, blob_id: [u8; 32]) -> Result<(), GitstoreError> {
        let path = self.final_path(&blob_id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Idempotent: already gone. Do NOT return early — still fsync the shard below, exactly as the
            // git loose-object delete does. A prior unlink may have removed the file but crashed before the
            // dir-entry deletion was made durable; a recovery pass seeing not-found here must persist that
            // deletion (else a power loss could resurrect an untracked blob the DB has finalized to absent).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(e)),
        }
        // fsync the shard dir so the removal is durable (mirrors the git loose-object delete). `fsync_path`
        // tolerates a missing shard dir (returns Ok), so this is safe even when nothing was there.
        if let Some(shard) = path.parent() {
            fsync_path(shard)?;
        }
        Ok(())
    }
}

/// Open a fresh, uniquely-named temp file with `O_EXCL` (`create_new`), so we never clobber a concurrent
/// writer's temp or an attacker-planted file. The name embeds the pid + a process-monotonic counter, so a
/// collision is effectively impossible within a process; the bounded retry only covers a stale leftover.
fn open_unique_temp(
    tmp_dir: &Path,
    blob_id: &[u8; 32],
) -> Result<(PathBuf, std::fs::File), GitstoreError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let hex = digest::to_hex(blob_id);
    let pid = std::process::id();
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = tmp_dir.join(format!("{hex}.{pid}.{n}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(f) => return Ok((temp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(e)),
        }
    }
    Err(GitstoreError::Io(
        "could not create a unique temp file for the large-object install".to_owned(),
    ))
}

/// Write all bytes then fsync the file, so its data is durable before the rename gives it its final name.
fn write_and_sync(file: &mut std::fs::File, bytes: &[u8]) -> Result<(), GitstoreError> {
    file.write_all(bytes).map_err(io_err)?;
    file.sync_all().map_err(io_err)
}

/// fsync every directory from `leaf` up to and including `stop_at`, so a newly-created shard chain's
/// directory entries are durable, not just the renamed file's data. `stop_at` must be an ancestor of `leaf`
/// (the caller passes the large-object root, of which every shard path is a descendant), so the walk
/// terminates.
fn fsync_dir_chain(leaf: &Path, stop_at: &Path) -> Result<(), GitstoreError> {
    let mut dir = Some(leaf);
    while let Some(d) = dir {
        fsync_path(d)?;
        if d == stop_at {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// fsync one path (a file or directory) by opening it and syncing. A not-found path is tolerated (a
/// just-removed object's file is gone; its shard dir carries the durable removal).
fn fsync_path(path: &Path) -> Result<(), GitstoreError> {
    match std::fs::File::open(path) {
        Ok(f) => f.sync_all().map_err(io_err),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(e)),
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> GitstoreError {
    GitstoreError::Io(format!("{e}"))
}
