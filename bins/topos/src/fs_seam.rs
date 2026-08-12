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
//!   would take the chmod instead — and BEFORE any byte is written: a pre-existing file at the
//!   path keeps its own (possibly permissive) mode through the open, so a secret written first
//!   and tightened second would be world-readable in the window, and exposed for good by a crash
//!   inside it;
//! - [`FsOps::create_dir_nofollow`] builds directories at DIRECTORY HANDLES below a proven base:
//!   each component is reached from its PARENT's held fd (`openat(O_NOFOLLOW | O_DIRECTORY)` to
//!   descend, `mkdirat` when absent, re-`openat` after a create), so no path-based syscall runs
//!   below the base and `mkdir -p` through a swapped ancestor is structurally impossible — the
//!   held fd keeps naming the directory object that was proven or created, wherever its path now
//!   points (the caller's post-create re-proof still backstops the final SPELLING); where the
//!   base is itself an already-HELD handle, [`FsOps::create_dir_nofollow_at`] descends from that
//!   fd directly — the base PATH is never re-opened, so a staging root swapped for an outward
//!   symlink after its creation cannot re-aim the walk (the path-based variant remains only
//!   where no handle is yet held);
//! - [`DirHandle`] pins a PROVEN parent directory open across the window: the landing
//!   rename/exchange runs *at the held fd* ([`FsOps::rename_at`] / [`FsOps::exchange_at`], safe
//!   `rustix` `renameat`), after an `lstat`-vs-fd identity check, so even a swap in the final
//!   beat moves bytes inside the directory object that was proven — never through the swapped
//!   path — and the litter/probe removals beside it ride the same handle
//!   ([`FsOps::remove_dir_all_at`]: `openat` + fd-anchored iteration + `unlinkat`). Where the
//!   SOURCE of a landing move is a staged tree built under a held handle, the `_src` landings
//!   ([`FsOps::exchange_at_src`] / [`FsOps::rename_at_noreplace_src`]) additionally re-open the
//!   source leaf from the parent's fd and prove it `(dev, ino)`-identical to that staging handle
//!   ([`DirHandle::verify_leaf_is`]) as the LAST operation before the namespace op — the parent
//!   re-proof and any no-replace probe run FIRST, so nothing sits between the leaf proof and the
//!   `renameat` — and a completed stage moved aside and SUBSTITUTED at its predictable name
//!   refuses instead of landing.
//!
//! Named residuals: `O_NOFOLLOW` guards only the final component (the directory walk + handle
//! anchoring cover the ancestors); a whole REAL directory relocated after the proof keeps its
//! identity and the held fd writes into it wherever it now sits (an attacker placing bytes in a
//! tree they could already write — not a redirect into a victim path); the `_src` landings'
//! source-identity proof and the rename/exchange it authorizes are still two SYSCALLS — the
//! proof runs last, with nothing between it and the namespace op, so a substitution can land
//! only inside that single syscall-pair window (irreducible from user space; every check that
//! can run earlier does); content READS of parked
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
    /// The path this handle was opened at (leaf derivation for the `*_at` ops; the test seam's
    /// trigger matching + recording).
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

    /// Prove the entry `leaf` names inside `parent` is the very directory object THIS handle
    /// holds: `openat(O_NOFOLLOW | O_DIRECTORY)` from the parent's held fd (a swapped-in symlink
    /// or non-directory is met as itself and refused), fstat what the open returned, and compare
    /// `(dev, ino)` against the capture taken when this handle was created. The landing's
    /// SOURCE-identity check: a completed stage moved aside and substituted at its predictable
    /// leaf name in the final beat earns a typed refusal, never a landing.
    pub(crate) fn verify_leaf_is(&self, parent: &DirHandle, leaf: &str) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let f = openat_dir_nofollow(&parent.file, leaf).map_err(|e| {
            io::Error::other(format!(
                "{} no longer opens as the directory staged there ({e}); refusing to land it",
                parent.path.join(leaf).display()
            ))
        })?;
        let meta = f.metadata()?;
        if meta.dev() == self.dev && meta.ino() == self.ino {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{} no longer names the directory staged there (replaced since the staging was \
                 built); refusing to land it",
                parent.path.join(leaf).display()
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
    /// held directory, for a landing whose SOURCE `from` is a stage built under a
    /// held handle: the check runs at the held fd (`statat`, no-follow) and the caller's writer
    /// lock closes the check→rename beat against topos's own writers. The parent re-proof and the
    /// no-replace probe run FIRST, then `from` is
    /// re-opened from the parent's fd and proven `(dev, ino)`-identical to `src` — the staging
    /// handle captured at construction ([`DirHandle::verify_leaf_is`]) — as the LAST operation
    /// before the rename syscall, with nothing between them. A completed stage moved aside and
    /// SUBSTITUTED at its predictable name in the final beat is a typed refusal — nothing lands
    /// (the remaining proof-to-rename syscall pair is the module doc's named residual).
    fn rename_at_noreplace_src(
        &self,
        h: &DirHandle,
        from: &str,
        to: &str,
        src: &DirHandle,
    ) -> io::Result<()>;
    /// Atomically exchange two EXISTING directories by LEAF names inside the held directory —
    /// one namespace operation (`RENAME_EXCHANGE` on Linux / `RENAME_SWAP` via `renameatx_np` on
    /// macOS — safe `rustix`, no `unsafe`) run at the handle's fd, after
    /// [`DirHandle::verify_unmoved`]. After it, each name holds what the other did — the single
    /// primitive that lets an *update* land new bytes onto a populated harness dir with no
    /// torn/mixed/partial state. Errors typed (e.g. `ENOTSUP` on an FS without the syscall) so
    /// the caller's capability fallback still works.
    fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()>;
    /// [`FsOps::exchange_at`] with [`FsOps::rename_at_noreplace_src`]'s source-identity proof
    /// and its ordering: the parent re-proof runs first, then the leaf `a` is re-opened from the
    /// parent's fd and must still be the very directory object `src` holds
    /// ([`DirHandle::verify_leaf_is`]) — the LAST operation before the exchange syscall — so a
    /// substituted stage refuses, never swaps live.
    fn exchange_at_src(&self, h: &DirHandle, a: &str, b: &str, src: &DirHandle) -> io::Result<()>;
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
    /// [`FsOps::write_staged`] by LEAF name AT the held handle — the staging-construction write:
    /// `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` from the handle's fd after
    /// [`DirHandle::verify_unmoved`] (every staged file is NEW, so an entry already at the name —
    /// a swapped-in symlink included — is `AlreadyExists`, never truncated through), the EXACT
    /// bundle mode forced through the open descriptor BEFORE any byte lands, then fd-fsyncs of
    /// the file and its directory entry (a plain fsync of the held fd). No path-based syscall —
    /// an ancestor swapped after the walk that proved the parent cannot re-aim the write.
    fn write_staged_at(
        &self,
        h: &DirHandle,
        leaf: &str,
        bytes: &[u8],
        executable: bool,
    ) -> io::Result<()>;
    /// Rename a FILE `from` → `to` refusing an EXISTING target at the KERNEL level
    /// (`RENAME_NOREPLACE` / macOS `RENAME_EXCL`; where the filesystem cannot, the
    /// hard-link+unlink dance — a link refuses an existing name atomically). The no-replace
    /// landing primitive a file BIRTH uses: unlike [`FsOps::rename_dir_noreplace`]'s
    /// check-then-rename (whose window only the caller's lock closes, against topos's own
    /// writers), an outside writer's file that appeared in the beat is met as `AlreadyExists` —
    /// nothing overwritten, the caller owns the typed refusal.
    fn rename_file_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>;
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
    /// [`FsOps::create_dir_nofollow`] whose BASE is an already-HELD handle: the walk descends
    /// from the handle's fd itself instead of re-opening the base PATH (a staging root's
    /// pathname is predictable and mutable, and the path open carries only `O_DIRECTORY` — a
    /// re-open would follow whatever was swapped in at it). `dir` must sit lexically under the
    /// path the handle was opened at; every component below rides the same `openat`/`mkdirat`
    /// discipline and the final HELD handle is returned. The staging constructors' primitive —
    /// the path-based variant remains only where no handle is yet held.
    fn create_dir_nofollow_at(&self, base: &DirHandle, dir: &Path) -> io::Result<DirHandle>;
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

    /// [`FsOps::write_staged`]'s body, with two test-only beats: `before_chmod` fires between the
    /// (`O_NOFOLLOW`) open and the `fchmod` that forces the exact mode — the seam a
    /// chmod-follows-a-swapped-symlink race would land in — and `before_write` fires between that
    /// `fchmod` and the `write_all`, the beat where a crash must already find the file at its
    /// final mode. Production passes no-ops.
    fn write_staged_hooked(
        path: &Path,
        bytes: &[u8],
        executable: bool,
        before_chmod: &dyn Fn(),
        before_write: &dyn Fn(),
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
        before_chmod();
        // `create(mode)` is masked by `umask` and ignored if the file already existed, so force
        // the exact mode — the executable bit is part of the bundle digest, so the placed bytes
        // must match it. Through the ALREADY-OPEN descriptor (`fchmod`), never the path: the
        // `O_NOFOLLOW` protection ends at the open, and a symlink swapped in at the path after it
        // would take a path-chmod onto its target instead. And BEFORE the write — the mode is an
        // invariant of the bytes, so no byte ever sits on disk at a mode that is not its own.
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        before_write();
        f.write_all(bytes)?;
        Ok(())
    }

    /// [`FsOps::write_private`]'s body — the same test-only beats as
    /// [`RealFs::write_staged_hooked`], for the 0600 secret arm.
    fn write_private_hooked(
        path: &Path,
        bytes: &[u8],
        before_chmod: &dyn Fn(),
        before_write: &dyn Fn(),
    ) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(nofollow_flag())
            .open(path)?;
        before_chmod();
        // `mode(0o600)` is masked by `umask` and ignored if the file already existed, so force
        // 0600 — a secret must never have a group/other-accessible window. Through the
        // ALREADY-OPEN descriptor (`fchmod`), never the path (same reasoning as the staged arm:
        // a path-chmod after the open can be re-aimed by a swapped-in symlink — here it would
        // REVOKE access on an unrelated file). And BEFORE any secret byte lands: a pre-existing
        // file at this (predictable) path keeps its own mode through the open, so writing first
        // and tightening second would leave the secret world-readable in the window — and a
        // crash inside it would leave the exposure permanent.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        before_write();
        f.write_all(bytes)?;
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
        use std::os::unix::fs::OpenOptionsExt;
        let base_flags = rustix::fs::OFlags::DIRECTORY.bits() as i32;
        let cur = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(base_flags)
            .open(base)?;
        Self::create_dir_walk_from(cur, base, dir, observe)
    }

    /// [`RealFs::create_dir_walk`] whose base is an already-HELD handle (the
    /// [`FsOps::create_dir_nofollow_at`] body): only the base ACQUISITION differs — the walk
    /// starts from a dup of the held fd, so the base pathname is never re-resolved.
    fn create_dir_walk_at(
        base: &DirHandle,
        dir: &Path,
        observe: &dyn Fn(&Path),
    ) -> io::Result<DirHandle> {
        Self::create_dir_walk_from(base.file.try_clone()?, &base.path, dir, observe)
    }

    /// The shared component loop behind both walks: `cur` is the already-open base fd, `base`
    /// its proof-time path (the lexical anchor `dir` must sit under).
    fn create_dir_walk_from(
        cur: File,
        base: &Path,
        dir: &Path,
        observe: &dyn Fn(&Path),
    ) -> io::Result<DirHandle> {
        use std::os::unix::fs::MetadataExt;
        use std::path::Component;
        let rel = dir.strip_prefix(base).map_err(|_| {
            io::Error::other(format!(
                "{} is not under {}; refusing the no-follow create",
                dir.display(),
                base.display()
            ))
        })?;
        let mut cur = cur;
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

    /// The fd-anchored no-replace probe the rename landings run: an entry already at `to`
    /// (`statat` at the held fd, no-follow) refuses `AlreadyExists`; only a clean NOENT proceeds.
    fn probe_no_replace(h: &DirHandle, to: &str) -> io::Result<()> {
        match rustix::fs::statat(&h.file, to, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "target exists",
            )),
            Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(e) => Err(io::Error::from(e)),
        }
    }

    /// The bare exchange syscall (`RENAME_EXCHANGE` on Linux / `RENAME_SWAP` via `renameatx_np`
    /// on macOS) at the held fd — the one cfg-gated body both exchange ops end on, with no check
    /// between the caller's last proof and the syscall itself.
    fn exchange_raw(h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
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
            let _ = (h, a, b);
            Err(io::Error::from(rustix::io::Errno::NOSYS))
        }
    }

    /// [`FsOps::rename_at_noreplace_src`]'s body, ordered so the LEAF-identity proof is the last
    /// operation before the rename syscall: the parent re-proof and the no-replace probe run
    /// FIRST, `before_leaf_verify` (a test-only beat; production passes a no-op) fires between
    /// them and [`DirHandle::verify_leaf_is`], and nothing runs between that proof and the
    /// `renameat` — the remaining proof-to-rename syscall pair is the module doc's named
    /// residual.
    fn rename_at_noreplace_src_hooked(
        h: &DirHandle,
        from: &str,
        to: &str,
        src: &DirHandle,
        before_leaf_verify: &dyn Fn(),
    ) -> io::Result<()> {
        h.verify_unmoved()?;
        Self::probe_no_replace(h, to)?;
        before_leaf_verify();
        src.verify_leaf_is(h, from)?;
        rustix::fs::renameat(&h.file, from, &h.file, to).map_err(io::Error::from)
    }

    /// [`FsOps::exchange_at_src`]'s body — the same ordering discipline as
    /// [`RealFs::rename_at_noreplace_src_hooked`]: the parent re-proof first, the test-only
    /// beat, then the leaf-identity proof immediately followed by the exchange syscall.
    fn exchange_at_src_hooked(
        h: &DirHandle,
        a: &str,
        b: &str,
        src: &DirHandle,
        before_leaf_verify: &dyn Fn(),
    ) -> io::Result<()> {
        h.verify_unmoved()?;
        before_leaf_verify();
        src.verify_leaf_is(h, a)?;
        Self::exchange_raw(h, a, b)
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
        Self::write_staged_hooked(path, bytes, executable, &|| {}, &|| {})
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        Self::write_private_hooked(path, bytes, &|| {}, &|| {})
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

    fn rename_at_noreplace_src(
        &self,
        h: &DirHandle,
        from: &str,
        to: &str,
        src: &DirHandle,
    ) -> io::Result<()> {
        // Parent re-proof + no-replace probe FIRST; the staged SOURCE's identity is the LAST
        // operation before the rename syscall (the hooked body; production passes no beat).
        Self::rename_at_noreplace_src_hooked(h, from, to, src, &|| {})
    }

    fn exchange_at_src(&self, h: &DirHandle, a: &str, b: &str, src: &DirHandle) -> io::Result<()> {
        // Parent re-proof FIRST; the staged SOURCE's identity is the LAST operation before the
        // exchange syscall (the hooked body; production passes no beat).
        Self::exchange_at_src_hooked(h, a, b, src, &|| {})
    }

    fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
        h.verify_unmoved()?;
        Self::exchange_raw(h, a, b)
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

    fn write_staged_at(
        &self,
        h: &DirHandle,
        leaf: &str,
        bytes: &[u8],
        executable: bool,
    ) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        h.verify_unmoved()?;
        let mode: u32 = if executable { 0o755 } else { 0o644 };
        let flags = rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let fd = rustix::fs::openat(
            &h.file,
            leaf,
            flags,
            // The raw-mode literal (the platform RawMode type — u16 on some libc backends); the
            // EXACT `mode` is forced through the fd below regardless.
            rustix::fs::Mode::from_bits_truncate(if executable { 0o755 } else { 0o644 }),
        )
        .map_err(io::Error::from)?;
        let mut f = File::from(fd);
        // The EXACT mode through the open descriptor, BEFORE any byte lands (the create mode is
        // masked by `umask`; the executable bit is part of the consent-bound digest).
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(bytes)?;
        Self::fsync_handle(&f)?;
        // The directory ENTRY, through the held fd (a plain fsync persists entries).
        rustix::fs::fsync(&h.file).map_err(io::Error::from)
    }

    fn rename_file_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        {
            use rustix::io::Errno;
            match rustix::fs::renameat_with(
                rustix::fs::CWD,
                from,
                rustix::fs::CWD,
                to,
                rustix::fs::RenameFlags::NOREPLACE,
            ) {
                Ok(()) => return Ok(()),
                Err(Errno::EXIST) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "target exists",
                    ));
                }
                // The kernel/filesystem does not speak RENAME_NOREPLACE — fall through to the
                // link+unlink dance, no-replace by hard-link exclusivity.
                Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {}
                Err(e) => return Err(io::Error::from(e)),
            }
        }
        // `link(2)` refuses an existing target atomically (EEXIST); the temp name is then
        // dropped. A crash between the two leaves both names — recovery's temp sweep clears it.
        std::fs::hard_link(from, to)?;
        std::fs::remove_file(from)?;
        Ok(())
    }

    fn create_dir_nofollow(&self, base: &Path, dir: &Path) -> io::Result<DirHandle> {
        Self::create_dir_walk(base, dir, &|_| {})
    }

    fn create_dir_nofollow_at(&self, base: &DirHandle, dir: &Path) -> io::Result<DirHandle> {
        Self::create_dir_walk_at(base, dir, &|_| {})
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

    /// P1 (SECURITY): a secret written to a PRE-EXISTING permissive file must be private BEFORE
    /// the first byte lands — the `mode(0o600)` on the open is ignored for an existing file, so
    /// only the fd-chmod running ahead of the write closes the window. The hook sits between
    /// that chmod and the `write_all` (a crash at this exact beat): the mode is already 0600 and
    /// no secret byte is on disk yet.
    #[test]
    fn a_preexisting_permissive_temp_is_0600_before_the_first_secret_byte_lands() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = scratch("prechmod");
        let target = dir.join("sessions.json.tmp");
        // The PREDICTABLE temp already exists, world-readable (stale litter — or a plant).
        std::fs::write(&target, b"stale, wide open").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let probe = target.clone();
        let fs = HookFs::before_write_of(&target, move || {
            assert_eq!(
                mode_of(&probe),
                0o600,
                "the mode must already be 0600 before any secret byte lands"
            );
            assert_eq!(
                std::fs::read(&probe).unwrap(),
                b"",
                "truncated and still empty — a crash here exposes nothing"
            );
            flag.store(true, Ordering::Relaxed);
        });
        fs.write_private(&target, b"secret").unwrap();
        assert!(fired.load(Ordering::Relaxed), "the before-write beat ran");
        assert_eq!(mode_of(&target), 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fd-anchored staging write: the exact mode on a fresh file, and a TRUE exclusive —
    /// an entry already at the leaf (a swapped-in symlink included) is `AlreadyExists`, never
    /// truncated through onto its target.
    #[test]
    fn write_staged_at_forces_the_exact_mode_and_refuses_an_existing_entry() {
        let dir = scratch("staged-at");
        let fs = RealFs;
        let h = fs.open_dir_handle(&dir).unwrap();
        fs.write_staged_at(&h, "run.sh", b"#!/bin/sh\n", true)
            .unwrap();
        assert_eq!(mode_of(&dir.join("run.sh")), 0o755);
        assert_eq!(std::fs::read(dir.join("run.sh")).unwrap(), b"#!/bin/sh\n");
        let vdir = scratch("staged-at-victim");
        let victim = vdir.join("secret");
        std::fs::write(&victim, b"private\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("link.md")).unwrap();
        let err = fs
            .write_staged_at(&h, "link.md", b"payload\n", false)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err:?}");
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"private\n",
            "nothing written through the link"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&vdir);
    }

    /// The birth-landing primitive: an EXISTING target refuses `AlreadyExists` with both files
    /// untouched; an absent one takes the rename.
    #[test]
    fn rename_file_noreplace_refuses_an_existing_target_and_lands_on_an_absent_one() {
        let dir = scratch("rfn");
        let from = dir.join("a.tmp");
        let to = dir.join("a");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"outside").unwrap();
        let err = RealFs.rename_file_noreplace(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err:?}");
        assert_eq!(
            std::fs::read(&to).unwrap(),
            b"outside",
            "nothing overwritten"
        );
        assert_eq!(
            std::fs::read(&from).unwrap(),
            b"new",
            "the source still stands"
        );
        std::fs::remove_file(&to).unwrap();
        RealFs.rename_file_noreplace(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
        let _ = std::fs::remove_dir_all(&dir);
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

    /// P1: the `_src` rename's LEAF-identity proof is the LAST operation before the syscall. The
    /// substitution lands at the in-op beat BETWEEN the parent-side checks and that proof — the
    /// practically reachable instant (everything later is a single syscall-pair residual) — and
    /// the proof refuses it: a typed leaf refusal, nothing landed at the target.
    #[test]
    fn a_stage_substituted_after_the_parent_checks_refuses_the_src_rename() {
        let dir = scratch("src-rename-sub");
        let stage = dir.join("stage");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("SKILL.md"), b"# staged\n").unwrap();
        let h = RealFs.open_dir_handle(&dir).unwrap();
        let src = RealFs.open_dir_handle(&stage).unwrap();
        let (st, aside) = (stage.clone(), dir.join("stolen"));
        let fs = HookFs::before_leaf_verify_of(&stage, move || {
            std::fs::rename(&st, &aside).unwrap();
            std::fs::create_dir(&st).unwrap();
            std::fs::write(st.join("SKILL.md"), b"# substituted\n").unwrap();
        });
        let err = fs
            .rename_at_noreplace_src(&h, "stage", "dest", &src)
            .unwrap_err();
        assert!(
            err.to_string().contains("refusing to land it"),
            "the LEAF proof (running after the beat) refused: {err}"
        );
        assert!(!dir.join("dest").exists(), "nothing landed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the beat's placement: it fires AFTER the parent re-proof. A parent PATH
    /// swapped at the beat can no longer refuse (that proof already ran), and the fd-anchored
    /// rename lands inside the REAL directory object wherever it now sits — never through the
    /// swapped-in path (the module doc's relocated-real-directory residual). Together with the
    /// test above this pins the beat between the parent verify and the leaf verify.
    #[test]
    fn the_leaf_verify_beat_fires_after_the_parent_reproof() {
        let outer = scratch("src-rename-beat");
        let parent = outer.join("parent");
        std::fs::create_dir(&parent).unwrap();
        let stage = parent.join("stage");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("SKILL.md"), b"# staged\n").unwrap();
        let h = RealFs.open_dir_handle(&parent).unwrap();
        let src = RealFs.open_dir_handle(&stage).unwrap();
        let moved = outer.join("moved");
        let (p, m) = (parent.clone(), moved.clone());
        let fs = HookFs::before_leaf_verify_of(&stage, move || {
            std::fs::rename(&p, &m).unwrap();
            std::fs::create_dir(&p).unwrap();
        });
        fs.rename_at_noreplace_src(&h, "stage", "dest", &src)
            .expect("the parent re-proof ran before the beat; the held fd lands the rename");
        assert_eq!(
            std::fs::read(moved.join("dest/SKILL.md")).unwrap(),
            b"# staged\n",
            "landed inside the real (moved) directory object"
        );
        assert_eq!(
            std::fs::read_dir(&parent).unwrap().count(),
            0,
            "nothing landed through the swapped-in path"
        );
        let _ = std::fs::remove_dir_all(&outer);
    }

    /// The exchange landing rides the same beat discipline: a substitution at the in-op beat is
    /// refused by the LAST-instant leaf proof, and the live target keeps its bytes.
    #[test]
    fn a_stage_substituted_after_the_parent_checks_refuses_the_src_exchange() {
        let dir = scratch("src-exchange-sub");
        // Probe first: not every temp FS speaks RENAME_EXCHANGE.
        std::fs::create_dir(dir.join("pa")).unwrap();
        std::fs::create_dir(dir.join("pb")).unwrap();
        let probe_h = RealFs.open_dir_handle(&dir).unwrap();
        if RealFs.exchange_at(&probe_h, "pa", "pb").is_err() {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let stage = dir.join("stage");
        let live = dir.join("live");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("SKILL.md"), b"# staged\n").unwrap();
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("SKILL.md"), b"# live\n").unwrap();
        let h = RealFs.open_dir_handle(&dir).unwrap();
        let src = RealFs.open_dir_handle(&stage).unwrap();
        let (st, aside) = (stage.clone(), dir.join("stolen"));
        let fs = HookFs::before_leaf_verify_of(&stage, move || {
            std::fs::rename(&st, &aside).unwrap();
            std::fs::create_dir(&st).unwrap();
            std::fs::write(st.join("SKILL.md"), b"# substituted\n").unwrap();
        });
        let err = fs.exchange_at_src(&h, "stage", "live", &src).unwrap_err();
        assert!(
            err.to_string().contains("refusing to land it"),
            "the LEAF proof (running after the beat) refused: {err}"
        );
        assert_eq!(
            std::fs::read(live.join("SKILL.md")).unwrap(),
            b"# live\n",
            "the live target never swapped"
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
        /// Inside a `_src` landing ([`FsOps::rename_at_noreplace_src`] /
        /// [`FsOps::exchange_at_src`]) of ONE named source dir: between the parent-side checks
        /// (the parent re-proof + any no-replace probe) and the LAST-instant leaf-identity
        /// proof — the sharpest reachable beat before the landing, where a substituted stage
        /// must still be refused by the proof that follows (the beat AFTER that proof is the
        /// module doc's named syscall-pair residual, deliberately hook-free).
        BeforeLeafVerifyOf { dir: PathBuf },
        /// Between a staged/private write's (`O_NOFOLLOW`) open and the `fchmod` that forces its
        /// mode — the beat where a path-based chmod would follow a swapped-in symlink.
        BeforeChmodOf { target: PathBuf },
        /// Between that `fchmod` and the `write_all` that lands the bytes — the beat where a
        /// crash must already find the file at its final mode (a secret: 0600 before the first
        /// secret byte exists on disk).
        BeforeWriteOf { target: PathBuf },
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

        pub(crate) fn before_leaf_verify_of(dir: &Path, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::BeforeLeafVerifyOf {
                    dir: dir.to_path_buf(),
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

        pub(crate) fn before_write_of(target: &Path, hook: impl Fn() + 'a) -> Self {
            Self::with(
                Trigger::BeforeWriteOf {
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
                // Land the hook INSIDE the op, between the (O_NOFOLLOW) open and the fchmod —
                // the exact beat a path-swapped symlink would catch a path-based chmod.
                return RealFs::write_staged_hooked(
                    path,
                    bytes,
                    executable,
                    &|| self.maybe_fire(true, 1),
                    &|| {},
                );
            }
            if matches!(&self.trigger, Trigger::BeforeWriteOf { target } if target == path) {
                // Between the fd-chmod and the write — where a crash must already find the
                // final mode.
                return RealFs::write_staged_hooked(path, bytes, executable, &|| {}, &|| {
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
                return RealFs::write_private_hooked(
                    path,
                    bytes,
                    &|| self.maybe_fire(true, 1),
                    &|| {},
                );
            }
            if matches!(&self.trigger, Trigger::BeforeWriteOf { target } if target == path) {
                return RealFs::write_private_hooked(path, bytes, &|| {}, &|| {
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
        fn rename_at_noreplace_src(
            &self,
            h: &DirHandle,
            from: &str,
            to: &str,
            src: &DirHandle,
        ) -> io::Result<()> {
            // The before-move beat fires BEFORE any of the op's checks run, so a test's
            // substitution at this exact instant must still be refused.
            let joined = h.path().join(from);
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if *dir == joined),
                1,
            );
            if matches!(&self.trigger, Trigger::BeforeLeafVerifyOf { dir } if *dir == joined) {
                // Land the hook INSIDE the op, between the parent-side checks and the
                // LAST-instant leaf-identity proof — the sharpest reachable beat before the
                // landing rename.
                return RealFs::rename_at_noreplace_src_hooked(h, from, to, src, &|| {
                    self.maybe_fire(true, 1);
                });
            }
            self.inner.rename_at_noreplace_src(h, from, to, src)
        }
        fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
            let (ja, jb) = (h.path().join(a), h.path().join(b));
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if *dir == ja || *dir == jb),
                1,
            );
            self.inner.exchange_at(h, a, b)
        }
        fn exchange_at_src(
            &self,
            h: &DirHandle,
            a: &str,
            b: &str,
            src: &DirHandle,
        ) -> io::Result<()> {
            let (ja, jb) = (h.path().join(a), h.path().join(b));
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if *dir == ja || *dir == jb),
                1,
            );
            if matches!(&self.trigger, Trigger::BeforeLeafVerifyOf { dir } if *dir == ja || *dir == jb)
            {
                // Between the parent re-proof and the LAST-instant leaf-identity proof — the
                // sharpest reachable beat before the exchange.
                return RealFs::exchange_at_src_hooked(h, a, b, src, &|| {
                    self.maybe_fire(true, 1);
                });
            }
            self.inner.exchange_at_src(h, a, b, src)
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
        fn create_dir_nofollow_at(&self, base: &DirHandle, dir: &Path) -> io::Result<DirHandle> {
            // The same beats as the path-based arm: the Nth-create trigger fires BEFORE the walk
            // (so a test can swap the base pathname there and prove the held fd ignores it), and
            // the component trigger lands inside the walk.
            let (matched, nth) = match &self.trigger {
                Trigger::NthCreateDirAll { target, nth } => (target == dir, *nth),
                _ => (false, 0),
            };
            self.maybe_fire(matched, nth);
            if matches!(&self.trigger, Trigger::BeforeComponentOf { .. }) {
                return RealFs::create_dir_walk_at(base, dir, &|cur| {
                    self.maybe_fire(
                        matches!(&self.trigger, Trigger::BeforeComponentOf { target } if target == cur),
                        1,
                    );
                });
            }
            self.inner.create_dir_nofollow_at(base, dir)
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
        fn write_staged_at(
            &self,
            h: &DirHandle,
            leaf: &str,
            bytes: &[u8],
            executable: bool,
        ) -> io::Result<()> {
            // The staging-window trigger fires here exactly as on the path-based arm — a staged
            // write is a staged write, whichever primitive lands it.
            self.maybe_fire(matches!(self.trigger, Trigger::FirstStagedWrite), 1);
            self.inner.write_staged_at(h, leaf, bytes, executable)
        }
        fn rename_file_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.maybe_fire(
                matches!(&self.trigger, Trigger::FirstMoveOf { dir } if dir == from),
                1,
            );
            self.inner.rename_file_noreplace(from, to)
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
        fn rename_at_noreplace_src(
            &self,
            h: &DirHandle,
            from: &str,
            to: &str,
            src: &DirHandle,
        ) -> io::Result<()> {
            self.tick()?;
            self.inner.rename_at_noreplace_src(h, from, to, src)
        }
        fn exchange_at(&self, h: &DirHandle, a: &str, b: &str) -> io::Result<()> {
            self.tick()?;
            self.inner.exchange_at(h, a, b)
        }
        fn exchange_at_src(
            &self,
            h: &DirHandle,
            a: &str,
            b: &str,
            src: &DirHandle,
        ) -> io::Result<()> {
            self.tick()?;
            self.inner.exchange_at_src(h, a, b, src)
        }
        fn create_dir_nofollow(&self, base: &Path, dir: &Path) -> io::Result<DirHandle> {
            self.tick()?;
            self.inner.create_dir_nofollow(base, dir)
        }
        fn create_dir_nofollow_at(&self, base: &DirHandle, dir: &Path) -> io::Result<DirHandle> {
            self.tick()?;
            self.inner.create_dir_nofollow_at(base, dir)
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
        fn write_staged_at(
            &self,
            h: &DirHandle,
            leaf: &str,
            bytes: &[u8],
            executable: bool,
        ) -> io::Result<()> {
            self.tick()?;
            self.inner.write_staged_at(h, leaf, bytes, executable)
        }
        fn rename_file_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.tick()?;
            self.inner.rename_file_noreplace(from, to)
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
