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

/// What a path is, by `lstat` (the final component is **not** dereferenced — a symlink is reported as
/// such, never as its target). The materializer branches on this: absent (`None`) is a first install, a
/// real `Dir` takes the atomic swap, a `Symlink` is canonicalized to its target dir, and `Other` (a
/// regular file or device where a skill dir belongs) is refused rather than clobbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathKind {
    /// A real directory.
    Dir,
    /// A symbolic link (the link itself, not its target).
    Symlink,
    /// Any other existing entry (regular file, device, fifo, …).
    Other,
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
    /// `lstat` a path: `None` if absent, else its [`PathKind`] (the final symlink is **not** followed).
    /// Read-only — used to pick the materialization strategy without touching bytes.
    fn path_kind(&self, path: &Path) -> io::Result<Option<PathKind>>;
    /// Write a staged file with its EXACT bundle mode (`0o755` if executable, else `0o644`) — the file
    /// mode is part of the consent-bound digest, so the materialized bit must match the approved bytes.
    /// Creates/truncates and forces the mode (defeating `umask`); **no fsync** (the caller fsyncs).
    fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> io::Result<()>;
    /// Create/truncate `path` and write `bytes` with mode **0600 from creation** — a SECRET (the device
    /// seed; later `follows.json` / a WAL). Unlike [`FsOps::write_staged`] (0644/0755) there is **no**
    /// world-readable window: the file is private from the open, then a defensive `set_permissions(0o600)`
    /// defeats a pre-existing looser mode / `umask`. **No fsync** (the caller fsyncs — see
    /// `crate::atomic::atomic_write_private`).
    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Whether `path` is owner-only (`mode & 0o077 == 0`) — the refuse-on-permissive gate a secret read
    /// uses to fail closed before trusting bytes a wider audience could have written. A pure read.
    fn private_perms_ok(&self, path: &Path) -> io::Result<bool>;
    /// Atomically exchange two EXISTING directories in one namespace operation (`RENAME_EXCHANGE` on
    /// Linux / `RENAME_SWAP` via `renameatx_np` on macOS — safe `rustix`, no `unsafe`). After it, each
    /// path names what the other held. The single primitive that lets an *update* land new bytes onto a
    /// populated harness dir with no torn/mixed/partial state. Errors typed (e.g. `ENOTSUP` on an FS
    /// without the syscall) so the caller can fall back.
    fn exchange_dir(&self, a: &Path, b: &Path) -> io::Result<()>;
    /// EXCLUSIVE create: write `bytes` to a file that must **not** already exist (`O_EXCL` —
    /// `create_new`), then fsync it. A true kernel-level exclusive, not a check-then-write: two
    /// racing creators get exactly one winner, and the loser's [`io::ErrorKind::AlreadyExists`]
    /// leaves the winner's file untouched. No temp file, no rename — so no shared temp name for a
    /// concurrent writer to tear.
    fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
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
        Self::fsync_handle(&f)?;
        // The first append CREATES the file — fsync the parent so its directory entry is durable too.
        if let Some(dir) = path.parent() {
            self.fsync_dir(dir)?;
        }
        Ok(())
    }

    // Removals are NotFound-tolerant: removing something a concurrent command already removed (a publish
    // rename that raced recovery's directory listing) is success, not a spurious hard error.
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
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

    fn path_kind(&self, path: &Path) -> io::Result<Option<PathKind>> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => Ok(Some(PathKind::Symlink)),
            Ok(m) if m.is_dir() => Ok(Some(PathKind::Dir)),
            Ok(_) => Ok(Some(PathKind::Other)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode: u32 = if executable { 0o755 } else { 0o644 };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)?;
        f.write_all(bytes)?;
        // `create(mode)` is masked by `umask` and ignored if the file already existed, so force the exact
        // mode — the executable bit is part of the bundle digest, so the placed bytes must match it.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        // `mode(0o600)` is masked by `umask` and ignored if the file already existed, so force 0600 — a
        // secret must never have a group/other-accessible window. This only tightens a pre-existing looser
        // file; a fresh file is already private from creation, so there is no chmod-after-write race.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn private_perms_ok(&self, path: &Path) -> io::Result<bool> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        Ok(mode & 0o077 == 0)
    }

    fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(bytes)?;
        Self::fsync_handle(&f)
    }

    fn exchange_dir(&self, a: &Path, b: &Path) -> io::Result<()> {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        {
            // EXCHANGE = renameat2(RENAME_EXCHANGE) on Linux / renameatx_np(RENAME_SWAP) on macOS — one
            // atomic namespace swap of two existing dirs. Safe rustix wrapper (no `unsafe`).
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                a,
                rustix::fs::CWD,
                b,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .map_err(io::Error::from)
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        {
            let _ = (a, b);
            Err(io::Error::from(rustix::io::Errno::NOSYS))
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
pub(crate) use hook::HookFs;

/// A test-only seam that lets a test act BETWEEN two of an operation's syscalls — the way a
/// concurrent process (or a person with a shell) would. `FaultFs` models a crash; this models a
/// RACE: the hook runs the first time a `write_staged` lands, i.e. inside the staging window, so a
/// revalidation-before-mutation rail can be proven rather than assumed.
#[cfg(test)]
mod hook {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// Wraps `RealFs` and runs a hook exactly once, immediately before a chosen op:
    ///
    /// - [`HookFs::new`] — before the FIRST [`FsOps::write_staged`] (inside a staging window);
    /// - [`HookFs::before_nth_exists`] — before the Nth [`FsOps::exists`] of ONE named path (the
    ///   read a destructive loop takes immediately before it removes a directory, and therefore
    ///   the seam that sits AFTER the scan which decided to remove it).
    ///
    /// Every other op passes straight through.
    pub(crate) struct HookFs<'a> {
        inner: RealFs,
        fired: AtomicBool,
        on_first_staged_write: Option<Box<dyn Fn() + 'a>>,
        exists_target: Option<PathBuf>,
        exists_nth: usize,
        exists_seen: AtomicUsize,
        on_exists: Option<Box<dyn Fn() + 'a>>,
    }

    impl<'a> HookFs<'a> {
        pub(crate) fn new(on_first_staged_write: impl Fn() + 'a) -> Self {
            Self {
                inner: RealFs,
                fired: AtomicBool::new(false),
                on_first_staged_write: Some(Box::new(on_first_staged_write)),
                exists_target: None,
                exists_nth: 0,
                exists_seen: AtomicUsize::new(0),
                on_exists: None,
            }
        }

        pub(crate) fn before_nth_exists(target: &Path, nth: usize, hook: impl Fn() + 'a) -> Self {
            Self {
                inner: RealFs,
                fired: AtomicBool::new(false),
                on_first_staged_write: None,
                exists_target: Some(target.to_path_buf()),
                exists_nth: nth,
                exists_seen: AtomicUsize::new(0),
                on_exists: Some(Box::new(hook)),
            }
        }
    }

    impl FsOps for HookFs<'_> {
        fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
            if let Some(hook) = &self.on_first_staged_write
                && !self.fired.swap(true, Ordering::Relaxed)
            {
                hook();
            }
            self.inner.write_staged(path, bytes, executable)
        }
        fn write_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.inner.write_temp(path, bytes)
        }
        fn fsync_file(&self, path: &Path) -> io::Result<()> {
            self.inner.fsync_file(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
            self.inner.fsync_dir(dir)
        }
        fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename_dir_noreplace(from, to)
        }
        fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
            self.inner.create_dir_all(dir)
        }
        fn append_fsync(&self, path: &Path, line: &[u8]) -> io::Result<()> {
            self.inner.append_fsync(path, line)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(path)
        }
        fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.inner.write_private(path, bytes)
        }
        fn exchange_dir(&self, a: &Path, b: &Path) -> io::Result<()> {
            self.inner.exchange_dir(a, b)
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.inner.write_new(path, bytes)
        }
        fn read_opt(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            self.inner.read_opt(path)
        }
        fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.inner.read_dir(dir)
        }
        fn exists(&self, path: &Path) -> bool {
            if let (Some(target), Some(hook)) = (&self.exists_target, &self.on_exists)
                && path == target
                && self.exists_seen.fetch_add(1, Ordering::Relaxed) + 1 == self.exists_nth
                && !self.fired.swap(true, Ordering::Relaxed)
            {
                hook();
            }
            self.inner.exists(path)
        }
        fn path_kind(&self, path: &Path) -> io::Result<Option<PathKind>> {
            self.inner.path_kind(path)
        }
        fn private_perms_ok(&self, path: &Path) -> io::Result<bool> {
            self.inner.private_perms_ok(path)
        }
        fn lock_exclusive(&self, path: &Path) -> io::Result<LockGuard> {
            self.inner.lock_exclusive(path)
        }
        fn try_lock_exclusive(&self, path: &Path) -> io::Result<Option<LockGuard>> {
            self.inner.try_lock_exclusive(path)
        }
    }
}

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
        fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
            self.tick()?;
            self.inner.write_staged(path, bytes, executable)
        }
        fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.tick()?;
            self.inner.write_private(path, bytes)
        }
        fn exchange_dir(&self, a: &Path, b: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.exchange_dir(a, b)
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.tick()?;
            self.inner.write_new(path, bytes)
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
        fn path_kind(&self, path: &Path) -> io::Result<Option<PathKind>> {
            self.inner.path_kind(path)
        }
        fn private_perms_ok(&self, path: &Path) -> io::Result<bool> {
            self.inner.private_perms_ok(path)
        }
        fn lock_exclusive(&self, path: &Path) -> io::Result<LockGuard> {
            self.inner.lock_exclusive(path)
        }
        fn try_lock_exclusive(&self, path: &Path) -> io::Result<Option<LockGuard>> {
            self.inner.try_lock_exclusive(path)
        }
    }
}
