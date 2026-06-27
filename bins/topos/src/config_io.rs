//! The bridge that lets the content-blind `topos-harness` adapters read + atomically replace a harness
//! **config** file (e.g. `~/.claude/settings.json`) through the CLI's one fault-injectable syscall
//! seam. The adapter owns the strict-JSON merge; this module owns the durable write — reusing the ONE
//! [`atomic_write_at`] dance, plus the extra care a *shared user* file (outside `~/.topos/`) needs that
//! the home-owned writer never did: ensure the parent dir, write **through** a symlink, use a
//! topos-namespaced temp name (never the user's `<file>.tmp`), and best-effort preserve the file's mode.

use std::io;
use std::path::{Path, PathBuf};

use topos_harness::ConfigStore;

use crate::atomic::atomic_write_at;
use crate::fs_seam::FsOps;

/// Atomically replace a harness config file's contents, crash-safely, with the foreign-file care a
/// shared user file needs. Routes the durable bytes through the one [`atomic_write_at`] dance over
/// `fs`, so the crash gate (`FaultFs`) still faults the underlying syscalls.
///
/// # Errors
/// An underlying [`FsOps`] failure (parent-create or the atomic write).
fn replace_config(fs: &dyn FsOps, target: &Path, bytes: &[u8]) -> io::Result<()> {
    // A first-ever write may need the config dir created — but only when absent, so the common case
    // adds no extra fault-tick (keeping the crash sweep's op count stable).
    if let Some(parent) = target.parent()
        && !fs.exists(parent)
    {
        fs.create_dir_all(parent)?;
    }
    // Write THROUGH a symlink to its real target so we never replace the user's link with a plain file
    // (and so an atomic rename can never land inside a skill dir the link might point at).
    let real = resolve_symlink(target);
    // Capture the existing mode so an overwrite preserves it (a fresh file keeps the umask default).
    let original_mode = std::fs::metadata(&real).ok().map(|m| m.permissions());
    // A topos-namespaced temp beside the real target (same filesystem for the rename); the fixed,
    // hidden name never collides with an unrelated user temp file and self-heals on a retry (a stale
    // temp from an earlier crash is simply overwritten).
    let tmp = topos_temp(&real);
    atomic_write_at(fs, &real, &tmp, bytes)?;
    // Best-effort: the durable write already succeeded; restoring the prior mode is cosmetic, so a miss
    // (or a fresh file with no prior mode) is intentionally non-fatal.
    if let Some(perms) = original_mode {
        let _ = std::fs::set_permissions(&real, perms);
    }
    Ok(())
}

/// Resolve `target` to its symlink destination (fully canonicalized) when it is a symlink; otherwise
/// return it unchanged. A dangling/unresolvable link falls back to the link path itself.
fn resolve_symlink(target: &Path) -> PathBuf {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf())
        }
        _ => target.to_path_buf(),
    }
}

/// The topos-owned temp path beside `target`: a hidden, namespaced, **per-write-unique** sibling
/// (`.<name>.topos.<pid>.<seq>.tmp`). Uniqueness (process id + a monotonic counter) is what keeps two
/// concurrent `topos` writers from sharing one temp and tearing the user's config — each writes its own
/// temp fully, then atomically renames onto `target`, so `target` is always one whole post-image (both
/// writers compute the same bytes). Trade-off: a temp orphaned by a crash mid-write is not self-healed by
/// a later differently-named write, but it is hidden, ~bundle-sized, and only appears on a crash — and
/// can't be swept blindly here without racing a concurrent writer's in-flight temp.
fn topos_temp(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_owned());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.topos.{}.{seq}.tmp", std::process::id()))
}

impl ConfigStore for crate::fs_seam::RealFs {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.read_opt(path)
    }
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        replace_config(self, path, bytes)
    }
}

#[cfg(test)]
impl ConfigStore for crate::fs_seam::FaultFs {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.read_opt(path)
    }
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        replace_config(self, path, bytes)
    }
}
