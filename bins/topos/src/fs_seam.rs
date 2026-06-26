//! The private fault-injectable filesystem/syscall seam — the one place every durable mutation goes
//! through, so the crash gate can fail the Nth op and assert recovery.
//!
//! `RealFs` is `std::fs` + `rustix` (safe wrappers — no `unsafe`, honoring the workspace
//! `unsafe_code = "forbid"`): `F_FULLFSYNC` on macOS for true durability, plain `fsync` elsewhere, and
//! `flock` for the per-skill writer lock. `FaultFs` (test-only) wraps `RealFs` with a shared op counter
//! and fails the chosen op **without** performing it — modelling a crash just before that syscall, with a
//! genuine real-syscall prefix so post-fault on-disk state is authentic for recovery.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// A held exclusive lock. Dropping it releases the `flock` (tied to the open file description).
#[derive(Debug)]
pub(crate) struct LockGuard {
    _file: File,
}

/// The durable-mutation seam. Read-only inspection of the user's *source* dir is **not** here — that is
/// the scanner's `std::fs` walk; this seam covers only what must survive a crash under `~/.topos/`.
pub(crate) trait FsOps {
    /// Create/truncate a temp file and write `bytes` — **no** fsync (the next op is the fsync).
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Flush a file's contents to stable storage (`F_FULLFSYNC` on macOS).
    fn fsync_file(&self, path: &Path) -> io::Result<()>;
    /// Atomically replace `to` with `from` (POSIX rename — all-or-nothing).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Flush a directory's entries to stable storage.
    fn fsync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Rename a directory to a target that must **not** already exist (no-replace publish).
    fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// `mkdir -p`.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Append a line (newline-terminated by the caller) and fsync — for `log.jsonl`.
    fn append_fsync(&self, path: &Path, line: &[u8]) -> io::Result<()>;
    /// Remove a single file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Remove a directory tree.
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Read a file, or `None` if it does not exist.
    fn read_opt(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
    /// The immediate entries of a directory (full paths), or empty if it does not exist.
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    /// Whether a path exists (following symlinks).
    fn exists(&self, path: &Path) -> bool;
    /// Acquire an exclusive lock on `path` (creating it), blocking until held.
    fn lock_exclusive(&self, path: &Path) -> io::Result<LockGuard>;
    /// Try to acquire an exclusive lock without blocking; `None` if another holder has it.
    fn try_lock_exclusive(&self, path: &Path) -> io::Result<Option<LockGuard>>;
}

/// The production seam: `std::fs` + `rustix` safe syscalls.
#[derive(Debug, Default)]
pub(crate) struct RealFs;

impl RealFs {
    fn fsync_handle(file: &File) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            // F_FULLFSYNC: the only call that actually flushes the drive cache on macOS.
            rustix::fs::fcntl_fullfsync(file).map_err(io::Error::from)
        }
        #[cfg(not(target_os = "macos"))]
        {
            rustix::fs::fsync(file).map_err(io::Error::from)
        }
    }
}

impl FsOps for RealFs {
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.write_all(bytes)?;
        Ok(())
    }

    fn fsync_file(&self, path: &Path) -> io::Result<()> {
        let f = File::open(path)?;
        Self::fsync_handle(&f)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
        // A directory fd flushed with plain fsync persists its entries; F_FULLFSYNC is for file data.
        let f = File::open(dir)?;
        rustix::fs::fsync(&f).map_err(io::Error::from)
    }

    fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        // No-replace: refuse if the target exists (a typed collision, never an overwrite). The per-skill
        // lock the caller holds closes the check→rename window for topos's own writers.
        if to.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "target exists",
            ));
        }
        std::fs::rename(from, to)
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn append_fsync(&self, path: &Path, line: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(line)?;
        Self::fsync_handle(&f)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    fn read_opt(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                let mut out = Vec::new();
                for e in rd {
                    out.push(e?.path());
                }
                Ok(out)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn lock_exclusive(&self, path: &Path) -> io::Result<LockGuard> {
        let file = open_lock_file(path)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(io::Error::from)?;
        Ok(LockGuard { _file: file })
    }

    fn try_lock_exclusive(&self, path: &Path) -> io::Result<Option<LockGuard>> {
        let file = open_lock_file(path)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(LockGuard { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(io::Error::from(e)),
        }
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false) // a lock file's content is irrelevant — never wipe it
        .open(path)
}

#[cfg(test)]
pub(crate) use fault::FaultFs;

#[cfg(test)]
mod fault {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Wraps `RealFs` and fails the chosen **mutating** op (1-based) without performing it. Reads + lock
    /// ops never fault and never advance the counter, so the count tracks exactly the durable mutations a
    /// sequence performs — the crash table drives `fail_at` across them.
    #[derive(Debug)]
    pub(crate) struct FaultFs {
        inner: RealFs,
        counter: AtomicUsize,
        fail_at: usize,
    }

    impl FaultFs {
        /// `fail_at == 0` never faults (a real run used to compute the post-state).
        pub(crate) fn new(fail_at: usize) -> Self {
            Self {
                inner: RealFs,
                counter: AtomicUsize::new(0),
                fail_at,
            }
        }

        /// How many mutating ops were attempted (so a test can size its fault sweep).
        pub(crate) fn ops_attempted(&self) -> usize {
            self.counter.load(Ordering::Relaxed)
        }

        fn tick(&self) -> io::Result<()> {
            let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            if self.fail_at != 0 && n == self.fail_at {
                Err(io::Error::other("injected fault"))
            } else {
                Ok(())
            }
        }
    }

    impl FsOps for FaultFs {
        fn write_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.tick()?;
            self.inner.write_temp(path, bytes)
        }
        fn fsync_file(&self, path: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.fsync_file(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.rename(from, to)
        }
        fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.fsync_dir(dir)
        }
        fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.rename_dir_noreplace(from, to)
        }
        fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.create_dir_all(dir)
        }
        fn append_fsync(&self, path: &Path, line: &[u8]) -> io::Result<()> {
            self.tick()?;
            self.inner.append_fsync(path, line)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.remove_dir_all(path)
        }
        // Reads + locks never fault — only durable mutations are crash-relevant.
        fn read_opt(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            self.inner.read_opt(path)
        }
        fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.inner.read_dir(dir)
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn lock_exclusive(&self, path: &Path) -> io::Result<LockGuard> {
            self.inner.lock_exclusive(path)
        }
        fn try_lock_exclusive(&self, path: &Path) -> io::Result<Option<LockGuard>> {
            self.inner.try_lock_exclusive(path)
        }
    }
}
