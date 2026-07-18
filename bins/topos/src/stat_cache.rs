//! `state/stat_cache.json` — a per-placement `(mtime_ns, ctime_ns, size) -> sha256` cache for the
//! per-placement drift scan, so a routine `update` sweep is **O(stat)** rather than **O(re-hash)**.
//!
//! ## What it is (and is not)
//!
//! The cache is **purely advisory**: it accelerates the local drift verdict (does this placement's
//! copy still match its recorded materialized sha, or is it a draft) and *never* gates correctness.
//! The verdict is byte-for-byte identical with the cache disabled — the kill switch
//! `TOPOS_NO_STAT_CACHE=1` proves it in the equivalence tests. A miss, a stale row, an unreadable
//! file, or a wrong/newer `schema_version` is silently ignored and the entry rebuilt; the cache is
//! never fail-closed and is never consulted to compute a digest that SHIPS bytes anywhere (publish /
//! merge / revert always full-`scan::scan` their bytes).
//!
//! ## Correctness — why a stale row can never hide a draft
//!
//! Each file row keys the content sha on the tuple `(mtime_ns, ctime_ns, size)`. Any content write
//! bumps the inode's **ctime** (the change-time; `utimensat` can move mtime but never moves ctime
//! backwards), so a byte change with a *forged* mtime and an unchanged size is still caught by the
//! ctime mismatch → cache miss → re-hash.
//!
//! The one residual the tuple alone cannot cover is a filesystem whose ctime resolution is so coarse
//! that two distinct writes land in the same tick with an identical size and a forged mtime. That is
//! closed by the **racy-clean guard** ([`last_written_ns`], git's "racy index" trick): a row is trusted
//! only when the file's own newest timestamp is STRICTLY older than the cache's last write, so a file
//! that could have changed in the same tick is re-hashed. (A placement on a DIFFERENT, coarser
//! filesystem than `~/.topos` is the remaining edge — and there the materializer's pre-swap full scan
//! snapshots any uncaptured edit before overwriting it, so even a theoretically missed draft is never a
//! lost byte.)
//!
//! ## Swap invalidation
//!
//! An `update` materialization lands new bytes by an atomic directory swap (`RENAME_EXCHANGE`), which
//! replaces the placement's files with brand-new inodes. Rather than write the cache from inside the
//! crash-committed materialize sequence (which would perturb the fault-injection op counts), each
//! bucket records the `basis` — the placement's recorded `materialized_sha` the rows were computed
//! against. Materialize advances that recorded sha on every swap, so the next scan sees
//! `basis != recorded`, drops the whole bucket, bumps its `generation`, and re-hashes from scratch.
//! (A same-version re-swap keeps the sha but still changes inodes → the per-file ctime guard catches
//! it.) The swap hook is therefore the recorded-sha check, not a write in the durability path.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use serde::{Deserialize, Serialize};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The document's own schema ceiling. A newer doc (or any parse failure) is treated as an empty
/// cache and rebuilt — the cache is advisory, never fail-closed like a durable state document.
pub(crate) const STAT_CACHE_SCHEMA_VERSION: u32 = 1;

/// The kill switch: `TOPOS_NO_STAT_CACHE=1` disables the cache entirely (every scan full-re-hashes).
pub(crate) const KILL_SWITCH_ENV: &str = "TOPOS_NO_STAT_CACHE";

/// The whole document: one bucket per placement directory, keyed by its path (a `BTreeMap`, so the
/// on-disk bytes are deterministic).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StatCache {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub placements: BTreeMap<String, PlacementBucket>,
}

/// One placement directory's cached rows, tagged with the materialized sha they were computed
/// against (`basis`) and a monotonic `generation` that bumps whenever the rows are rebuilt — the
/// visible signal a swap invalidation fired.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlacementBucket {
    #[serde(default)]
    pub generation: u64,
    /// The placement's recorded `materialized_sha` (hex) the rows were built against. `None` only for
    /// a legacy/empty bucket. A mismatch with the current recorded sha invalidates the whole bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// Bundle-relative forward-slash path → the file's stat tuple + content sha.
    #[serde(default)]
    pub files: BTreeMap<String, FileStat>,
}

/// One file's cached identity: the stat tuple that keys the sha, plus the sha itself (hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileStat {
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub size: u64,
    pub sha256: String,
}

/// The stat tuple that keys a cached sha — everything about a file *except* its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatKey {
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub size: u64,
}

impl StatKey {
    /// Extract the stat tuple from a file's metadata (the `lstat` the scanner already took for its
    /// hazard check — no extra syscall). Nanoseconds since the epoch fit in `i64` until well past
    /// year 2200, so the multiply never overflows in practice.
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            mtime_ns: meta.mtime().saturating_mul(1_000_000_000) + meta.mtime_nsec(),
            ctime_ns: meta.ctime().saturating_mul(1_000_000_000) + meta.ctime_nsec(),
            size: meta.size(),
        }
    }
}

impl FileStat {
    /// The stat tuple keying this row.
    pub(crate) fn key(&self) -> StatKey {
        StatKey {
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
            size: self.size,
        }
    }
}

impl PlacementBucket {
    /// The prior rows to consult for `recorded`, or `None` when the bucket is for a different
    /// materialized sha (a swap moved the placement) and must be rebuilt from scratch.
    pub(crate) fn usable_rows(&self, recorded: &str) -> Option<&BTreeMap<String, FileStat>> {
        (self.basis.as_deref() == Some(recorded)).then_some(&self.files)
    }
}

/// Whether the kill switch value disables the cache. A pure predicate over the raw env value, so the
/// decision is unit-testable without mutating process-global env.
pub(crate) fn is_disabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// Whether the cache is enabled for this process (the kill switch is unset or not `1`).
pub(crate) fn enabled_from_env() -> bool {
    !is_disabled(std::env::var_os(KILL_SWITCH_ENV).as_deref())
}

/// The **racy-clean reference**: the nanosecond timestamp the cache file was last persisted (its own
/// `mtime`/`ctime`), or `None` when absent. This is git's "index mtime" trick, adapted: a placement
/// file whose newest timestamp is NOT strictly older than this could have been rewritten in the same
/// filesystem tick the cache was recorded, so its cached row is re-hashed rather than trusted. Because
/// the reference is read from the SAME clock the placement's timestamps use, the comparison
/// self-calibrates to the filesystem's resolution — closing the coarse-resolution, same-tick,
/// forged-mtime, unchanged-size window that `(mtime, ctime, size)` alone cannot (ctime cannot be
/// forged backwards, but a low-resolution filesystem can floor two distinct writes into one tick).
/// (Residual: a placement on a DIFFERENT, coarser filesystem than `~/.topos` — the materializer's
/// pre-swap full scan is the data-loss backstop there; see `crate::scan::drift_digest`.)
pub(crate) fn last_written_ns(layout: &Layout) -> Option<i64> {
    let meta = std::fs::metadata(layout.stat_cache_path()).ok()?;
    let key = StatKey::from_metadata(&meta);
    Some(key.mtime_ns.max(key.ctime_ns))
}

/// Load the cache, or an empty default on ANY problem (absent, unreadable, corrupt, or a newer
/// `schema_version`). Advisory by design: a bad cache is never an error, only a cold start.
pub(crate) fn load(fs: &dyn FsOps, layout: &Layout) -> StatCache {
    let Ok(Some(bytes)) = fs.read_opt(&layout.stat_cache_path()) else {
        return StatCache::default();
    };
    match serde_json::from_slice::<StatCache>(&bytes) {
        Ok(cache) if cache.schema_version <= STAT_CACHE_SCHEMA_VERSION => cache,
        // A newer writer's doc, or a corrupt one: start cold rather than trust or delete it.
        _ => StatCache::default(),
    }
}

/// Persist the cache (best-effort, atomic). The caller ignores the result — a failed cache write
/// never fails a sweep; the next scan just re-hashes.
///
/// # Errors
/// Propagates the [`FsOps`] failure (the caller discards it).
pub(crate) fn store(fs: &dyn FsOps, layout: &Layout, cache: &StatCache) -> Result<(), ClientError> {
    fs.create_dir_all(&layout.state_dir())?;
    let mut out = cache.clone();
    out.schema_version = STAT_CACHE_SCHEMA_VERSION;
    doc::write_doc(fs, &layout.stat_cache_path(), &out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-sc-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> StatCache {
        let mut files = BTreeMap::new();
        files.insert(
            "SKILL.md".to_owned(),
            FileStat {
                mtime_ns: 1_700_000_000_000_000_000,
                ctime_ns: 1_700_000_000_500_000_000,
                size: 42,
                sha256: "ab".repeat(32),
            },
        );
        let mut placements = BTreeMap::new();
        placements.insert(
            "/home/u/.agents/skills/demo".to_owned(),
            PlacementBucket {
                generation: 3,
                basis: Some("cd".repeat(32)),
                files,
            },
        );
        StatCache {
            schema_version: STAT_CACHE_SCHEMA_VERSION,
            placements,
        }
    }

    #[test]
    fn store_then_load_round_trips() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("rt"));
        let cache = sample();
        store(&fs, &layout, &cache).unwrap();
        assert_eq!(load(&fs, &layout), cache);
    }

    #[test]
    fn absent_cache_loads_empty_not_erroring() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("absent"));
        assert_eq!(load(&fs, &layout), StatCache::default());
    }

    #[test]
    fn a_newer_or_corrupt_doc_loads_empty_never_fails_closed() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("newer"));
        fs.create_dir_all(&layout.state_dir()).unwrap();
        // A schema from the future — a durable doc would refuse; the advisory cache starts cold.
        doc::write_doc(
            &fs,
            &layout.stat_cache_path(),
            &serde_json::json!({ "schema_version": STAT_CACHE_SCHEMA_VERSION + 9 }),
        )
        .unwrap();
        assert_eq!(load(&fs, &layout), StatCache::default());
        // Outright garbage → still empty, never an error.
        fs.write_temp(&layout.stat_cache_path(), b"not json")
            .unwrap();
        assert_eq!(load(&fs, &layout), StatCache::default());
    }

    #[test]
    fn kill_switch_predicate_only_trips_on_exactly_one() {
        assert!(is_disabled(Some(OsStr::new("1"))));
        assert!(!is_disabled(Some(OsStr::new("0"))));
        assert!(!is_disabled(Some(OsStr::new("true"))));
        assert!(!is_disabled(None));
    }

    #[test]
    fn usable_rows_gates_on_the_basis() {
        let bucket = PlacementBucket {
            generation: 1,
            basis: Some("ff".repeat(32)),
            files: BTreeMap::new(),
        };
        assert!(bucket.usable_rows(&"ff".repeat(32)).is_some());
        assert!(bucket.usable_rows(&"00".repeat(32)).is_none());
    }
}
