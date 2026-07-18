//! The I/O bundle scanner — a distinct artifact from the kernel's pure path predicate. It walks a real
//! skill directory and applies the **filesystem-level** reject rules to on-disk reality (symlink /
//! device / fifo / socket / any non-regular file / non-UTF-8 name) that the `no_std` kernel can never
//! see, then feeds `(path, mode, sha256)` to the kernel digest — which re-applies the byte-pure path
//! rejects (absolute / `..` / NUL / control) and the NFC/case-fold **collision** rejects.
//!
//! Two entry points share ONE directory walk ([`walk_files`]), so they can never disagree on which
//! files exist, in what order, or under what rejects:
//! - [`scan`] reads every file's bytes (the full bundle — for adoption, publish, diff, and any path
//!   that ships or stores bytes);
//! - [`drift_digest`] computes only the `bundle_digest`, reading + hashing a file ONLY on a stat-cache
//!   miss (the routine drift scan — O(stat) when nothing changed). It never yields bytes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use topos_core::digest::{self, FileMode, ManifestEntry};

use crate::error::ClientError;
use crate::stat_cache::{FileStat, StatKey};

/// One scanned file: its bundle-relative forward-slash path, mode, and raw bytes.
#[derive(Debug, Clone)]
pub(crate) struct ScannedFile {
    pub path: String,
    pub mode: FileMode,
    pub bytes: Vec<u8>,
}

/// A scanned bundle: the files (sorted by raw path bytes), the kernel `bundle_digest`, and the optional
/// name parsed from `SKILL.md` frontmatter.
#[derive(Debug, Clone)]
pub(crate) struct ScannedBundle {
    pub files: Vec<ScannedFile>,
    pub bundle_digest: [u8; 32],
    pub name_hint: Option<String>,
}

/// The result of a cache-backed [`drift_digest`]: the bundle digest and the CURRENT per-file rows (to
/// write back into the cache bucket — deleted files pruned, added files included).
#[derive(Debug, Clone)]
pub(crate) struct DriftScan {
    pub bundle_digest: [u8; 32],
    pub files: BTreeMap<String, FileStat>,
}

/// One walked file: the shared output of [`walk_files`] — its path, mode, `lstat` metadata (a regular
/// file, so `lstat == stat`; reused as the stat-cache key with no extra syscall), and absolute path.
struct WalkedFile {
    path: String,
    mode: FileMode,
    meta: std::fs::Metadata,
    abs: PathBuf,
}

/// Scan a real skill directory into the full byte bundle.
///
/// # Errors
/// [`ClientError::Scan`] on a filesystem-level reject (symlink/device/non-regular/non-UTF-8) or a kernel
/// path/collision reject; [`ClientError::EmptyBundle`] if nothing adoptable remains; [`ClientError::Io`]
/// on a read failure.
pub(crate) fn scan(root: &Path) -> Result<ScannedBundle, ClientError> {
    let walked = walk_files(root)?;
    if walked.is_empty() {
        return Err(ClientError::EmptyBundle);
    }

    let mut files = Vec::with_capacity(walked.len());
    for w in &walked {
        let bytes = std::fs::read(&w.abs)
            .map_err(|e| ClientError::Io(format!("read {}: {e}", w.abs.display())))?;
        files.push(ScannedFile {
            path: w.path.clone(),
            mode: w.mode,
            bytes,
        });
    }

    // The kernel re-runs check_path + the collision rules and computes the consent digest.
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let bundle_digest = digest::bundle_digest(&entries)?;

    let name_hint = files
        .iter()
        .find(|f| f.path == "SKILL.md")
        .and_then(|f| frontmatter_name(&f.bytes));

    Ok(ScannedBundle {
        files,
        bundle_digest,
        name_hint,
    })
}

/// Compute a skill directory's `bundle_digest` using the stat cache — reading + hashing a file ONLY on
/// a cache miss (a stat tuple absent from `prev` or not matching it). A full hit is O(stat): no byte is
/// read. `prev` is the placement's usable prior rows (the caller passes `None` when a swap invalidated
/// the bucket). Yields the SAME digest as [`scan`] over the same tree — proven by the equivalence
/// tests and the cache kill switch.
///
/// `racy_ref` is the [racy-clean reference](crate::stat_cache::last_written_ns) — the time the cache
/// was last persisted. A cached row is trusted ONLY when the file's own newest timestamp is STRICTLY
/// older than it; a file that could have been rewritten in the same filesystem tick the cache was
/// written (or later) is re-hashed. `None` (no cache file yet) distrusts every row. This closes the
/// coarse-resolution, same-tick, forged-mtime, unchanged-size window that the `(mtime, ctime, size)`
/// tuple alone cannot. (Even were a stale row ever trusted, the materializer full-scans a placement
/// before overwriting it and snapshots any uncaptured edit — so a missed draft is never a lost byte.)
///
/// # Errors
/// As [`scan`] (the same walk + rejects), plus [`ClientError::Io`] on a miss's read failure.
pub(crate) fn drift_digest(
    root: &Path,
    prev: Option<&BTreeMap<String, FileStat>>,
    racy_ref: Option<i64>,
) -> Result<DriftScan, ClientError> {
    let walked = walk_files(root)?;
    if walked.is_empty() {
        return Err(ClientError::EmptyBundle);
    }

    let mut entries = Vec::with_capacity(walked.len());
    let mut rows = BTreeMap::new();
    for w in &walked {
        let key = StatKey::from_metadata(&w.meta);
        // A hit needs (a) the file to be RACILY CLEAN — its newest timestamp strictly older than the
        // last cache write, so it cannot have changed in the same tick — AND (b) the row's stat tuple
        // to match AND (c) its stored hex to parse back to 32 bytes; any failure degrades to a miss
        // (re-hash), never trusting a possibly-stale row.
        let racily_clean = racy_ref.is_some_and(|r| key.mtime_ns.max(key.ctime_ns) < r);
        let content_sha256 = match prev
            .filter(|_| racily_clean)
            .and_then(|p| p.get(&w.path))
            .filter(|row| row.key() == key)
            .and_then(|row| hex32(&row.sha256))
        {
            Some(sha) => sha,
            None => {
                let bytes = std::fs::read(&w.abs)
                    .map_err(|e| ClientError::Io(format!("read {}: {e}", w.abs.display())))?;
                digest::sha256(&bytes)
            }
        };
        entries.push(ManifestEntry {
            path: w.path.clone(),
            mode: w.mode,
            content_sha256,
        });
        rows.insert(
            w.path.clone(),
            FileStat {
                mtime_ns: key.mtime_ns,
                ctime_ns: key.ctime_ns,
                size: key.size,
                sha256: digest::to_hex(&content_sha256),
            },
        );
    }
    let bundle_digest = digest::bundle_digest(&entries)?;
    Ok(DriftScan {
        bundle_digest,
        files: rows,
    })
}

/// Walk a skill directory into its files, sorted by raw path bytes (the manifest order both entry
/// points feed the kernel). Applies every filesystem-level reject and drops `.git` + `.DS_Store`. Does
/// NOT read file bytes — only `lstat`s each entry.
fn walk_files(root: &Path) -> Result<Vec<WalkedFile>, ClientError> {
    let mut out = Vec::new();
    walk(root, "", &mut out)?;
    out.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<WalkedFile>) -> Result<(), ClientError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| ClientError::Io(format!("read_dir {}: {e}", dir.display())))?
        .collect::<Result<_, _>>()
        .map_err(|e| ClientError::Io(format!("{e}")))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ClientError::Scan(format!("non-UTF-8 name under {}", dir.display())))?;

        // Never follow a symlink — inspect the link itself.
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| ClientError::Io(format!("stat {}: {e}", path.display())))?;
        let ft = meta.file_type();

        if ft.is_symlink() {
            return Err(ClientError::Scan(format!("symlink: {name}")));
        }
        if ft.is_dir() {
            // Drop the VCS dir; recurse everything else.
            if name == ".git" {
                continue;
            }
            let child_prefix = join(prefix, &name);
            walk(&path, &child_prefix, out)?;
            continue;
        }
        if !ft.is_file() {
            // device / fifo / socket / anything else non-regular.
            return Err(ClientError::Scan(format!("not a regular file: {name}")));
        }
        // A regular file. Drop the macOS dropping; keep the rest byte-exact.
        if name == ".DS_Store" {
            continue;
        }
        let mode = file_mode(&meta);
        out.push(WalkedFile {
            path: join(prefix, &name),
            mode,
            meta,
            abs: path,
        });
    }
    Ok(())
}

/// Decode a 64-char lowercase hex sha into 32 bytes, or `None` if malformed.
fn hex32(s: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

fn file_mode(meta: &std::fs::Metadata) -> FileMode {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        FileMode::Executable
    } else {
        FileMode::Regular
    }
}

/// Extract a `name:` value from leading YAML frontmatter (`---` … `---`). A minimal line scan — no YAML
/// dependency; an unquoted or single/double-quoted scalar is accepted. Returns `None` if absent.
fn frontmatter_name(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            let v = rest.trim().trim_matches(['"', '\'']).trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-scan-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
    }

    /// The cache-backed digest equals the full-scan digest — cold (no prev) and warm (fed prev rows).
    #[test]
    fn drift_digest_matches_scan_cold_and_warm() {
        let dir = scratch("match");
        write(&dir, "SKILL.md", b"---\nname: demo\n---\nbody\n");
        write(&dir, "scripts/run.sh", b"#!/bin/sh\necho hi\n");

        let full = scan(&dir).unwrap();
        let cold = drift_digest(&dir, None, None).unwrap();
        assert_eq!(cold.bundle_digest, full.bundle_digest);
        // Warm: feed the cold rows back — every file a hit, same digest.
        let warm = drift_digest(&dir, Some(&cold.files), Some(i64::MAX)).unwrap();
        assert_eq!(warm.bundle_digest, full.bundle_digest);
    }

    /// On a stat match of a racily-CLEAN file the cache is TRUSTED without reading: a poisoned (but
    /// valid-hex) sha with an unchanged stat tuple + a settled (far-future) racy reference flows
    /// straight into the digest, proving the bytes were not re-read.
    #[test]
    fn a_matching_stat_trusts_the_cached_sha_without_reading() {
        let dir = scratch("trust");
        write(&dir, "SKILL.md", b"real body\n");
        let cold = drift_digest(&dir, None, None).unwrap();

        let mut poisoned = cold.files.clone();
        poisoned.get_mut("SKILL.md").unwrap().sha256 = "11".repeat(32); // wrong, but valid hex
        let out = drift_digest(&dir, Some(&poisoned), Some(i64::MAX)).unwrap();
        assert_ne!(
            out.bundle_digest,
            scan(&dir).unwrap().bundle_digest,
            "a matching stat must trust the cached sha (no read), so the poison shows through"
        );
    }

    /// The racy-clean guard: a file whose timestamp is NOT strictly older than the racy reference (it
    /// could have changed in the same tick the cache was written) is re-hashed even when its stat tuple
    /// matches a cached row — so a same-tick, forged-mtime, unchanged-size write can never be trusted.
    #[test]
    fn a_racy_file_is_rehashed_even_when_the_tuple_matches() {
        let dir = scratch("racy");
        write(&dir, "SKILL.md", b"the true bytes\n");
        let cold = drift_digest(&dir, None, None).unwrap();

        // Poison the row with a valid-hex but wrong sha; trusting it would corrupt the digest.
        let mut poisoned = cold.files.clone();
        poisoned.get_mut("SKILL.md").unwrap().sha256 = "22".repeat(32);

        // racy_ref in the distant PAST → the file's real ctime is >= it → racy → re-hash → true digest.
        let racy = drift_digest(&dir, Some(&poisoned), Some(1)).unwrap();
        assert_eq!(
            racy.bundle_digest, cold.bundle_digest,
            "a racy file must be re-hashed, not trusted"
        );
        // Contrast: a settled reference (far future) trusts the poisoned row — proving the guard, not a
        // coincidental miss, is what forced the re-hash above.
        let settled = drift_digest(&dir, Some(&poisoned), Some(i64::MAX)).unwrap();
        assert_ne!(settled.bundle_digest, cold.bundle_digest);
    }

    /// A forged mtime with an unchanged size does NOT hide an edit: the write bumps ctime, so the row
    /// key no longer matches → the file is re-hashed → the digest moves. (If a platform's ctime were
    /// too coarse to distinguish the writes, this asserts the residual is at least caught by the read.)
    #[test]
    fn forged_mtime_same_size_is_caught_by_ctime() {
        let dir = scratch("forge");
        // Two 5-byte bodies with the same length but different bytes.
        write(&dir, "SKILL.md", b"aaaaa");
        let before = drift_digest(&dir, None, None).unwrap();

        // Capture the original mtime, rewrite with a DIFFERENT 5-byte body, then forge mtime back.
        let orig = std::fs::metadata(dir.join("SKILL.md"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::write(dir.join("SKILL.md"), b"bbbbb").unwrap();
        let times = std::fs::FileTimes::new().set_modified(orig);
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("SKILL.md"))
            .unwrap()
            .set_times(times)
            .unwrap();

        // Fed the stale rows (matching mtime + size), the scan still detects the change: ctime moved.
        let after = drift_digest(&dir, Some(&before.files), Some(i64::MAX)).unwrap();
        assert_ne!(
            before.bundle_digest, after.bundle_digest,
            "a forged mtime must not hide a byte change"
        );
        assert_eq!(after.bundle_digest, scan(&dir).unwrap().bundle_digest);
    }

    /// Adding and removing files reshapes the rows (deleted pruned, added included) and moves the
    /// digest even when the surviving files' stats are cache hits.
    #[test]
    fn added_and_removed_files_reshape_the_digest() {
        let dir = scratch("reshape");
        write(&dir, "SKILL.md", b"body\n");
        let base = drift_digest(&dir, None, None).unwrap();

        write(&dir, "extra.md", b"more\n");
        let grown = drift_digest(&dir, Some(&base.files), Some(i64::MAX)).unwrap();
        assert_ne!(base.bundle_digest, grown.bundle_digest);
        assert!(grown.files.contains_key("extra.md"));
        assert_eq!(grown.bundle_digest, scan(&dir).unwrap().bundle_digest);

        std::fs::remove_file(dir.join("extra.md")).unwrap();
        let shrunk = drift_digest(&dir, Some(&grown.files), Some(i64::MAX)).unwrap();
        assert_eq!(shrunk.bundle_digest, base.bundle_digest);
        assert!(!shrunk.files.contains_key("extra.md"));
    }

    /// A malformed cached sha is treated as a miss (re-hashed), never trusted.
    #[test]
    fn a_corrupt_cached_sha_degrades_to_a_miss() {
        let dir = scratch("corrupt");
        write(&dir, "SKILL.md", b"body\n");
        let good = drift_digest(&dir, None, None).unwrap();
        let mut poisoned = good.files.clone();
        poisoned.get_mut("SKILL.md").unwrap().sha256 = "zz".repeat(32); // not hex
        let out = drift_digest(&dir, Some(&poisoned), Some(i64::MAX)).unwrap();
        // A malformed row is a miss → re-hashed to the true bytes, never trusted.
        assert_eq!(out.bundle_digest, good.bundle_digest);
        assert_eq!(out.files["SKILL.md"].sha256, good.files["SKILL.md"].sha256);
    }
}
