//! The one crash-safe write primitive + the fail-closed schema-migration dispatch.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// The suffix recovery sweeps for a temp file a faulted [`atomic_write`] may have left pre-rename.
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// The one crash-safe write sequence: write `bytes` to `tmp` → fsync the file → atomic rename onto
/// `target` → fsync the dir. After any fault `target` is byte-for-byte the **pre** or **post** state —
/// never torn. The temp path is the caller's, so a foreign-file writer can pick a unique, namespaced
/// name (never the user's `<file>.tmp`); `target`'s parent must already exist. This is the single dance
/// — [`atomic_write`] and the harness config writer both go through it, so it cannot drift.
///
/// # Errors
/// Propagates the underlying [`FsOps`] failure (which the crash gate injects).
pub(crate) fn atomic_write_at(
    fs: &dyn FsOps,
    target: &Path,
    tmp: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    fs.write_temp(tmp, bytes)?;
    fs.fsync_file(tmp)?;
    fs.rename(tmp, target)?;
    if let Some(dir) = target.parent() {
        fs.fsync_dir(dir)?;
    }
    Ok(())
}

/// Crash-safely write `bytes` to `target` under `~/.topos/` (temp = `target` + [`TMP_SUFFIX`], same
/// dir). The caller serializes + validates `bytes` in memory first, so a malformed value never reaches
/// disk.
///
/// # Errors
/// Propagates the underlying [`FsOps`] failure (which the crash gate injects).
pub(crate) fn atomic_write(fs: &dyn FsOps, target: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    Ok(atomic_write_at(fs, target, &temp_path(target), bytes)?)
}

/// The crash-safe write for a **SECRET**: the same temp → fsync → rename → fsync-dir dance as
/// [`atomic_write_at`], but the temp is created **0600 from creation** ([`FsOps::write_private`]), so the
/// secret never has a world-readable window at any instant — not even mid-write or post-fault (the temp,
/// if a fault leaves one, is itself 0600). The rename carries that 0600 mode onto `target`. Temp =
/// `target` + [`TMP_SUFFIX`], same directory (a same-filesystem rename). Used for the device seed,
/// `follows.json`, and the enrollment WAL; ordinary, non-secret docs use [`atomic_write`].
///
/// # Errors
/// Propagates the underlying [`FsOps`] failure (which the crash gate injects).
pub(crate) fn atomic_write_private(
    fs: &dyn FsOps,
    target: &Path,
    bytes: &[u8],
) -> Result<(), ClientError> {
    let tmp = temp_path(target);
    fs.write_private(&tmp, bytes)?;
    fs.fsync_file(&tmp)?;
    fs.rename(&tmp, target)?;
    if let Some(dir) = target.parent() {
        fs.fsync_dir(dir)?;
    }
    Ok(())
}

/// Crash-safe replace of an EXECUTABLE file: stage a sibling temp (mode 0755, forced past umask),
/// fsync it, atomically rename it over `target`, then fsync the directory. On Unix a running process
/// keeps its old inode, so replacing the live binary is safe. `tmp` MUST be a sibling of `target`
/// (same filesystem) so the rename is atomic — the caller picks a unique, namespaced temp name.
///
/// # Errors
/// Propagates the underlying [`FsOps`] failure (which the crash gate injects).
pub(crate) fn atomic_write_executable(
    fs: &dyn FsOps,
    target: &Path,
    tmp: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    fs.write_staged(tmp, bytes, true)?; // 0755 from creation
    fs.fsync_file(tmp)?;
    fs.rename(tmp, target)?;
    if let Some(dir) = target.parent() {
        fs.fsync_dir(dir)?;
    }
    Ok(())
}

/// The temp path for a target: the same path with [`TMP_SUFFIX`] appended (same directory, so the rename
/// is same-filesystem; a recognizable suffix recovery can sweep).
pub(crate) fn temp_path(target: &Path) -> PathBuf {
    let mut s: OsString = target.as_os_str().to_owned();
    s.push(TMP_SUFFIX);
    PathBuf::from(s)
}

/// Deserialize a persisted document **fail-closed** on its `schema_version`.
///
/// The `schema_version` is probed FIRST; a value newer than `max` is **never** handed to serde (a newer
/// client wrote it — the caller must report "upgrade required", never silently parse or delete it). A
/// missing/non-integer `schema_version`, or a value below the floor, is rejected too. Real `vN-1 → vN`
/// migrations slot into the `1..=max` arm when a v2 schema exists.
///
/// # Errors
/// [`ClientError::UnknownSchemaVersion`] for a newer doc; [`ClientError::UnsupportedLegacy`] below the
/// floor; [`ClientError::Corrupt`] if the probe or the full parse fails.
pub(crate) fn load_versioned<T: DeserializeOwned>(
    bytes: &[u8],
    max: u32,
) -> Result<T, ClientError> {
    #[derive(Deserialize)]
    struct Probe {
        schema_version: u32,
    }
    let probe: Probe = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::Corrupt(format!("missing/invalid schema_version: {e}")))?;
    match probe.schema_version {
        0 => Err(ClientError::UnsupportedLegacy { found: 0 }),
        v if v <= max => serde_json::from_slice(bytes)
            .map_err(|e| ClientError::Corrupt(format!("document parse: {e}"))),
        v => Err(ClientError::UnknownSchemaVersion { found: v, max }),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::fs_seam::{FaultFs, RealFs};

    /// A throwaway directory under the OS temp dir (no `tempfile` dep in this crate).
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-aw-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_private_creates_a_0600_file() {
        let fs = RealFs;
        let p = scratch("wp").join("secret");
        fs.write_private(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn write_private_tightens_a_preexisting_0644_file() {
        // A file already on disk at 0644 (e.g. an earlier non-secret write) is forced to 0600 — the
        // `set_permissions` defeats `mode()` being ignored for an existing file.
        let fs = RealFs;
        let p = scratch("wp644").join("secret");
        fs.write_staged(&p, b"old", false).unwrap();
        assert_eq!(mode_of(&p), 0o644);
        fs.write_private(&p, b"new").unwrap();
        assert_eq!(mode_of(&p), 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
    }

    #[test]
    fn private_perms_ok_is_false_for_a_group_or_other_readable_file() {
        let fs = RealFs;
        let p = scratch("pp").join("f");
        fs.write_staged(&p, b"x", false).unwrap(); // 0644
        assert!(!fs.private_perms_ok(&p).unwrap());
        fs.write_private(&p, b"x").unwrap(); // 0600
        assert!(fs.private_perms_ok(&p).unwrap());
    }

    #[test]
    fn atomic_write_private_round_trips_at_0600() {
        let fs = RealFs;
        let p = scratch("awp").join("doc.json");
        atomic_write_private(&fs, &p, b"{\"k\":1}").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"k\":1}");
        assert_eq!(mode_of(&p), 0o600);
    }

    /// A fault at ANY step of `atomic_write_executable` (the self-updater's binary replace) leaves the
    /// target byte-for-byte pre OR post — never a torn/partial binary — and always keeps its 0755 mode. On
    /// Unix a running process holds its old inode, so a crash mid-upgrade never wedges the live binary.
    #[test]
    fn faultfs_mid_write_executable_never_leaves_a_torn_binary() {
        for fail_at in 1..=4 {
            let dir = scratch(&format!("awxf{fail_at}"));
            let p = dir.join("topos");
            let tmp = dir.join(".topos-upgrade.tmp");
            let real = RealFs;
            real.write_staged(&p, b"OLD", true).unwrap(); // pre-state, 0755
            let ff = FaultFs::new(fail_at);
            let _ = atomic_write_executable(&ff, &p, &tmp, b"NEWBINARY"); // faults at step `fail_at`

            // The target is exactly the pre or the post bytes (never a mix) and keeps its executable mode.
            let now = std::fs::read(&p).unwrap();
            assert!(
                now == b"OLD" || now == b"NEWBINARY",
                "fail_at={fail_at}: target neither pre nor post (torn)"
            );
            assert_eq!(
                mode_of(&p),
                0o755,
                "fail_at={fail_at}: target lost its 0755 mode"
            );
        }
    }

    /// A fault at ANY step of `atomic_write_private` leaves the target byte-for-byte pre OR post (never
    /// torn) and — the secret invariant — NEVER a world-readable partial: both the target and any leftover
    /// temp are 0600, because the temp is private from creation.
    #[test]
    fn faultfs_mid_write_private_never_leaves_a_world_readable_partial() {
        for fail_at in 1..=4 {
            let p = scratch(&format!("awpf{fail_at}")).join("doc.json");
            let real = RealFs;
            atomic_write_private(&real, &p, b"OLD").unwrap(); // pre-state, 0600
            let ff = FaultFs::new(fail_at);
            let _ = atomic_write_private(&ff, &p, b"NEW"); // faults at step `fail_at`

            // The target is 0600 and is exactly the pre or the post bytes.
            assert_eq!(
                mode_of(&p),
                0o600,
                "fail_at={fail_at}: target lost its 0600 mode"
            );
            let now = std::fs::read(&p).unwrap();
            assert!(
                now == b"OLD" || now == b"NEW",
                "fail_at={fail_at}: target neither pre nor post (torn)"
            );
            // Any leftover temp is also 0600 — never a 0644 partial a wider audience could read.
            let tmp = temp_path(&p);
            if real.exists(&tmp) {
                assert_eq!(
                    mode_of(&tmp),
                    0o600,
                    "fail_at={fail_at}: secret temp was not 0600"
                );
            }
        }
    }
}
