//! The MACHINE-WIDE fetched-object cache: content-addressed blob bytes under
//! `<machine home>/cache/objects/<64-hex sha256>`, keyed by the very object id the plane names.
//!
//! It sits INSIDE the byte transport ([`crate::plane_http::UreqPlane`]) and nowhere else, so no
//! verb, engine, or store knows it exists — a fetch either finds the bytes here or dials for them,
//! and everything downstream sees the same [`crate::plane::FetchedVersion`] either way. That is
//! also why it is machine-wide rather than per-scope: an object id IS the bytes, so a blob a
//! project checkout downloaded is the same blob the machine store wants, and one download serves
//! every scope on the box.
//!
//! **Anchored like every other durable write in this crate.** The `cache/objects` chain is built
//! and proven with the no-follow walk ([`FsOps::create_dir_nofollow`] below the machine home), and
//! a blob LANDS through the held handle as a kernel-EXCLUSIVE create at its final name
//! ([`FsOps::write_new_at`]): no staging file, no path-based rename, and a `cache` component
//! swapped for a symlink is met as itself and refused — a download can never create bytes outside
//! the sidecar. Two racing writers get exactly one winner; the loser's `AlreadyExists` is success,
//! because the file's content is fixed by its name. The residue of a crash mid-create — a
//! truncated FINAL entry — is exactly what the read-side eviction is for.
//!
//! **Fail-open, everywhere.** A cache is an optimization, never an authority: an unreadable entry,
//! a corrupt one, a full disk, a chain that cannot be proven — each degrades to a plain network
//! fetch, and none of them can fail a command. The one thing the cache may never do is hand back
//! bytes that are not the object asked for, so every read RE-VERIFIES `sha256 == the object id`
//! and EVICTS an entry that fails (a truncated write from a crashed run, a hand-edited file, bit
//! rot) rather than serving it — the eviction runs only after the chain proof re-verifies, so it
//! removes inside the proven directory or not at all. The transport verifies again on top of that
//! (`build_fetched_version`'s per-blob gate) — the cache never becomes the last word on the bytes.

use std::path::PathBuf;
use std::rc::Rc;

use topos_core::digest::{self, to_hex};

use crate::fs_seam::{DirHandle, FsOps};

/// The machine-wide blob cache — the machine home it anchors under, plus the fs seam every
/// operation rides.
///
/// `!Send` by construction (the `Rc<dyn FsOps>` seam is single-threaded, like every other durable
/// write in this crate): the transport's parallel blob fetch reads and writes it on the
/// COORDINATING thread only, and the worker threads carry nothing but sockets.
pub(crate) struct FetchCache {
    /// The MACHINE home the `cache/objects` chain is proven below (the caller resolves it —
    /// never a project store, whose bytes would be re-downloaded per checkout and deleted with
    /// it). The chain is created lazily, on first use — a machine that never downloads a blob
    /// never grows the directory.
    home: PathBuf,
    fs: Rc<dyn FsOps>,
}

impl std::fmt::Debug for FetchCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The seam is not `Debug`; the anchor is the whole safe shape.
        f.debug_struct("FetchCache")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl FetchCache {
    /// The cache anchored under `home` (the resolved MACHINE home).
    pub(crate) fn new(home: PathBuf, fs: Rc<dyn FsOps>) -> Self {
        Self { home, fs }
    }

    /// The objects directory's path: `<home>/cache/objects`.
    fn objects_dir(&self) -> PathBuf {
        self.home.join("cache").join("objects")
    }

    /// Prove (and lazily create) the `cache/objects` chain below the home, no-follow at every
    /// component, answering the HELD handle the entry ops run at. `None` = the chain cannot be
    /// proven (no home yet, a symlinked component, permissions) — the cache is simply not there.
    fn objects_handle(&self) -> Option<DirHandle> {
        self.fs
            .create_dir_nofollow(&self.home, &self.objects_dir())
            .ok()
    }

    /// The bytes for `object_id`, or `None` for every kind of miss there is — absent, unreadable,
    /// or NOT THE OBJECT ASKED FOR. The last case also EVICTS: an entry whose bytes no longer
    /// hash to its own name can never become valid again, and leaving it would cost every future
    /// run the same wasted read. The eviction re-proves the handle first, so it removes inside
    /// the proven directory or not at all.
    pub(crate) fn read(&self, object_id: &[u8; 32]) -> Option<Vec<u8>> {
        let handle = self.objects_handle()?;
        let path = self.objects_dir().join(to_hex(object_id));
        // The no-follow read, like every other document topos owns: a symlink at the name reads
        // as an error (a miss here), never through to whatever it points at. The chain above it
        // was proven a beat ago by the handle walk.
        let bytes = self.fs.read_opt_nofollow(&path).ok()??;
        if digest::sha256(&bytes) == *object_id {
            Some(bytes)
        } else {
            // Destructive, so anchored: only when the path STILL names the proven directory
            // object is the corrupt entry removed (the crate's proof-then-act discipline; the
            // one-beat window between the re-proof and the unlink is the module-documented
            // irreducible residual of a path-based removal).
            if handle.verify_unmoved().is_ok() {
                let _ = self.fs.remove_file(&path);
            }
            None
        }
    }

    /// Put `bytes` in the cache under `object_id` — BEST-EFFORT: every failure is swallowed, and
    /// a target another writer (or an earlier run) already landed is success, because the file's
    /// content is fixed by its name. The caller has verified `sha256(bytes) == object_id` before
    /// calling; this never writes bytes it was not told are the object's.
    ///
    /// The landing is [`FsOps::write_new_at`] at the proven handle: a kernel-exclusive create at
    /// the FINAL name (two racers elect one winner; the loser changes nothing), fsynced file and
    /// directory entry, no staging file to litter or sweep. The crate has no logging facade, so a
    /// failure says nothing anywhere: a cache that cannot write is a cache that is not there.
    pub(crate) fn write(&self, object_id: &[u8; 32], bytes: &[u8]) {
        let Some(handle) = self.objects_handle() else {
            return;
        };
        let _ = self.fs.write_new_at(&handle, &to_hex(object_id), bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::{FaultFs, RealFs};
    use std::path::Path;

    /// A throwaway HOME under the OS temp dir (no `tempfile` dep in this crate).
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-fc-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cache_at(home: &Path) -> FetchCache {
        FetchCache::new(home.to_path_buf(), Rc::new(RealFs))
    }

    fn entry_path(home: &Path, id: &[u8; 32]) -> PathBuf {
        home.join("cache").join("objects").join(to_hex(id))
    }

    /// The whole contract in one pass: a miss before anything is written, a hit on the exact
    /// bytes after, and a re-write of an object already held is success (not an overwrite).
    #[test]
    fn a_written_object_reads_back_and_a_rewrite_is_a_no_op() {
        let home = scratch("hit");
        let cache = cache_at(&home);
        let bytes = b"# a skill\n".to_vec();
        let id = digest::sha256(&bytes);
        assert!(cache.read(&id).is_none(), "nothing is cached yet");
        cache.write(&id, &bytes);
        assert_eq!(cache.read(&id).unwrap(), bytes);
        cache.write(&id, &bytes);
        assert_eq!(cache.read(&id).unwrap(), bytes, "still the same bytes");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// An entry whose bytes do NOT hash to its own name is never served, and never read twice:
    /// the read evicts it, so the next run takes a clean miss instead of the same wasted read.
    #[test]
    fn a_corrupt_entry_is_evicted_and_never_served() {
        let home = scratch("corrupt");
        let cache = cache_at(&home);
        let bytes = b"the real bytes\n".to_vec();
        let id = digest::sha256(&bytes);
        cache.write(&id, &bytes);
        let path = entry_path(&home, &id);
        // Entries land read-only-ish via the exclusive create; rewrite through std for the test.
        std::fs::write(&path, b"tampered\n").unwrap();
        assert!(cache.read(&id).is_none(), "corrupt bytes are never served");
        assert!(!path.exists(), "and the entry is evicted");
        // The next write repairs it.
        cache.write(&id, &bytes);
        assert_eq!(cache.read(&id).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A symlink at an entry's name reads as a MISS, never through to the bytes it points at —
    /// the no-follow read is what keeps a planted link from feeding a fetch someone else's file.
    #[test]
    fn a_symlink_at_an_entry_is_a_miss() {
        let home = scratch("link");
        let cache = cache_at(&home);
        let bytes = b"payload\n".to_vec();
        let id = digest::sha256(&bytes);
        let elsewhere = home.join("elsewhere");
        std::fs::write(&elsewhere, &bytes).unwrap();
        std::fs::create_dir_all(home.join("cache").join("objects")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, entry_path(&home, &id)).unwrap();
        assert!(cache.read(&id).is_none());
        assert_eq!(
            std::fs::read(&elsewhere).unwrap(),
            bytes,
            "the link's target is untouched"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A `cache` component swapped for a symlink REFUSES the whole operation: the anchored walk
    /// meets the link as itself, so a write lands NOTHING outside the sidecar and a read is a
    /// plain miss. This is the containment the crate's dir-handle rule exists for.
    #[test]
    fn a_symlinked_intermediate_refuses_writes_and_misses_reads() {
        let home = scratch("redirect");
        let outside = scratch("redirect-target");
        std::os::unix::fs::symlink(&outside, home.join("cache")).unwrap();
        let cache = cache_at(&home);
        let bytes = b"must not escape\n".to_vec();
        let id = digest::sha256(&bytes);
        cache.write(&id, &bytes);
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing may land through the symlinked component"
        );
        assert!(cache.read(&id).is_none(), "and a read is a plain miss");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Two processes downloading the SAME object land one valid file between them: the exclusive
    /// create elects one winner at the final name, the losers change nothing, and there is no
    /// staging litter because there is no staging file at all. (Threads stand in for processes;
    /// each builds its own cache, because the seam is single-threaded by design.)
    #[test]
    fn racing_writers_of_one_object_leave_exactly_one_valid_file() {
        let home = scratch("race");
        let bytes = b"contended bytes\n".to_vec();
        let id = digest::sha256(&bytes);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let home = home.clone();
                let bytes = bytes.clone();
                scope.spawn(move || {
                    FetchCache::new(home, Rc::new(RealFs)).write(&id, &bytes);
                });
            }
        });
        let objects = home.join("cache").join("objects");
        let entries: Vec<PathBuf> = std::fs::read_dir(&objects)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "one file, no staging litter: {entries:?}");
        assert_eq!(std::fs::read(&entries[0]).unwrap(), bytes);
        assert_eq!(cache_at(&home).read(&id).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A fault at ANY step of the write leaves the cache with either NO entry or the WHOLE
    /// entry — never a torn one a later run would serve as the object's bytes. (`read` would
    /// evict a torn entry anyway; this proves the seam's fault points don't produce one.)
    #[test]
    fn faultfs_mid_write_never_leaves_a_torn_entry() {
        let bytes = b"a bundle blob that is written in one go\n".to_vec();
        let id = digest::sha256(&bytes);
        for fail_at in 1..=5 {
            let home = scratch(&format!("fault{fail_at}"));
            FetchCache::new(home.clone(), Rc::new(FaultFs::new(fail_at))).write(&id, &bytes);
            let entry = entry_path(&home, &id);
            if entry.exists() {
                assert_eq!(
                    std::fs::read(&entry).unwrap(),
                    bytes,
                    "fail_at={fail_at}: the final entry is torn"
                );
            }
            // Whatever the fault did, the cache still answers honestly.
            let cache = cache_at(&home);
            if let Some(got) = cache.read(&id) {
                assert_eq!(got, bytes, "fail_at={fail_at}: served the wrong bytes");
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }
}
