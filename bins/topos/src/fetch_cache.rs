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
//! **Fail-open, everywhere.** A cache is an optimization, never an authority: an unreadable entry,
//! a corrupt one, a full disk, a directory that cannot be created — each degrades to a plain
//! network fetch, and none of them can fail a command. The one thing the cache may never do is
//! hand back bytes that are not the object asked for, so every read RE-VERIFIES `sha256 == the
//! object id` and EVICTS an entry that fails (a truncated write from a crashed run, a hand-edited
//! file, a bit rot) rather than serving it. The transport verifies again on top of that
//! (`build_fetched_version`'s per-blob gate) — the cache never becomes the last word on the bytes.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use topos_core::digest::{self, to_hex};

use crate::atomic::atomic_write_new_at;
use crate::fs_seam::FsOps;

/// The per-process counter that makes each staged temp name unique (see [`FetchCache::write`]):
/// the pid alone is not enough, because one process writes many blobs of one version and may
/// retry an object it just failed on.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The machine-wide blob cache — one directory plus the fs seam every write rides.
///
/// `!Send` by construction (the `Rc<dyn FsOps>` seam is single-threaded, like every other durable
/// write in this crate): the transport's parallel blob fetch reads and writes it on the
/// COORDINATING thread only, and the worker threads carry nothing but sockets.
pub(crate) struct FetchCache {
    /// The objects directory itself (`<machine home>/cache/objects`). Created lazily, on the
    /// first write — a machine that never downloads a blob never grows the directory.
    dir: PathBuf,
    fs: Rc<dyn FsOps>,
}

impl std::fmt::Debug for FetchCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The seam is not `Debug`; the directory is the whole safe shape.
        f.debug_struct("FetchCache")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl FetchCache {
    /// The cache over `dir` (the caller resolves the MACHINE home — never a project store, whose
    /// bytes would be re-downloaded per checkout and deleted with it).
    pub(crate) fn new(dir: PathBuf, fs: Rc<dyn FsOps>) -> Self {
        Self { dir, fs }
    }

    /// Where one object's bytes live: the flat 64-hex spelling of its id.
    fn path_for(&self, object_id: &[u8; 32]) -> PathBuf {
        self.dir.join(to_hex(object_id))
    }

    /// The bytes for `object_id`, or `None` for every kind of miss there is — absent, unreadable,
    /// or NOT THE OBJECT ASKED FOR. The last case also EVICTS: an entry whose bytes no longer
    /// hash to its own name can never become valid again, and leaving it would cost every future
    /// run the same wasted read.
    pub(crate) fn read(&self, object_id: &[u8; 32]) -> Option<Vec<u8>> {
        let path = self.path_for(object_id);
        // The no-follow read, like every other document topos owns: a symlink at the name reads
        // as an error (a miss here), never through to whatever it points at.
        let bytes = self.fs.read_opt_nofollow(&path).ok()??;
        if digest::sha256(&bytes) == *object_id {
            Some(bytes)
        } else {
            let _ = self.fs.remove_file(&path);
            None
        }
    }

    /// Put `bytes` in the cache under `object_id` — BEST-EFFORT: every failure is swallowed, and
    /// a target another writer (or an earlier run) already landed is success, because the file's
    /// content is fixed by its name. The caller has verified `sha256(bytes) == object_id` before
    /// calling; this never writes bytes it was not told are the object's.
    ///
    /// The staged temp carries the pid and a process-monotonic counter, so two processes — or two
    /// blobs of one download — never share a staging name; the birth rename then elects one
    /// winner and the loser's staging is dropped. The crate has no logging facade, so a failure
    /// says nothing anywhere: a cache that cannot write is a cache that is not there.
    pub(crate) fn write(&self, object_id: &[u8; 32], bytes: &[u8]) {
        if self.fs.create_dir_all(&self.dir).is_err() {
            return;
        }
        let target = self.path_for(object_id);
        let tmp = self.tmp_beside(&target);
        if atomic_write_new_at(&*self.fs, &target, &tmp, bytes).is_err() {
            // A fault anywhere in the birth may leave the staging file; nothing sweeps this
            // directory, so clear it here (the unique name is nobody else's to collide with).
            let _ = self.fs.remove_file(&tmp);
        }
    }

    /// A staging path beside `target` that no concurrent writer can hold: `<target>.<pid>.<n>.tmp`.
    fn tmp_beside(&self, target: &Path) -> PathBuf {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut name = target.as_os_str().to_owned();
        name.push(format!(".{}.{n}.tmp", std::process::id()));
        PathBuf::from(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::{FaultFs, RealFs};

    /// A throwaway directory under the OS temp dir (no `tempfile` dep in this crate).
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-fc-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cache_at(dir: &Path) -> FetchCache {
        FetchCache::new(dir.join("objects"), Rc::new(RealFs))
    }

    /// The whole contract in one pass: a miss before anything is written, a hit on the exact
    /// bytes after, and a re-write of an object already held is success (not an overwrite).
    #[test]
    fn a_written_object_reads_back_and_a_rewrite_is_a_no_op() {
        let dir = scratch("hit");
        let cache = cache_at(&dir);
        let bytes = b"# a skill\n".to_vec();
        let id = digest::sha256(&bytes);
        assert!(cache.read(&id).is_none(), "nothing is cached yet");
        cache.write(&id, &bytes);
        assert_eq!(cache.read(&id).unwrap(), bytes);
        cache.write(&id, &bytes);
        assert_eq!(cache.read(&id).unwrap(), bytes, "still the same bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An entry whose bytes do NOT hash to its own name is never served, and never read twice:
    /// the read evicts it, so the next run takes a clean miss instead of the same wasted read.
    #[test]
    fn a_corrupt_entry_is_evicted_and_never_served() {
        let dir = scratch("corrupt");
        let cache = cache_at(&dir);
        let bytes = b"the real bytes\n".to_vec();
        let id = digest::sha256(&bytes);
        cache.write(&id, &bytes);
        let path = dir.join("objects").join(to_hex(&id));
        std::fs::write(&path, b"tampered\n").unwrap();
        assert!(cache.read(&id).is_none(), "corrupt bytes are never served");
        assert!(!path.exists(), "and the entry is evicted");
        // The next write repairs it.
        cache.write(&id, &bytes);
        assert_eq!(cache.read(&id).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink at an entry's name reads as a MISS, never through to the bytes it points at —
    /// the no-follow read is what keeps a planted link from feeding a fetch someone else's file.
    #[test]
    fn a_symlink_at_an_entry_is_a_miss() {
        let dir = scratch("link");
        let cache = cache_at(&dir);
        let bytes = b"payload\n".to_vec();
        let id = digest::sha256(&bytes);
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, &bytes).unwrap();
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("objects").join(to_hex(&id))).unwrap();
        assert!(cache.read(&id).is_none());
        assert_eq!(
            std::fs::read(&elsewhere).unwrap(),
            bytes,
            "the link's target is untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two processes downloading the SAME object land one valid file between them: each stages at
    /// its own unique temp, the no-replace birth elects one winner, and no staging litter is left
    /// behind. (Threads stand in for processes; each builds its own cache, because the seam is
    /// single-threaded by design.)
    #[test]
    fn racing_writers_of_one_object_leave_exactly_one_valid_file() {
        let dir = scratch("race");
        let objects = dir.join("objects");
        let bytes = b"contended bytes\n".to_vec();
        let id = digest::sha256(&bytes);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let objects = objects.clone();
                let bytes = bytes.clone();
                scope.spawn(move || {
                    FetchCache::new(objects, Rc::new(RealFs)).write(&id, &bytes);
                });
            }
        });
        let entries: Vec<PathBuf> = std::fs::read_dir(&objects)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "one file, no staging litter: {entries:?}");
        assert_eq!(std::fs::read(&entries[0]).unwrap(), bytes);
        assert_eq!(cache_at(&dir).read(&id).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fault at ANY step of the write leaves the cache with either NO entry or the WHOLE
    /// entry — never a truncated one a later run would serve as the object's bytes. (`read` would
    /// evict a torn entry anyway; this proves one is never produced in the first place.)
    #[test]
    fn faultfs_mid_write_never_leaves_a_torn_entry() {
        let bytes = b"a bundle blob that is written in one go\n".to_vec();
        let id = digest::sha256(&bytes);
        for fail_at in 1..=5 {
            let dir = scratch(&format!("fault{fail_at}"));
            let objects = dir.join("objects");
            FetchCache::new(objects.clone(), Rc::new(FaultFs::new(fail_at))).write(&id, &bytes);
            let entry = objects.join(to_hex(&id));
            if entry.exists() {
                assert_eq!(
                    std::fs::read(&entry).unwrap(),
                    bytes,
                    "fail_at={fail_at}: the final entry is torn"
                );
            }
            // Whatever the fault did, the cache still answers honestly.
            let cache = FetchCache::new(objects, Rc::new(RealFs));
            if let Some(got) = cache.read(&id) {
                assert_eq!(got, bytes, "fail_at={fail_at}: served the wrong bytes");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
