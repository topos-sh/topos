//! The MACHINE-WIDE fetched-object cache: content-addressed blob bytes under
//! `<machine home>/cache/objects/<host>/<workspace>/<64-hex sha256>`, keyed by the very object id
//! the plane names — SCOPED per server host + workspace. The scoping mirrors the vault's own
//! no-cross-workspace-dedup ruling: one workspace's downloads never prime (or reveal themselves
//! to) another's fetches, so the cache opens no cross-tenant probing or splicing channel; within
//! a workspace, every scope on the box shares one download.
//!
//! It sits INSIDE the byte transport ([`crate::plane_http::UreqPlane`]) and nowhere else, so no
//! verb, engine, or store knows it exists — a fetch either finds the bytes here or dials for them,
//! and everything downstream sees the same [`crate::plane::FetchedVersion`] either way.
//!
//! **Anchored like every other durable write in this crate.** The scope's directory chain is
//! built and proven with the no-follow walk ([`FsOps::create_dir_nofollow`] below the machine
//! home) — a `cache/` component swapped for a symlink is met as itself and refused, so a download
//! can never create bytes outside the sidecar. A blob then lands via the crate's one no-replace
//! birth ([`crate::atomic::atomic_write_new_at`]): a UNIQUE staging name (pid + a
//! process-monotonic counter — no two writers ever share one), then a kernel-refusing no-replace
//! rename — so a concurrent reader can never observe (and mistakenly evict) an in-flight write;
//! only whole entries ever carry an entry's name. Two racers elect one winner; the loser's
//! staging is dropped.
//!
//! **Bounded.** The cache self-prunes to [`CACHE_SIZE_CAP_BYTES`] (oldest-mtime first),
//! opportunistically after fills — an unbounded machine-wide directory is a disk leak, not a
//! cache. **Fail-open, everywhere.** A cache is an optimization, never an authority: an
//! unreadable entry, a corrupt one, a full disk, a chain that cannot be proven — each degrades to
//! a plain network fetch, and none of them can fail a command. The one thing the cache may never
//! do is hand back bytes that are not the object asked for, so every read RE-VERIFIES
//! `sha256 == the object id` and EVICTS an entry that fails rather than serving it; the transport
//! verifies again on top (`build_fetched_version`'s per-blob gate) — the cache never becomes the
//! last word on the bytes.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use topos_core::digest::{self, to_hex};

use crate::atomic::atomic_write_new_at;
use crate::fs_seam::{DirHandle, FsOps};

/// The cache's self-pruning ceiling (1 GiB). Not configuration — a safety bound; the prune keeps
/// the newest entries, which are the ones a converging fleet re-reads.
pub(crate) const CACHE_SIZE_CAP_BYTES: u64 = 1 << 30;

/// How often (one in N successful writes) the opportunistic prune runs — often enough that the
/// cap holds in practice, rare enough that a bundle fetch never pays a directory walk per blob.
const PRUNE_EVERY: u64 = 32;

/// The per-process counter behind the unique staging names AND the prune cadence.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The machine-wide blob cache — the machine home it anchors under, plus the fs seam every
/// operation rides.
///
/// `!Send` by construction (the `Rc<dyn FsOps>` seam is single-threaded, like every other durable
/// write in this crate): the transport's parallel blob fetch reads and writes it on the
/// COORDINATING thread only, and the worker threads carry nothing but sockets.
pub(crate) struct FetchCache {
    /// The MACHINE home the `cache/objects/<host>/<workspace>` chains are proven below (the
    /// caller resolves it — never a project store, whose bytes would be re-downloaded per
    /// checkout and deleted with it). Chains are created lazily, on first use.
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

/// One cache scope: the server host the bytes came from + the workspace they belong to. Both ride
/// into directory components through [`sanitize_component`], so a hostile spelling can only ever
/// name a DIFFERENT scope, never escape the cache.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheScope<'a> {
    pub host: &'a str,
    pub workspace: &'a str,
}

impl FetchCache {
    /// The cache anchored under `home` (the resolved MACHINE home).
    pub(crate) fn new(home: PathBuf, fs: Rc<dyn FsOps>) -> Self {
        Self { home, fs }
    }

    /// The cache's root: `<home>/cache/objects`.
    fn objects_dir(&self) -> PathBuf {
        self.home.join("cache").join("objects")
    }

    /// One scope's directory: `<home>/cache/objects/<host>/<workspace>`.
    fn scope_dir(&self, scope: CacheScope<'_>) -> PathBuf {
        self.objects_dir()
            .join(sanitize_component(scope.host))
            .join(sanitize_component(scope.workspace))
    }

    /// Prove (and lazily create) a scope's chain below the home, no-follow at every component,
    /// answering the HELD handle the entry ops anchor on. `None` = the chain cannot be proven
    /// (no home yet, a symlinked component, permissions) — the cache is simply not there.
    fn scope_handle(&self, scope: CacheScope<'_>) -> Option<DirHandle> {
        self.fs
            .create_dir_nofollow(&self.home, &self.scope_dir(scope))
            .ok()
    }

    /// The bytes for `object_id` in `scope`, or `None` for every kind of miss there is — absent,
    /// unreadable, or NOT THE OBJECT ASKED FOR. The last case also EVICTS: an entry whose bytes
    /// no longer hash to its own name can never become valid again (the no-replace birth means it
    /// was never a half-written one), and leaving it would cost every future run the same wasted
    /// read. The eviction re-proves the handle first, so it removes inside the proven directory
    /// or not at all.
    pub(crate) fn read(&self, scope: CacheScope<'_>, object_id: &[u8; 32]) -> Option<Vec<u8>> {
        let handle = self.scope_handle(scope)?;
        let path = self.scope_dir(scope).join(to_hex(object_id));
        // The no-follow read, like every other document topos owns: a symlink at the name reads
        // as an error (a miss here), never through to whatever it points at. The chain above it
        // was proven a beat ago by the handle walk.
        let bytes = self.fs.read_opt_nofollow(&path).ok()??;
        if digest::sha256(&bytes) == *object_id {
            Some(bytes)
        } else {
            if handle.verify_unmoved().is_ok() {
                let _ = self.fs.remove_file(&path);
            }
            None
        }
    }

    /// Put `bytes` in the cache under `scope`/`object_id` — BEST-EFFORT: every failure is
    /// swallowed, and a target another writer (or an earlier run) already landed is success,
    /// because the file's content is fixed by its name. The caller has verified
    /// `sha256(bytes) == object_id` before calling; this never writes bytes it was not told are
    /// the object's.
    ///
    /// The birth is [`atomic_write_new_at`] inside the just-proven chain: a UNIQUE staging name
    /// (`<final>.<pid>.<n>.tmp` — two processes, or two blobs of one download, never collide),
    /// then the kernel-level no-replace rename — a reader can never meet a partial entry at the
    /// final name. A failed birth clears its own staging (nothing sweeps this directory). The
    /// crate has no logging facade, so a failure says nothing anywhere: a cache that cannot
    /// write is a cache that is not there.
    pub(crate) fn write(&self, scope: CacheScope<'_>, object_id: &[u8; 32], bytes: &[u8]) {
        let Some(handle) = self.scope_handle(scope) else {
            return;
        };
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let target = self.scope_dir(scope).join(to_hex(object_id));
        let mut tmp_name = target.as_os_str().to_owned();
        tmp_name.push(format!(".{}.{n}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp_name);
        // The chain was proven and is still the object the handle holds; the staged write + the
        // no-replace landing run inside it (the one-beat proof-to-write window is the documented
        // residual of the path-based landing pair).
        if handle.verify_unmoved().is_err() {
            return;
        }
        if atomic_write_new_at(&*self.fs, &target, &tmp, bytes).is_err() {
            let _ = self.fs.remove_file(&tmp);
        } else if n.is_multiple_of(PRUNE_EVERY) {
            self.prune(CACHE_SIZE_CAP_BYTES);
        }
    }

    /// Prune the WHOLE cache (every scope) down to `cap` bytes, oldest mtime first — the
    /// opportunistic bound behind [`CACHE_SIZE_CAP_BYTES`]. Best-effort throughout: a walk or
    /// removal error simply ends the pass (the next fill retries). Staging litter (`*.tmp` from
    /// a crashed run) is swept by age here too.
    pub(crate) fn prune(&self, cap: u64) {
        let root = self.objects_dir();
        // Prove the chain before the path-based walk + removals below.
        let Ok(handle) = self.fs.create_dir_nofollow(&self.home, &root) else {
            return;
        };
        if handle.verify_unmoved().is_err() {
            return;
        }
        let mut entries: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                return;
            };
            for entry in read.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(entry.path());
                } else if meta.is_file() {
                    let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    entries.push((entry.path(), mtime, meta.len()));
                }
            }
        }
        let mut total: u64 = entries.iter().map(|(_, _, len)| len).sum();
        if total <= cap {
            return;
        }
        entries.sort_by_key(|(_, mtime, _)| *mtime);
        for (path, _, len) in entries {
            if total <= cap {
                break;
            }
            if self.fs.remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// A path-safe directory component from an untrusted label: the id charset (`[A-Za-z0-9._-]`)
/// passes through, everything else becomes `_`, a leading dot is folded (dot-prefixed names are
/// reserved), and the length is capped. Collisions between two hostile spellings merge their
/// SCOPES only — content stays verified per object, so nothing can be spliced across one.
fn sanitize_component(label: &str) -> String {
    let mut out: String = label
        .chars()
        .take(128)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::{FaultFs, RealFs};
    use std::path::Path;

    /// A throwaway HOME under the OS temp dir (no `tempfile` dep in this crate).
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU32;
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

    const SCOPE: CacheScope<'static> = CacheScope {
        host: "plane.example",
        workspace: "w_test",
    };

    fn entry_path(home: &Path, scope: CacheScope<'_>, id: &[u8; 32]) -> PathBuf {
        home.join("cache")
            .join("objects")
            .join(scope.host)
            .join(scope.workspace)
            .join(to_hex(id))
    }

    /// The whole contract in one pass: a miss before anything is written, a hit on the exact
    /// bytes after, and a re-write of an object already held is success (not an overwrite).
    #[test]
    fn a_written_object_reads_back_and_a_rewrite_is_a_no_op() {
        let home = scratch("hit");
        let cache = cache_at(&home);
        let bytes = b"# a skill\n".to_vec();
        let id = digest::sha256(&bytes);
        assert!(cache.read(SCOPE, &id).is_none(), "nothing is cached yet");
        cache.write(SCOPE, &id, &bytes);
        assert_eq!(cache.read(SCOPE, &id).unwrap(), bytes);
        cache.write(SCOPE, &id, &bytes);
        assert_eq!(
            cache.read(SCOPE, &id).unwrap(),
            bytes,
            "still the same bytes"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The scope IS an isolation boundary: the same object cached under one workspace is a MISS
    /// under another (and under another host) — the no-cross-workspace-dedup ruling, held at the
    /// cache too, so one tenant's downloads never prime or reveal themselves to another's.
    #[test]
    fn scopes_are_isolated_per_host_and_workspace() {
        let home = scratch("scoped");
        let cache = cache_at(&home);
        let bytes = b"scoped bytes\n".to_vec();
        let id = digest::sha256(&bytes);
        cache.write(SCOPE, &id, &bytes);
        assert_eq!(cache.read(SCOPE, &id).unwrap(), bytes);
        let other_ws = CacheScope {
            host: SCOPE.host,
            workspace: "w_other",
        };
        let other_host = CacheScope {
            host: "other.example",
            workspace: SCOPE.workspace,
        };
        assert!(cache.read(other_ws, &id).is_none());
        assert!(cache.read(other_host, &id).is_none());
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
        cache.write(SCOPE, &id, &bytes);
        let path = entry_path(&home, SCOPE, &id);
        std::fs::write(&path, b"tampered\n").unwrap();
        assert!(
            cache.read(SCOPE, &id).is_none(),
            "corrupt bytes are never served"
        );
        assert!(!path.exists(), "and the entry is evicted");
        // The next write repairs it.
        cache.write(SCOPE, &id, &bytes);
        assert_eq!(cache.read(SCOPE, &id).unwrap(), bytes);
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
        std::fs::create_dir_all(entry_path(&home, SCOPE, &id).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, entry_path(&home, SCOPE, &id)).unwrap();
        assert!(cache.read(SCOPE, &id).is_none());
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
        cache.write(SCOPE, &id, &bytes);
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing may land through the symlinked component"
        );
        assert!(
            cache.read(SCOPE, &id).is_none(),
            "and a read is a plain miss"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Two processes downloading the SAME object land one valid file between them: each stages at
    /// its own unique name, the no-replace birth elects one winner, and no staging litter is left
    /// behind. (Threads stand in for processes; each builds its own cache, because the seam is
    /// single-threaded by design.)
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
                    FetchCache::new(home, Rc::new(RealFs)).write(SCOPE, &id, &bytes);
                });
            }
        });
        let dir = entry_path(&home, SCOPE, &id);
        let dir = dir.parent().unwrap();
        let entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "one file, no staging litter: {entries:?}");
        assert_eq!(std::fs::read(&entries[0]).unwrap(), bytes);
        assert_eq!(cache_at(&home).read(SCOPE, &id).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The seam's fault points (modelled by `FaultFs`: each op fails INSTEAD of running) never
    /// leave an entry at the FINAL name unless it is whole — the staged-then-no-replace birth
    /// means a torn write can only ever be a `.tmp`, which no read consults and the prune sweeps.
    #[test]
    fn a_faulted_write_leaves_no_final_entry_or_a_whole_one() {
        let bytes = b"a bundle blob that is written in one go\n".to_vec();
        let id = digest::sha256(&bytes);
        for fail_at in 1..=8 {
            let home = scratch(&format!("fault{fail_at}"));
            FetchCache::new(home.clone(), Rc::new(FaultFs::new(fail_at))).write(SCOPE, &id, &bytes);
            let entry = entry_path(&home, SCOPE, &id);
            if entry.exists() {
                assert_eq!(
                    std::fs::read(&entry).unwrap(),
                    bytes,
                    "fail_at={fail_at}: the final entry is torn"
                );
            }
            // Whatever the fault did, the cache still answers honestly.
            let cache = cache_at(&home);
            if let Some(got) = cache.read(SCOPE, &id) {
                assert_eq!(got, bytes, "fail_at={fail_at}: served the wrong bytes");
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// The size bound: pruning removes OLDEST entries first until the cap holds, across scopes.
    #[test]
    fn prune_evicts_oldest_first_down_to_the_cap() {
        let home = scratch("cap");
        let cache = cache_at(&home);
        let mut ids = Vec::new();
        for i in 0u8..4 {
            let bytes = vec![i; 100];
            let id = digest::sha256(&bytes);
            cache.write(SCOPE, &id, &bytes);
            // Distinct mtimes, oldest first (coarse clocks round to the second on some filesystems).
            let path = entry_path(&home, SCOPE, &id);
            let t = filetime_from_secs(1_000_000 + u64::from(i) * 10);
            set_mtime(&path, t);
            ids.push(id);
        }
        cache.prune(250);
        assert!(cache.read(SCOPE, &ids[0]).is_none(), "oldest evicted");
        assert!(cache.read(SCOPE, &ids[1]).is_none(), "next-oldest evicted");
        assert!(cache.read(SCOPE, &ids[2]).is_some(), "newer survives");
        assert!(cache.read(SCOPE, &ids[3]).is_some(), "newest survives");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn filetime_from_secs(secs: u64) -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    /// Set a file's mtime via std (no extra dep): `File::set_times` (Rust 1.75+).
    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
    }
}
