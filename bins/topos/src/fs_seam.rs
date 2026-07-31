//! The private fault-injectable filesystem/syscall seam — the one place every durable mutation goes
//! through, so the crash gate can fail the Nth op and assert recovery.
//!
//! `RealFs` is `std::fs` + `rustix` (safe wrappers — no `unsafe`, honoring the workspace
//! `unsafe_code = "forbid"`): `F_FULLFSYNC` on macOS for true durability, plain `fsync` elsewhere, and
//! `flock` for the per-skill writer lock. `FaultFs` (test-only) wraps `RealFs` with a shared op counter
//! and fails the chosen op **without** performing it — modelling a crash just before that syscall, with a
//! genuine real-syscall prefix so post-fault on-disk state is authentic for recovery.
//!
//! ## The no-follow write boundary (unix)
//!
//! A containment proof is a statement about a PATH at one instant; the write it authorizes happens
//! at another. Re-checking the path harder never closes that gap — an ancestor swapped for a
//! symlink between the proof and the syscall re-aims the same spelling at a different tree. So the
//! writes themselves refuse to follow links:
//!
//! - every file create/open in this seam carries `O_NOFOLLOW` (a symlink at the final component
//!   is met as itself and refused, never written through), and the mode a staged/private write
//!   forces is applied through the ALREADY-OPEN descriptor (`fchmod`), never the path — the
//!   `O_NOFOLLOW` protection would otherwise end at the open, and a symlink swapped in behind it
//!   would take the chmod instead;
//! - [`FsOps::create_dir_nofollow`] builds directories at DIRECTORY HANDLES below a proven base:
//!   each component is reached from its PARENT's held fd (`openat(O_NOFOLLOW | O_DIRECTORY)` to
//!   descend, `mkdirat` when absent, re-`openat` after a create), so no path-based syscall runs
//!   below the base and `mkdir -p` through a swapped ancestor is structurally impossible — the
//!   held fd keeps naming the directory object that was proven or created, wherever its path now
//!   points (the caller's post-create re-proof still backstops the final SPELLING);
//! - [`DirHandle`] pins a PROVEN parent directory open across the window: the landing
//!   rename/exchange runs *at the held fd* ([`FsOps::rename_at`] / [`FsOps::exchange_at`], safe
//!   `rustix` `renameat`), after an `lstat`-vs-fd identity check, so even a swap in the final
//!   beat moves bytes inside the directory object that was proven — never through the swapped
//!   path — and the litter/probe removals beside it ride the same handle
//!   ([`FsOps::remove_dir_all_at`]: `openat` + fd-anchored iteration + `unlinkat`).
//!
//! Named residuals: `O_NOFOLLOW` guards only the final component (the directory walk + handle
//! anchoring cover the ancestors); a whole REAL directory relocated after the proof keeps its
//! identity and the held fd writes into it wherever it now sits (an attacker placing bytes in a
//! tree they could already write — not a redirect into a victim path); content READS of parked
//! trees (the settle rail's scans) and the park-journal restores of arbitrary paths stay
//! path-based — each destructive conclusion they feed is preceded by a handle identity check
//! where one is held, and the trees they act on are settle-rail-judged. On non-unix targets
//! these primitives would degrade to the path-based checks alone — this crate currently builds
//! on unix only, and a port must revisit this boundary (Windows has no `O_NOFOLLOW`;
//! junction/reparse-point checks would take its place).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// The `O_NOFOLLOW` open(2) flag as `OpenOptionsExt::custom_flags` wants it.
fn nofollow_flag() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

/// A HELD open directory handle — the anchor that makes a containment proof durable across the
/// proof-to-write window. Opened (`O_NOFOLLOW | O_DIRECTORY`) immediately after the caller's
/// containment proof, it pins the directory OBJECT the proof was about: `(dev, ino)` are captured
/// at open, [`DirHandle::verify_unmoved`] re-checks the path still names that object, and the
/// `*_at` seam ops run against the fd itself — a path swapped afterwards can no longer re-aim
/// them.
#[derive(Debug)]
pub(crate) struct DirHandle {
    file: File,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl DirHandle {
    /// The path this handle was opened at (the test seam's trigger matching + recording).
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Prove the path this handle was opened at STILL names the very directory object the handle
    /// holds: `lstat` the path (no-follow — a symlink swapped in reads as itself, never its
    /// target) and compare `(dev, ino)` against the fd captured at proof time. A mismatch means
    /// an ancestor swap re-aimed the spelling since the proof — the caller refuses; nothing
    /// re-resolves.
    pub(crate) fn verify_unmoved(&self) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&self.path)?;
        if meta.dev() == self.dev && meta.ino() == self.ino {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{} no longer names the directory it was proven as (replaced since the \
                 containment proof); refusing to write through it",
                self.path.display()
            )))
        }
    }
}

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
    /// [`FsOps::read_opt`] that refuses to FOLLOW a final-component symlink (`O_NOFOLLOW` open) —
    /// the read for topos's OWN persisted documents (lock/map/sync/journal/…), which topos only
    /// ever writes as regular files. Following a link there turns a DANGLING symlink into a lying
    /// "absent" (and recovery deletes on "absent"), so a symlink reads as an ERROR — unreadable,
    /// fail-closed — never as `None`.
    fn read_opt_nofollow(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
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
    /// Open a directory (`O_NOFOLLOW | O_DIRECTORY`) as a held [`DirHandle`], capturing its
    /// `(dev, ino)` — called immediately after a containment proof so the proven object is
    /// pinned across the proof-to-write window. A pure open; nothing is mutated.
    fn open_dir_handle(&self, dir: &Path) -> io::Result<DirHandle>;
    /// Rename `from` → `to`, both LEAF names inside the held directory — `renameat` against the
    /// handle's fd, after [`DirHandle::verify_unmoved`]. The landing-rename primitive a proven
    /// parent's writes use: a path swap after the proof cannot re-aim it.
    fn rename_at(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()>;
    /// [`FsOps::rename_at`] refusing an existing target (no-replace), by leaf names inside the
    /// held directory. The check runs at the held fd (`statat`, no-follow); the caller's writer
    /// lock closes the check→rename beat against topos's own writers.
    fn rename_at_noreplace(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()>;
    /// Atomically exchange two EXISTING directories by LEAF names inside the held directory —
    /// one namespace operation (`RENAME_EXCHANGE` on Linux / `RENAME_SWAP` via `renameatx_np` on
    /// macOS — safe `rustix`, no `unsafe`) run at the handle's fd, after
    /// [`DirHandle::verify_unmoved`]. After it, each name holds what the other did — the single
    /// primitive that lets an *update* land new bytes onto a populated harness dir with no
    /// torn/mixed/partial state. Errors typed (e.g. `ENOTSUP` on an FS without the syscall) so
    /// the caller's capability fallback still works.
    fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()>;
    /// [`FsOps::remove_dir_all`] by LEAF name AT the held handle: the tree is opened
    /// `openat(O_NOFOLLOW | O_DIRECTORY)` from the handle's fd (after
    /// [`DirHandle::verify_unmoved`]) and deleted by fd-anchored iteration (`rustix` `Dir` +
    /// `unlinkat`/`REMOVEDIR`) — no path-based syscall anywhere, so a parent swapped after the
    /// proof cannot re-aim a removal outside the proven directory object. A symlink at the leaf
    /// is unlinked as ITSELF, never followed; an absent leaf is success (NotFound-tolerant,
    /// like [`FsOps::remove_dir_all`]).
    fn remove_dir_all_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()>;
    /// `mkdir` ONE level by LEAF name at the held handle (`mkdirat` after
    /// [`DirHandle::verify_unmoved`]). Strict: an existing entry (whatever it is) is
    /// `AlreadyExists`, never adopted — the probe dirs this creates were removed a beat earlier
    /// under the same handle.
    fn create_dir_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()>;
    /// EXCLUSIVE create + fsync by LEAF name AT the held handle — a true kernel-level exclusive
    /// (two racing creators get exactly one winner; the loser's `AlreadyExists` leaves the
    /// winner's file untouched):
    /// `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` from the handle's fd after
    /// [`DirHandle::verify_unmoved`], then fsync the file AND the directory entry (a plain fsync
    /// of the held fd) — the write lands in the very directory object the walk proved, so a path
    /// swapped after the proof cannot re-aim it.
    fn write_new_at(&self, h: &DirHandle, leaf: &str, bytes: &[u8]) -> io::Result<()>;
    /// `mkdir -p` that REFUSES to follow a symlink, walked at DIRECTORY HANDLES: `dir` must sit
    /// lexically under `base` (an already-proven directory), and each component below `base` is
    /// reached from its PARENT's held fd — `openat(O_NOFOLLOW | O_DIRECTORY)` to descend,
    /// `mkdirat` from the same fd when absent, a re-`openat` after a create — so no path-based
    /// syscall runs below the base: a swapped ancestor is met as itself (refused at its own
    /// level) and can never re-aim the walk, because the walk never re-resolves a path it
    /// already holds. `..`/root components refuse. Returns the HELD handle of the final
    /// directory so the caller's next write can run at the very object the walk proved
    /// ([`FsOps::write_new_at`]).
    fn create_dir_nofollow(&self, base: &Path, dir: &Path) -> io::Result<DirHandle>;
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

    /// [`FsOps::write_staged`]'s body, with a test-only beat (`before_chmod`) fired between the
    /// write through the opened descriptor and the `fchmod` that forces the exact mode — the seam
    /// a chmod-follows-a-swapped-symlink race would land in. Production passes a no-op.
    fn write_staged_hooked(
        path: &Path,
        bytes: &[u8],
        executable: bool,
        before_chmod: &dyn Fn(),
    ) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode: u32 = if executable { 0o755 } else { 0o644 };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .custom_flags(nofollow_flag())
            .open(path)?;
        f.write_all(bytes)?;
        before_chmod();
        // `create(mode)` is masked by `umask` and ignored if the file already existed, so force
        // the exact mode — the executable bit is part of the bundle digest, so the placed bytes
        // must match it. Through the ALREADY-OPEN descriptor (`fchmod`), never the path: the
        // `O_NOFOLLOW` protection ends at the open, and a symlink swapped in at the path after it
        // would take a path-chmod onto its target instead.
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    /// [`FsOps::write_private`]'s body — the same test-only beat as
    /// [`RealFs::write_staged_hooked`], for the 0600 secret arm.
    fn write_private_hooked(path: &Path, bytes: &[u8], before_chmod: &dyn Fn()) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(nofollow_flag())
            .open(path)?;
        f.write_all(bytes)?;
        before_chmod();
        // `mode(0o600)` is masked by `umask` and ignored if the file already existed, so force
        // 0600 — a secret must never have a group/other-accessible window. Through the
        // ALREADY-OPEN descriptor (`fchmod`), never the path (same reasoning as the staged arm:
        // a path-chmod after the open can be re-aimed by a swapped-in symlink — here it would
        // REVOKE access on an unrelated file). This only tightens a pre-existing looser file; a
        // fresh file is already private from creation, so there is no chmod-after-write race.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// The fd-anchored walk behind [`FsOps::create_dir_nofollow`]: the proven base is opened ONCE
    /// (`O_DIRECTORY`; its identity is the caller's containment proof), and every component below
    /// it is reached from its PARENT's held descriptor — `openat(O_NOFOLLOW | O_DIRECTORY)` to
    /// descend, `mkdirat` from the same fd when absent, then a re-`openat` of what was created
    /// (a symlink or non-directory that appeared in the beat is met as itself and refused). No
    /// path-based syscall runs below the base, so an ancestor swapped for a symlink mid-walk
    /// cannot re-aim the walk: the held fd keeps naming the directory object that was proven or
    /// created, wherever its path now points. `observe` is the test seam's beat — fired with each
    /// component's full path immediately before that component is opened-or-created.
    fn create_dir_walk(base: &Path, dir: &Path, observe: &dyn Fn(&Path)) -> io::Result<DirHandle> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        use std::path::Component;
        let rel = dir.strip_prefix(base).map_err(|_| {
            io::Error::other(format!(
                "{} is not under {}; refusing the no-follow create",
                dir.display(),
                base.display()
            ))
        })?;
        let base_flags = rustix::fs::OFlags::DIRECTORY.bits() as i32;
        let mut cur = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(base_flags)
            .open(base)?;
        let mut cur_path = base.to_path_buf();
        for comp in rel.components() {
            let name = match comp {
                Component::Normal(c) => c,
                Component::CurDir => continue,
                // `..` (or a root/prefix) would climb out of the proven base — refuse it whole.
                _ => {
                    return Err(io::Error::other(format!(
                        "{} climbs out of {}; refusing the no-follow create",
                        dir.display(),
                        base.display()
                    )));
                }
            };
            cur_path.push(name);
            observe(&cur_path);
            let next = match openat_dir_nofollow(&cur, name) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => {
                    match rustix::fs::mkdirat(
                        &cur,
                        name,
                        rustix::fs::Mode::from_bits_truncate(0o777),
                    ) {
                        // A racing creator won the level — the re-open below meets what appeared
                        // as ITSELF (a symlink that landed in the beat is exactly the swap this
                        // walk exists to refuse).
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(io::Error::from(e)),
                    }
                    match openat_dir_nofollow(&cur, name) {
                        Ok(fd) => fd,
                        Err(e) => return Err(refuse_component(&cur_path, e)),
                    }
                }
                Err(e) => return Err(refuse_component(&cur_path, e)),
            };
            cur = next;
        }
        let meta = cur.metadata()?;
        Ok(DirHandle {
            file: cur,
            path: dir.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

/// `openat` a DIRECTORY child from a held parent fd, refusing to follow a symlink at the name.
fn openat_dir_nofollow(
    parent: &File,
    name: impl rustix::path::Arg,
) -> Result<File, rustix::io::Errno> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()).map(File::from)
}

/// The typed refusal for a walk component that is not an openable real directory: a symlink is
/// met as itself (`ELOOP`/`EMLINK` under `O_NOFOLLOW`), a non-directory as `ENOTDIR`.
fn refuse_component(at: &Path, e: rustix::io::Errno) -> io::Error {
    match e {
        rustix::io::Errno::LOOP | rustix::io::Errno::MLINK => io::Error::other(format!(
            "{} is a symlink; refusing to create through it",
            at.display()
        )),
        rustix::io::Errno::NOTDIR => {
            io::Error::other(format!("{} exists and is not a directory", at.display()))
        }
        e => io::Error::from(e),
    }
}

/// Delete one ENTRY (by name) from an open directory fd, recursively and fd-anchored: a child
/// directory is descended via [`openat_dir_nofollow`] and emptied entry-by-entry (`rustix`
/// `Dir` iteration over the held fd), every removal an `unlinkat` from its parent's fd; a
/// symlink or non-directory at any name is unlinked as ITSELF, never followed. NotFound at any
/// step is tolerated (a concurrent remover concluding first is success, matching
/// [`FsOps::remove_dir_all`]).
fn remove_entry_at(dir: &File, name: &std::ffi::CStr) -> io::Result<()> {
    use rustix::io::Errno;
    match openat_dir_nofollow(dir, name) {
        Ok(sub) => {
            let mut names: Vec<std::ffi::CString> = Vec::new();
            for entry in rustix::fs::Dir::read_from(&sub).map_err(io::Error::from)? {
                let entry = entry.map_err(io::Error::from)?;
                let bytes = entry.file_name().to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                names.push(entry.file_name().to_owned());
            }
            for child in names {
                remove_entry_at(&sub, &child)?;
            }
            match rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::REMOVEDIR) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(io::Error::from(e)),
            }
        }
        Err(Errno::NOENT) => Ok(()),
        // Not an openable real directory (a symlink, a file, a device): unlink the entry itself.
        Err(Errno::LOOP | Errno::MLINK | Errno::NOTDIR) => {
            match rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(io::Error::from(e)),
            }
        }
        Err(e) => Err(io::Error::from(e)),
    }
}

/// A leaf name as the `CString` the `*at` syscalls take (an interior NUL cannot name a real file).
fn leaf_cstr(leaf: &str) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(leaf)
        .map_err(|_| io::Error::other(format!("{leaf:?} contains an interior NUL")))
}

impl FsOps for RealFs {
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(nofollow_flag())
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
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .custom_flags(nofollow_flag())
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

    fn read_opt_nofollow(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nofollow_flag())
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            // A symlink at the final component (ELOOP under O_NOFOLLOW) — or anything else —
            // surfaces as the error it is: UNREADABLE, never "absent".
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        f.read_to_end(&mut out)?;
        Ok(Some(out))
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
        Self::write_staged_hooked(path, bytes, executable, &|| {})
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        Self::write_private_hooked(path, bytes, &|| {})
    }

    fn private_perms_ok(&self, path: &Path) -> io::Result<bool> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        Ok(mode & 0o077 == 0)
    }

    fn open_dir_handle(&self, dir: &Path) -> io::Result<DirHandle> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let flags = (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY).bits() as i32;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(dir)?;
        let meta = file.metadata()?;
        Ok(DirHandle {
            file,
            path: dir.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    fn rename_at(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()> {
        h.verify_unmoved()?;
        rustix::fs::renameat(&h.file, from, &h.file, to).map_err(io::Error::from)
    }

    fn rename_at_noreplace(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()> {
        h.verify_unmoved()?;
        // No-replace by an fd-anchored no-follow probe; the caller's writer lock closes the
        // check→rename beat for topos's own writers (the same contract `rename_dir_noreplace`
        // documents).
        match rustix::fs::statat(&h.file, to, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "target exists",
                ));
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(e) => return Err(io::Error::from(e)),
        }
        rustix::fs::renameat(&h.file, from, &h.file, to).map_err(io::Error::from)
    }

    fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
        h.verify_unmoved()?;
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        {
            rustix::fs::renameat_with(&h.file, a, &h.file, b, rustix::fs::RenameFlags::EXCHANGE)
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

    fn remove_dir_all_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()> {
        h.verify_unmoved()?;
        let name = leaf_cstr(leaf)?;
        remove_entry_at(&h.file, &name)
    }

    fn create_dir_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()> {
        h.verify_unmoved()?;
        rustix::fs::mkdirat(&h.file, leaf, rustix::fs::Mode::from_bits_truncate(0o777))
            .map_err(io::Error::from)
    }

    fn write_new_at(&self, h: &DirHandle, leaf: &str, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        h.verify_unmoved()?;
        let flags = rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let fd = rustix::fs::openat(
            &h.file,
            leaf,
            flags,
            rustix::fs::Mode::from_bits_truncate(0o666),
        )
        .map_err(io::Error::from)?;
        let mut f = File::from(fd);
        f.write_all(bytes)?;
        Self::fsync_handle(&f)?;
        // The directory ENTRY, through the held fd (a plain fsync persists entries).
        rustix::fs::fsync(&h.file).map_err(io::Error::from)
    }

    fn create_dir_nofollow(&self, base: &Path, dir: &Path) -> io::Result<DirHandle> {
        Self::create_dir_walk(base, dir, &|_| {})
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-seam-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// P1: the `O_NOFOLLOW` protection must not end at the open. The mode is forced through the
    /// ALREADY-OPEN descriptor (`fchmod`), so a symlink swapped in at the path BETWEEN the open
    /// and the chmod — the exact beat this hook lands in — chmods nothing but the opened inode:
    /// the staged arm can no longer make an unrelated private file 0644.
    #[test]
    fn a_symlink_swapped_in_after_the_open_never_takes_the_staged_chmod() {
        let dir = scratch("chmod-staged");
        let vdir = scratch("chmod-staged-victim");
        let victim = vdir.join("secret");
        std::fs::write(&victim, b"private bytes\n").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
        let target = dir.join("staged.txt");
        let (t, v) = (target.clone(), victim.clone());
        let fs = HookFs::before_chmod_of(&target, move || {
            // The swap: the just-opened file leaves the path, a symlink to the victim takes it.
            std::fs::remove_file(&t).unwrap();
            std::os::unix::fs::symlink(&v, &t).unwrap();
        });
        fs.write_staged(&target, b"payload\n", false).unwrap();
        assert_eq!(mode_of(&victim), 0o600, "the victim's mode is untouched");
        assert_eq!(std::fs::read(&victim).unwrap(), b"private bytes\n");
        assert!(
            std::fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the swapped-in link itself is what remains at the path"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&vdir);
    }

    /// P1, the private arm: the same swap must not REVOKE access on an unrelated file (a
    /// path-chmod to 0600 through the link would).
    #[test]
    fn a_symlink_swapped_in_after_the_open_never_takes_the_private_chmod() {
        let dir = scratch("chmod-private");
        let vdir = scratch("chmod-private-victim");
        let victim = vdir.join("shared.txt");
        std::fs::write(&victim, b"world-readable\n").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        let target = dir.join("secret.json");
        let (t, v) = (target.clone(), victim.clone());
        let fs = HookFs::before_chmod_of(&target, move || {
            std::fs::remove_file(&t).unwrap();
            std::os::unix::fs::symlink(&v, &t).unwrap();
        });
        fs.write_private(&target, b"{}\n").unwrap();
        assert_eq!(
            mode_of(&victim),
            0o644,
            "the victim's access is not revoked"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"world-readable\n");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&vdir);
    }

    /// P1 (the dangling-lock class at its root): a symlink at a persisted-doc path reads as an
    /// ERROR through the no-follow read — never as "absent". The dangling case is the trap: a
    /// follow-read maps the target's NotFound to `None`, and recovery deletes on `None`.
    #[test]
    fn read_opt_nofollow_reads_a_symlink_as_an_error_never_as_absent() {
        let dir = scratch("nofollow-read");
        let dangling = dir.join("lock.json");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
        assert!(
            RealFs.read_opt_nofollow(&dangling).is_err(),
            "a dangling symlink is UNREADABLE, not absent"
        );
        // A live symlink is refused the same way — the bytes behind it are not this doc's.
        let real = dir.join("real.json");
        std::fs::write(&real, b"{}").unwrap();
        let live = dir.join("map.json");
        std::os::unix::fs::symlink(&real, &live).unwrap();
        assert!(RealFs.read_opt_nofollow(&live).is_err());
        // An absent path is still a plain None; a regular file still reads.
        assert!(
            RealFs
                .read_opt_nofollow(&dir.join("gone"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            RealFs.read_opt_nofollow(&real).unwrap().unwrap(),
            b"{}".to_vec()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A test-only seam that lets a test act BETWEEN two of an operation's syscalls — the way a
/// concurrent process (or a person with a shell) would. `FaultFs` models a crash; this models a
/// RACE: the hook runs the first time a `write_staged` lands, i.e. inside the staging window, so a
/// revalidation-before-mutation rail can be proven rather than assumed.
#[cfg(test)]
mod hook {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// WHEN a [`HookFs`] fires — one trigger per instance, each naming the exact seam a race
    /// would land in.
    enum Trigger {
        /// The FIRST [`FsOps::write_staged`] (inside a staging window).
        FirstStagedWrite,
        /// The Nth [`FsOps::exists`] of ONE named path (the read a destructive loop takes
        /// immediately before it acts on a directory, and therefore the seam that sits AFTER the
        /// scan which decided to).
        NthExists { target: PathBuf, nth: usize },
        /// The Nth [`FsOps::create_dir_all`] of ONE named directory — the seam a read-modify-write
        /// crosses on its way from the read to the write.
        NthCreateDirAll { target: PathBuf, nth: usize },
        /// The Nth [`FsOps::read_opt`] of ONE named file — how a test lands an outside edit
        /// BETWEEN the read that decides an act and the read that re-proves it.
        NthRead { target: PathBuf, nth: usize },
        /// The FIRST namespace move of ONE directory — the [`FsOps::rename`] that PARKS it or the
        /// [`FsOps::exchange_at`] that swaps it. The park-then-verify rail's race point: after
        /// this instant the tree is parked (or replaced) and no path reaches it, so an edit
        /// landing here is the last one a run can lose.
        FirstMoveOf { dir: PathBuf },
        /// Immediately AFTER [`FsOps::open_dir_handle`] pins ONE named directory — the first beat
        /// of the proof-to-write window, where a concurrent process swaps the proven parent's
        /// PATH out from under the held fd.
        AfterHandleOpen { target: PathBuf },
        /// Between a staged/private write's (`O_NOFOLLOW`) open+write and the `fchmod` that
        /// forces its mode — the beat where a path-based chmod would follow a swapped-in symlink.
        BeforeChmodOf { target: PathBuf },
        /// Immediately before a [`FsOps::create_dir_nofollow`] walk descends to ONE named
        /// component (its `openat`-or-`mkdirat`) — the mid-walk beat where an already-walked
        /// ancestor is swapped for a symlink.
        BeforeComponentOf { target: PathBuf },
    }

    /// Wraps `RealFs` and runs a hook exactly once, immediately before the op its [`Trigger`]
    /// names. Every other op passes straight through.
    pub(crate) struct HookFs<'a> {
        inner: RealFs,
        fired: AtomicBool,
        seen: AtomicUsize,
        trigger: Trigger,
        hook: Box<dyn Fn() + 'a>,
    }

    impl<'a> HookFs<'a> {
        fn with(trigger: Trigger, hook: impl Fn() + 'a) -> Self {
            Self {
                inner: RealFs,
                fired: AtomicBool::new(false),
                seen: AtomicUsize::new(0),
                trigger,
                hook: Box::new(hook),
            }
        }

        pub(crate) fn new(on_first_staged_write: impl Fn() + 'a) -> Self {
            Self::with(Trigger::FirstStagedWrite, on_first_staged_write)
        }

        pub(crate) fn before_nth_exists(target: &Path, nth: usize, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::NthExists {
                    target: target.to_path_buf(),
                    nth,
                },
                hook,
            )
        }

        pub(crate) fn before_nth_create_dir_all(
            target: &Path,
            nth: usize,
            hook: impl Fn() + 'a,
        ) -> Self {
            Self::with(
                Trigger::NthCreateDirAll {
                    target: target.to_path_buf(),
                    nth,
                },
                hook,
            )
        }

        pub(crate) fn before_nth_read(target: &Path, nth: usize, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::NthRead {
                    target: target.to_path_buf(),
                    nth,
                },
                hook,
            )
        }

        pub(crate) fn before_first_move_of(dir: &Path, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::FirstMoveOf {
                    dir: dir.to_path_buf(),
                },
                hook,
            )
        }

        pub(crate) fn after_dir_handle_open(target: &Path, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::AfterHandleOpen {
                    target: target.to_path_buf(),
                },
                hook,
            )
        }

        pub(crate) fn before_chmod_of(target: &Path, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::BeforeChmodOf {
                    target: target.to_path_buf(),
                },
                hook,
            )
        }

        pub(crate) fn before_component_of(target: &Path, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::BeforeComponentOf {
                    target: target.to_path_buf(),
                },
                hook,
            )
        }

        /// Fire once when `matched` and the trigger's own counter reaches `nth`.
        fn maybe_fire(&self, matched: bool, nth: usize) {
            if !matched {
                return;
            }
            if self.seen.fetch_add(1, Ordering::Relaxed) + 1 == nth
                && !self.fired.swap(true, Ordering::Relaxed)
            {
                (self.hook)();
            }
        }
    }

    impl FsOps for HookFs<'_> {
        fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
            self.maybe_fire(matches!(self.trigger, Trigger::FirstStagedWrite), 1);
            if matches!(&self.trigger, Trigger::BeforeChmodOf { target } if target == path) {
                // Land the hook INSIDE the op, between the (O_NOFOLLOW) open+write and the fchmod
                // — the exact beat a path-swapped symlink would catch a path-based chmod.
                return RealFs::write_staged_hooked(path, bytes, executable, &|| {
                    self.maybe_fire(true, 1);
                });
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
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if dir == from),
                1,
            );
            self.inner.rename(from, to)
        }
        fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
            self.inner.fsync_dir(dir)
        }
        fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if dir == from),
                1,
            );
            self.inner.rename_dir_noreplace(from, to)
        }
        fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
            let (matched, nth) = match &self.trigger {
                Trigger::NthCreateDirAll { target, nth } => (target == dir, *nth),
                _ => (false, 0),
            };
            self.maybe_fire(matched, nth);
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
            if matches!(&self.trigger, Trigger::BeforeChmodOf { target } if target == path) {
                return RealFs::write_private_hooked(path, bytes, &|| {
                    self.maybe_fire(true, 1);
                });
            }
            self.inner.write_private(path, bytes)
        }
        fn open_dir_handle(&self, dir: &Path) -> io::Result<DirHandle> {
            let h = self.inner.open_dir_handle(dir)?;
            // AFTER the open: the handle is pinned, and the hook models the swap that follows it.
            self.maybe_fire(
                matches!(&self.trigger, Trigger::AfterHandleOpen { target } if target == dir),
                1,
            );
            Ok(h)
        }
        fn rename_at(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()> {
            let joined = h.path().join(from);
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if *dir == joined),
                1,
            );
            self.inner.rename_at(h, from, to)
        }
        fn rename_at_noreplace(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()> {
            let joined = h.path().join(from);
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if *dir == joined),
                1,
            );
            self.inner.rename_at_noreplace(h, from, to)
        }
        fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
            let (ja, jb) = (h.path().join(a), h.path().join(b));
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if *dir == ja || *dir == jb),
                1,
            );
            self.inner.exchange_at(h, a, b)
        }
        fn create_dir_nofollow(&self, base: &Path, dir: &Path) -> io::Result<DirHandle> {
            let (matched, nth) = match &self.trigger {
                Trigger::NthCreateDirAll { target, nth } => (target == dir, *nth),
                _ => (false, 0),
            };
            self.maybe_fire(matched, nth);
            if matches!(&self.trigger, Trigger::BeforeComponentOf { .. }) {
                // Land the hook INSIDE the walk: immediately before the named component's
                // openat-or-mkdirat, with every prior component's fd already held.
                return RealFs::create_dir_walk(base, dir, &|cur| {
                    self.maybe_fire(
                        matches!(&self.trigger, Trigger::BeforeComponentOf { target } if target == cur),
                        1,
                    );
                });
            }
            self.inner.create_dir_nofollow(base, dir)
        }
        fn remove_dir_all_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()> {
            self.inner.remove_dir_all_at(h, leaf)
        }
        fn create_dir_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()> {
            self.inner.create_dir_at(h, leaf)
        }
        fn write_new_at(&self, h: &DirHandle, leaf: &str, bytes: &[u8]) -> io::Result<()> {
            self.inner.write_new_at(h, leaf, bytes)
        }
        fn read_opt(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            let (matched, nth) = match &self.trigger {
                Trigger::NthRead { target, nth } => (target == path, *nth),
                _ => (false, 0),
            };
            self.maybe_fire(matched, nth);
            self.inner.read_opt(path)
        }
        fn read_opt_nofollow(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            let (matched, nth) = match &self.trigger {
                Trigger::NthRead { target, nth } => (target == path, *nth),
                _ => (false, 0),
            };
            self.maybe_fire(matched, nth);
            self.inner.read_opt_nofollow(path)
        }
        fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.inner.read_dir(dir)
        }
        fn exists(&self, path: &Path) -> bool {
            let (matched, nth) = match &self.trigger {
                Trigger::NthExists { target, nth } => (target == path, *nth),
                _ => (false, 0),
            };
            self.maybe_fire(matched, nth);
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
        fn rename_at(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()> {
            self.tick()?;
            self.inner.rename_at(h, from, to)
        }
        fn rename_at_noreplace(&self, h: &DirHandle, from: &str, to: &str) -> io::Result<()> {
            self.tick()?;
            self.inner.rename_at_noreplace(h, from, to)
        }
        fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
            self.tick()?;
            self.inner.exchange_at(h, a, b)
        }
        fn create_dir_nofollow(&self, base: &Path, dir: &Path) -> io::Result<DirHandle> {
            self.tick()?;
            self.inner.create_dir_nofollow(base, dir)
        }
        fn remove_dir_all_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()> {
            self.tick()?;
            self.inner.remove_dir_all_at(h, leaf)
        }
        fn create_dir_at(&self, h: &DirHandle, leaf: &str) -> io::Result<()> {
            self.tick()?;
            self.inner.create_dir_at(h, leaf)
        }
        fn write_new_at(&self, h: &DirHandle, leaf: &str, bytes: &[u8]) -> io::Result<()> {
            self.tick()?;
            self.inner.write_new_at(h, leaf, bytes)
        }
        // Reads + locks never fault — only durable mutations are crash-relevant.
        fn open_dir_handle(&self, dir: &Path) -> io::Result<DirHandle> {
            self.inner.open_dir_handle(dir)
        }
        fn read_opt(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            self.inner.read_opt(path)
        }
        fn read_opt_nofollow(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            self.inner.read_opt_nofollow(path)
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
