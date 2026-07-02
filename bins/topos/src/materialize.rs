//! The crash-safe byte-writing materializer: place a verified bundle's exact bytes onto a harness skill
//! directory via a **namespace-atomic directory swap**, then advance the durable docs — so a crash at any
//! boundary leaves the placement holding the OLD-or-NEW *complete* bytes, never a torn, mixed, or
//! half-written tree.
//!
//! `atomic.rs` (the single-*file* crash-safe write) is unchanged; this module owns the crash-safe
//! *sequence* for a whole directory. The raw swap syscall is the [`FsOps::exchange_dir`] seam op.
//!
//! ## The order is the safety
//!
//! 1. Build a staging dir as a **sibling in the placement's PARENT** (guaranteed same filesystem) and
//!    `fsync` every staged file AND every staging directory.
//! 2. **Atomic swap** the staging dir with the placement dir ([`SwapCapability::AtomicExchange`]) — one
//!    namespace operation. (A first install renames into an absent dir; a swap-incapable FS degrades to
//!    the logged [`SwapCapability::RenameDance`] with a brief *absent*, never *mixed*, window.)
//! 3. `fsync` the parent so the swap is durable.
//! 4. Drop the old bytes the swap parked at the staging path.
//! 5. Commit the docs **map → lock → sync** ([`commit_docs`]); `applied` advances only at the final sync
//!    write, strictly after the new bytes are durably on disk.
//!
//! A fault before step 5's sync write leaves `applied` naming the OLD generation while the bytes are NEW;
//! the next pull re-derives the working hash, sees it already equals the target, and HEALS forward (the
//! kernel `sync::refine_after_fetch` `AlreadyAtTarget` path) rather than mistaking the bytes for a draft.
//! The per-skill writer flock (held by the caller, living OUTSIDE the swapped dir) serializes topos
//! writers across the whole sequence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use topos_gitstore::{FileMode, RenderedBundle};
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, PathKind};
use crate::sidecar::SkillPaths;

/// The placement-map fields the ENGINE computes (the materializer fills in `pre_existing_sha` +
/// `swap_capability`, which it alone derives).
#[derive(Debug, Clone)]
pub(crate) struct NextMapCore {
    pub placements: Vec<String>,
    pub applied_commit: String,
    pub materialized_sha: String,
    pub harness: Option<topos_types::HarnessId>,
    pub harness_layer: Option<String>,
}

/// Everything the materializer needs for one apply. The engine has already fetched + `render_verified`'d
/// `bundle` (so the bytes are authenticated) and computed the complete `next_lock` + `next_sync` target.
pub(crate) struct MaterializeReq<'a> {
    /// The stable skill id (names the staging / graveyard / probe siblings).
    pub skill_id: &'a str,
    /// The placement directory (possibly a symlink; canonicalized to its real dir inside).
    pub placement_dir: &'a Path,
    /// The verified bytes to place.
    pub bundle: &'a RenderedBundle,
    /// The durable prior map (the swap-capability cache + the `pre_existing_sha` derivation source).
    pub prior_map: &'a PlacementMap,
    /// The map fields the engine computed (the materializer adds `pre_existing_sha` + `swap_capability`).
    pub next_map_core: NextMapCore,
    /// The lock to write (built by the engine from the bundle — kept in step with the placed bytes).
    pub next_lock: &'a Lock,
    /// The complete target sync state (the engine computed `applied`/`base_commit`/`work_hash`/…).
    pub next_sync: &'a SyncState,
    /// Where the three durable docs live.
    pub sp: &'a SkillPaths,
}

/// What the materializer actually did (so the engine can log / record the effective capability).
#[derive(Debug, Clone)]
pub(crate) struct MaterializeReport {
    pub swap_capability: SwapCapability,
    pub pre_existing_sha: Option<String>,
}

/// The sha of whatever was in the placement dir BEFORE topos first wrote into it — restored on
/// uninstall. **Sticky:** once captured it never changes. On the first overwrite (`prior` has no
/// `pre_existing_sha` yet but a directory was present) the prior `materialized_sha` *is* the user's
/// original bytes (adopt-in-place wrote nothing into the dir, so the recorded sha equals what is there).
/// A genuine first install into an absent dir has nothing pre-existing.
///
/// Shared by [`materialize`] and the engine's heal path so a crash-after-swap reconciliation cannot lose
/// the first-overwrite capture. Computed from the DURABLE prior map, never from post-swap disk.
pub(crate) fn derive_pre_existing_sha(
    prior: &PlacementMap,
    dir_was_present: bool,
) -> Option<String> {
    if let Some(existing) = &prior.pre_existing_sha {
        Some(existing.clone())
    } else if dir_was_present {
        Some(prior.materialized_sha.clone())
    } else {
        None
    }
}

/// Write the three durable docs in the load-bearing order **map → lock → sync**.
///
/// ORDER IS LOAD-BEARING. An apply mutates these docs IN PLACE (there is no enclosing staging-dir rename,
/// unlike `add`, whose whole-directory publish rename makes its internal doc order crash-irrelevant).
/// `sync.json` is the COMMIT POINT: `applied` advances only here, and only after the new bytes are durably
/// swapped onto disk. `map` + `lock` are written FIRST so a crash between any two leaves `applied` still
/// naming the OLD generation — the next pull re-derives the working hash, sees it equals the target, and
/// HEALS forward instead of mistaking the new bytes for a draft. Were `sync` written first, a crash before
/// `map` would leave `applied` current while `map.applied_commit` / `lock` stayed stale forever (uninstall
/// and go-back would then restore the wrong bytes).
///
/// Each individual write is itself crash-safe (atomic temp → fsync → rename → fsync-dir).
pub(crate) fn commit_docs(
    fs: &dyn FsOps,
    sp: &SkillPaths,
    next_map: &PlacementMap,
    next_lock: &Lock,
    next_sync: &SyncState,
) -> Result<(), ClientError> {
    doc::write_doc(fs, &sp.map, next_map)?;
    doc::write_doc(fs, &sp.lock, next_lock)?;
    doc::write_doc(fs, &sp.sync, next_sync)?;
    Ok(())
}

/// Materialize `req.bundle`'s bytes onto the placement and commit the docs. Returns the effective
/// capability + the recorded prior-bytes sha.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] if the placement is a non-directory, an unresolvable symlink, or
/// on a filesystem with no safe swap; otherwise the underlying [`FsOps`] failure (which the crash gate
/// injects). On any error `applied` has NOT advanced (the sync write is the last, all-or-nothing step).
pub(crate) fn materialize(
    fs: &dyn FsOps,
    req: &MaterializeReq<'_>,
) -> Result<MaterializeReport, ClientError> {
    let kind = fs.path_kind(req.placement_dir)?;
    let target = resolve_target(fs, req.placement_dir, kind)?;
    let parent = target.parent.clone();

    // Clear any leftover litter from a prior crashed apply of THIS skill (under the caller's flock).
    cleanup_litter(fs, &parent, req.skill_id)?;

    // Trust the cached capability; probe only a genesis `Unsupported` placeholder.
    let mut cap = req.prior_map.swap_capability;
    if cap == SwapCapability::Unsupported {
        cap = probe_capability(fs, &parent, req.skill_id)?;
    }

    // Build + fsync the staging dir (a same-filesystem sibling of the placement).
    let staging = staging_path(&parent, req.skill_id);
    build_staging(fs, &staging, req.bundle)?;

    // Derive the prior-bytes sha from the DURABLE prior map (never from post-swap disk).
    let pre_existing_sha = derive_pre_existing_sha(req.prior_map, target.dir_was_present);

    // Place the bytes.
    if target.dir_was_present {
        cap = place_update(fs, &staging, &target.dir, &parent, req.skill_id, cap)?;
    } else {
        // First install: an atomic create — no prior bytes to mix.
        fs.rename_dir_noreplace(&staging, &target.dir)
            .map_err(|e| ClientError::Io(format!("first-install rename: {e}")))?;
        fs.fsync_dir(&parent)?;
    }

    // Commit map → lock → sync (the commit point; `applied` advances only here).
    let next_map = PlacementMap {
        schema_version: PERSISTED_SCHEMA_VERSION,
        placements: req.next_map_core.placements.clone(),
        applied_commit: req.next_map_core.applied_commit.clone(),
        materialized_sha: req.next_map_core.materialized_sha.clone(),
        pre_existing_sha: pre_existing_sha.clone(),
        swap_capability: cap,
        harness: req.next_map_core.harness,
        harness_layer: req.next_map_core.harness_layer.clone(),
    };
    commit_docs(fs, req.sp, &next_map, req.next_lock, req.next_sync)?;

    Ok(MaterializeReport {
        swap_capability: cap,
        pre_existing_sha,
    })
}

/// The resolved placement target.
struct Target {
    /// The real directory to swap (a symlink placement is canonicalized to its target).
    dir: PathBuf,
    /// The directory's parent (the same-filesystem home for the staging sibling).
    parent: PathBuf,
    /// Whether a directory was present before this apply (drives swap-vs-first-install + `pre_existing_sha`).
    dir_was_present: bool,
}

/// Resolve the placement to a real directory, or detect a first install, or refuse a non-directory.
fn resolve_target(
    fs: &dyn FsOps,
    placement_dir: &Path,
    kind: Option<PathKind>,
) -> Result<Target, ClientError> {
    match kind {
        None => {
            // First install: canonicalize the PARENT (must exist) so ancestor symlinks resolve, then
            // re-join the leaf. Create the parent if absent (the harness skills dir may not exist yet).
            let parent_raw =
                placement_dir
                    .parent()
                    .ok_or_else(|| ClientError::PlacementUnsupported {
                        reason: "placement path has no parent directory".into(),
                    })?;
            fs.create_dir_all(parent_raw)?;
            let parent = std::fs::canonicalize(parent_raw)
                .map_err(|e| ClientError::Io(format!("canonicalize placement parent: {e}")))?;
            let leaf =
                placement_dir
                    .file_name()
                    .ok_or_else(|| ClientError::PlacementUnsupported {
                        reason: "placement path has no final component".into(),
                    })?;
            Ok(Target {
                dir: parent.join(leaf),
                parent,
                dir_was_present: false,
            })
        }
        Some(PathKind::Dir) | Some(PathKind::Symlink) => {
            // Canonicalize (resolving a symlink placement to its real directory) and operate THERE, so
            // the swap replaces the directory's contents, never the symlink itself.
            let dir = std::fs::canonicalize(placement_dir)
                .map_err(|e| ClientError::Io(format!("canonicalize placement: {e}")))?;
            if !dir.is_dir() {
                return Err(ClientError::PlacementUnsupported {
                    reason: "placement resolves to a non-directory".into(),
                });
            }
            let parent = dir
                .parent()
                .ok_or_else(|| ClientError::PlacementUnsupported {
                    reason: "placement directory has no parent".into(),
                })?
                .to_path_buf();
            Ok(Target {
                dir,
                parent,
                dir_was_present: true,
            })
        }
        Some(PathKind::Other) => Err(ClientError::PlacementUnsupported {
            reason: "a non-directory file occupies the placement path".into(),
        }),
    }
}

/// Place new bytes over an existing directory, self-healing a stale `AtomicExchange` to `RenameDance`.
fn place_update(
    fs: &dyn FsOps,
    staging: &Path,
    dir: &Path,
    parent: &Path,
    skill_id: &str,
    cap: SwapCapability,
) -> Result<SwapCapability, ClientError> {
    match cap {
        SwapCapability::AtomicExchange => match fs.exchange_dir(staging, dir) {
            Ok(()) => {
                fs.fsync_dir(parent)?;
                // The swap parked the OLD bytes at the staging path; drop them.
                fs.remove_dir_all(staging)?;
                Ok(SwapCapability::AtomicExchange)
            }
            Err(e) if is_unsupported(&e) => {
                // The cached capability is stale (the placement moved onto a swap-incapable FS). Fall
                // back to the rename-dance, reusing the already-built staging.
                do_dance(fs, staging, dir, parent, skill_id)?;
                Ok(SwapCapability::RenameDance)
            }
            Err(e) => Err(ClientError::Io(format!("atomic directory swap: {e}"))),
        },
        SwapCapability::RenameDance => {
            do_dance(fs, staging, dir, parent, skill_id)?;
            Ok(SwapCapability::RenameDance)
        }
        SwapCapability::Unsupported => Err(ClientError::PlacementUnsupported {
            reason: "no safe directory swap on this filesystem".into(),
        }),
    }
}

/// The degraded fallback when no atomic swap exists: park the old dir, move the new in, drop the old.
/// Each `rename` is atomic, so the dir is never *mixed*; between the two renames it is briefly **absent**
/// (the named, logged residual). A crash in that window leaves the dir absent → the next pull takes the
/// first-install branch and restores the new bytes (the old version is still in the sidecar store).
fn do_dance(
    fs: &dyn FsOps,
    staging: &Path,
    dir: &Path,
    parent: &Path,
    skill_id: &str,
) -> Result<(), ClientError> {
    let graveyard = graveyard_path(parent, skill_id);
    fs.remove_dir_all(&graveyard)?;
    fs.rename(dir, &graveyard)
        .map_err(|e| ClientError::Io(format!("rename-dance park old: {e}")))?;
    // --- the brief ABSENT (never mixed) window is between these two atomic renames ---
    fs.rename(staging, dir)
        .map_err(|e| ClientError::Io(format!("rename-dance install new: {e}")))?;
    fs.fsync_dir(parent)?;
    fs.remove_dir_all(&graveyard)?;
    Ok(())
}

/// Build a fresh staging dir holding the bundle's exact bytes, fsync every file AND every staging dir.
fn build_staging(
    fs: &dyn FsOps,
    staging: &Path,
    bundle: &RenderedBundle,
) -> Result<(), ClientError> {
    fs.remove_dir_all(staging)?;
    fs.create_dir_all(staging)?;
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    dirs.insert(staging.to_path_buf());
    for f in &bundle.files {
        let dest = staging.join(&f.path);
        if let Some(file_parent) = dest.parent() {
            fs.create_dir_all(file_parent)?;
            // Collect every directory from the file's parent up to (and including) the staging root, so
            // each directory entry is fsynced before the swap. `f.path` is kernel-validated (no `..`, no
            // absolute), so the walk stays inside staging.
            let mut d: &Path = file_parent;
            loop {
                dirs.insert(d.to_path_buf());
                if d == staging {
                    break;
                }
                match d.parent() {
                    Some(up) if up == staging || up.starts_with(staging) => d = up,
                    _ => break,
                }
            }
        }
        fs.write_staged(&dest, &f.bytes, f.mode == FileMode::Executable)?;
        fs.fsync_file(&dest)?;
    }
    for d in &dirs {
        fs.fsync_dir(d)?;
    }
    Ok(())
}

/// Remove any leftover staging / graveyard / probe siblings of THIS skill (idempotent, NotFound-tolerant).
fn cleanup_litter(fs: &dyn FsOps, parent: &Path, skill_id: &str) -> Result<(), ClientError> {
    fs.remove_dir_all(&staging_path(parent, skill_id))?;
    fs.remove_dir_all(&graveyard_path(parent, skill_id))?;
    fs.remove_dir_all(&probe_path(parent, skill_id, 'a'))?;
    fs.remove_dir_all(&probe_path(parent, skill_id, 'b'))?;
    Ok(())
}

/// Probe the placement's filesystem ONCE for an atomic directory swap, by exchanging two throwaway
/// sibling directories. Any failure (the syscall is unsupported, or anything else) means "no atomic swap"
/// → degrade to the rename-dance. Self-cleaning.
fn probe_capability(
    fs: &dyn FsOps,
    parent: &Path,
    skill_id: &str,
) -> Result<SwapCapability, ClientError> {
    let a = probe_path(parent, skill_id, 'a');
    let b = probe_path(parent, skill_id, 'b');
    fs.remove_dir_all(&a)?;
    fs.remove_dir_all(&b)?;
    fs.create_dir_all(&a)?;
    fs.create_dir_all(&b)?;
    let supported = fs.exchange_dir(&a, &b).is_ok();
    fs.remove_dir_all(&a)?;
    fs.remove_dir_all(&b)?;
    Ok(if supported {
        SwapCapability::AtomicExchange
    } else {
        SwapCapability::RenameDance
    })
}

/// Whether an error means the atomic swap syscall is unavailable on this filesystem (so fall back),
/// versus a real I/O failure (so propagate). The unsupported set: `ENOTSUP`/`EOPNOTSUPP` (FS without
/// `RENAME_EXCHANGE`/`RENAME_SWAP`), `EINVAL` (flag unsupported), `ENOSYS` (syscall absent).
fn is_unsupported(err: &std::io::Error) -> bool {
    use rustix::io::Errno;
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    [Errno::NOTSUP, Errno::OPNOTSUPP, Errno::INVAL, Errno::NOSYS]
        .iter()
        .any(|e| std::io::Error::from(*e).raw_os_error() == Some(code))
}

fn staging_path(parent: &Path, skill_id: &str) -> PathBuf {
    parent.join(format!(".topos-staging-{skill_id}"))
}
fn graveyard_path(parent: &Path, skill_id: &str) -> PathBuf {
    parent.join(format!(".topos-old-{skill_id}"))
}
fn probe_path(parent: &Path, skill_id: &str, slot: char) -> PathBuf {
    parent.join(format!(".topos-probe-{skill_id}-{slot}"))
}

/// The placement-side `.topos-*` siblings this materializer may create for a skill (staging / graveyard /
/// probe dirs). Exposed so crash recovery can sweep them beside the placement — outside `~/.topos/` — even
/// when the next command is not another pull of this skill, so they are never orphaned by `uninstall`.
pub(crate) fn litter_siblings(parent: &Path, skill_id: &str) -> Vec<PathBuf> {
    vec![
        staging_path(parent, skill_id),
        graveyard_path(parent, skill_id),
        probe_path(parent, skill_id, 'a'),
        probe_path(parent, skill_id, 'b'),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic::load_versioned;
    use crate::fs_seam::{FaultFs, FsOps, RealFs};
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_core::digest::{self, FileMode, ManifestEntry};
    use topos_gitstore::RenderedFile;
    use topos_types::Generation;
    use topos_types::persisted::{LockedFile, RecordedTuple};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-mat-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rendered(files: &[(&str, FileMode, &[u8])]) -> RenderedBundle {
        let rf: Vec<RenderedFile> = files
            .iter()
            .map(|(p, m, b)| RenderedFile {
                path: (*p).to_owned(),
                mode: *m,
                bytes: b.to_vec(),
                content_sha256: digest::sha256(b),
            })
            .collect();
        let entries: Vec<ManifestEntry> = rf
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: f.mode,
                content_sha256: f.content_sha256,
            })
            .collect();
        RenderedBundle {
            files: rf,
            bundle_digest: digest::bundle_digest(&entries).unwrap(),
        }
    }

    fn digest_hex(files: &[(&str, FileMode, &[u8])]) -> String {
        digest::to_hex(&rendered(files).bundle_digest)
    }

    /// Read a placement dir into a sorted (rel-path, bytes) list, or `None` if absent. `..tmp` files left
    /// by a faulted atomic write are ignored (they are not part of the placed tree).
    fn dir_snapshot(dir: &Path) -> Option<Vec<(String, Vec<u8>)>> {
        if !dir.exists() {
            return None;
        }
        let mut out = Vec::new();
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                    out.push((rel, std::fs::read(&p).unwrap()));
                }
            }
        }
        walk(dir, dir, &mut out);
        out.sort();
        Some(out)
    }

    fn expected(files: &[(&str, FileMode, &[u8])]) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(p, _, b)| ((*p).to_owned(), b.to_vec()))
            .collect();
        v.sort();
        v
    }

    struct Docs {
        sp: SkillPaths,
        _home: PathBuf,
    }
    fn docs_under(home: &Path, id: &str) -> Docs {
        let id = crate::id::SkillId::parse(id).expect("fixture skill id is charset-clean");
        let sp = crate::sidecar::Layout::new(home).published(&id);
        std::fs::create_dir_all(sp.lock.parent().unwrap()).unwrap();
        Docs {
            sp,
            _home: home.to_path_buf(),
        }
    }

    fn lock_of(id: &str, files: &[(&str, FileMode, &[u8])], base: &str) -> Lock {
        Lock {
            schema_version: 1,
            skill_id: id.to_owned(),
            name: "demo".into(),
            base_commit: base.to_owned(),
            bundle_digest: digest_hex(files),
            files: files
                .iter()
                .map(|(p, m, b)| LockedFile {
                    path: (*p).to_owned(),
                    mode: m.as_str().to_owned(),
                    sha256: digest::to_hex(&digest::sha256(b)),
                    size: b.len() as u64,
                })
                .collect(),
        }
    }

    fn sync_at(applied: Generation, observed: Generation, base: &str, work: &str) -> SyncState {
        SyncState {
            schema_version: 1,
            observed,
            applied,
            recorded: vec![RecordedTuple {
                generation: observed,
                commit_id: base.to_owned(),
            }],
            base_commit: base.to_owned(),
            work_hash: work.to_owned(),
            held: false,
        }
    }

    fn prior_map(placement: &str, materialized: &str, cap: SwapCapability) -> PlacementMap {
        PlacementMap {
            schema_version: 1,
            placements: vec![placement.to_owned()],
            applied_commit: "0".repeat(64),
            materialized_sha: materialized.to_owned(),
            pre_existing_sha: None,
            swap_capability: cap,
            harness: None,
            harness_layer: None,
        }
    }

    /// Does the temp filesystem support the atomic dir exchange? (APFS/ext4 do; some do not.)
    fn swap_supported(parent: &Path) -> bool {
        let a = parent.join(".swcheck-a");
        let b = parent.join(".swcheck-b");
        let _ = std::fs::create_dir_all(&a);
        let _ = std::fs::create_dir_all(&b);
        let ok = RealFs.exchange_dir(&a, &b).is_ok();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        ok
    }

    const NEW: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# new\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho new\n"),
        ("ref/data.txt", FileMode::Regular, b"nested new\n"),
    ];
    const OLD: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# old\n"),
        ("legacy.txt", FileMode::Regular, b"only in old\n"),
    ];

    fn install_old(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for (p, _, b) in OLD {
            let dest = dir.join(p);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, b).unwrap();
        }
    }

    fn req<'a>(
        skill_id: &'a str,
        placement: &'a Path,
        bundle: &'a RenderedBundle,
        prior: &'a PlacementMap,
        next_lock: &'a Lock,
        next_sync: &'a SyncState,
        sp: &'a SkillPaths,
    ) -> MaterializeReq<'a> {
        MaterializeReq {
            skill_id,
            placement_dir: placement,
            bundle,
            prior_map: prior,
            next_map_core: NextMapCore {
                placements: vec![placement.to_string_lossy().into_owned()],
                applied_commit: "1".repeat(64),
                materialized_sha: digest_hex(NEW),
                harness: None,
                harness_layer: None,
            },
            next_lock,
            next_sync,
            sp,
        }
    }

    #[test]
    fn first_install_places_exact_bytes_and_modes() {
        let parent = Scratch::new("first");
        let home = Scratch::new("first-home");
        let placement = parent.0.join("demo"); // absent
        let bundle = rendered(NEW);
        let lock = lock_of("topos_first", NEW, &"1".repeat(64));
        let g = Generation { epoch: 1, seq: 1 };
        let sync = sync_at(g, g, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &placement.to_string_lossy(),
            &"0".repeat(64),
            SwapCapability::Unsupported,
        );
        let d = docs_under(&home.0, "topos_first");

        let report = materialize(
            &RealFs,
            &req(
                "topos_first",
                &placement,
                &bundle,
                &prior,
                &lock,
                &sync,
                &d.sp,
            ),
        )
        .unwrap();

        assert_eq!(dir_snapshot(&placement), Some(expected(NEW)));
        // Executable bit survived (it is part of the consent-bound digest).
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(placement.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "run.sh must stay executable");
        // First install has no pre-existing bytes.
        assert!(report.pre_existing_sha.is_none());
        // Docs committed.
        let written: SyncState = load_versioned(&std::fs::read(&d.sp.sync).unwrap(), 1).unwrap();
        assert_eq!(written.applied, g);
    }

    #[test]
    fn update_swaps_to_new_and_records_pre_existing() {
        let parent = Scratch::new("upd");
        let home = Scratch::new("upd-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let placement = parent.0.join("demo");
        install_old(&placement);
        let bundle = rendered(NEW);
        let lock = lock_of("topos_upd", NEW, &"1".repeat(64));
        let g0 = Generation { epoch: 1, seq: 1 };
        let g1 = Generation { epoch: 1, seq: 2 };
        let sync = sync_at(g1, g1, &"1".repeat(64), &digest_hex(NEW));
        // prior map mimics `add`: pre_existing None, materialized = the adopted (old) bytes.
        let prior = prior_map(
            &placement.to_string_lossy(),
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let _ = g0;
        let d = docs_under(&home.0, "topos_upd");

        let report = materialize(
            &RealFs,
            &req(
                "topos_upd",
                &placement,
                &bundle,
                &prior,
                &lock,
                &sync,
                &d.sp,
            ),
        )
        .unwrap();

        assert_eq!(
            dir_snapshot(&placement),
            Some(expected(NEW)),
            "new bytes placed"
        );
        // The first overwrite captured the adopted (old) bytes as pre-existing.
        assert_eq!(
            report.pre_existing_sha.as_deref(),
            Some(digest_hex(OLD).as_str())
        );
        // No old-bytes staging litter left behind.
        assert!(!staging_path(&parent.0, "topos_upd").exists());
    }

    #[test]
    fn refuses_a_non_directory_placement() {
        let parent = Scratch::new("file");
        let home = Scratch::new("file-home");
        let placement = parent.0.join("demo");
        std::fs::write(&placement, b"i am a file").unwrap(); // Other, not a dir
        let bundle = rendered(NEW);
        let lock = lock_of("topos_file", NEW, &"1".repeat(64));
        let g = Generation { epoch: 1, seq: 1 };
        let sync = sync_at(g, g, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &placement.to_string_lossy(),
            &"0".repeat(64),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_file");

        let err = materialize(
            &RealFs,
            &req(
                "topos_file",
                &placement,
                &bundle,
                &prior,
                &lock,
                &sync,
                &d.sp,
            ),
        )
        .unwrap_err();
        assert!(matches!(err, ClientError::PlacementUnsupported { .. }));
        // The user's file is untouched.
        assert_eq!(std::fs::read(&placement).unwrap(), b"i am a file");
    }

    #[test]
    fn symlink_placement_updates_the_real_target() {
        let parent = Scratch::new("link");
        let home = Scratch::new("link-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let real = parent.0.join("real");
        install_old(&real);
        let placement = parent.0.join("demo");
        std::os::unix::fs::symlink(&real, &placement).unwrap();
        let bundle = rendered(NEW);
        let lock = lock_of("topos_link", NEW, &"1".repeat(64));
        let g = Generation { epoch: 1, seq: 2 };
        let sync = sync_at(g, g, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &placement.to_string_lossy(),
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_link");

        materialize(
            &RealFs,
            &req(
                "topos_link",
                &placement,
                &bundle,
                &prior,
                &lock,
                &sync,
                &d.sp,
            ),
        )
        .unwrap();

        // The symlink still points at `real`, which now holds the new bytes (the link was not replaced).
        assert!(
            placement
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(dir_snapshot(&real), Some(expected(NEW)));
    }

    /// The release-blocker crash gate: fault every materialize boundary and assert the placement holds
    /// the OLD-or-NEW *complete* bytes (never torn/mixed), and `applied` advances ONLY once the bytes are
    /// new and all docs are written. A clean re-run converges.
    #[test]
    fn crash_gate_atomic_exchange_leaves_old_or_new_complete() {
        let probe = Scratch::new("probe");
        if !swap_supported(&probe.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let g_old = Generation { epoch: 1, seq: 1 };
        let g_new = Generation { epoch: 1, seq: 2 };
        let new_digest = digest_hex(NEW);

        // Size the sweep from a clean run.
        let n_ops = {
            let parent = Scratch::new("cg-count");
            let home = Scratch::new("cg-count-home");
            let placement = parent.0.join("demo");
            install_old(&placement);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_cg", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &placement.to_string_lossy(),
                &digest_hex(OLD),
                SwapCapability::AtomicExchange,
            );
            let d = docs_under(&home.0, "topos_cg");
            let fs = FaultFs::new(0);
            materialize(
                &fs,
                &req("topos_cg", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            fs.ops_attempted()
        };
        assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

        for fail_at in 1..=n_ops {
            let parent = Scratch::new(&format!("cg-{fail_at}"));
            let home = Scratch::new(&format!("cg-{fail_at}-home"));
            let placement = parent.0.join("demo");
            install_old(&placement);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_cg", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &placement.to_string_lossy(),
                &digest_hex(OLD),
                SwapCapability::AtomicExchange,
            );
            let d = docs_under(&home.0, "topos_cg");
            // Seed the prior sync so a pre-commit fault leaves a readable OLD sync (mirrors a real apply).
            doc::write_doc(
                &RealFs,
                &d.sp.sync,
                &sync_at(g_old, g_old, &"0".repeat(64), &digest_hex(OLD)),
            )
            .unwrap();

            let fs = FaultFs::new(fail_at);
            let _ = materialize(
                &fs,
                &req("topos_cg", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            );

            // (a) bytes are old-complete or new-complete — never torn/mixed.
            let snap = dir_snapshot(&placement);
            let is_old = snap.as_deref() == Some(&expected(OLD));
            let is_new = snap.as_deref() == Some(&expected(NEW));
            assert!(
                is_old || is_new,
                "fail_at={fail_at}: placement is torn/mixed: {snap:?}"
            );

            // (b) `applied` advances only when the bytes are new AND every doc is written.
            if let Some(bytes) = std::fs::read(&d.sp.sync).ok()
                && let Ok(s) = load_versioned::<SyncState>(&bytes, 1)
                && s.applied == g_new
            {
                assert!(
                    is_new,
                    "fail_at={fail_at}: applied advanced without new bytes"
                );
                let m: PlacementMap =
                    load_versioned(&std::fs::read(&d.sp.map).unwrap(), 1).unwrap();
                assert_eq!(
                    m.applied_commit,
                    "1".repeat(64),
                    "fail_at={fail_at}: map lags sync"
                );
                let l: Lock = load_versioned(&std::fs::read(&d.sp.lock).unwrap(), 1).unwrap();
                assert_eq!(
                    l.bundle_digest, new_digest,
                    "fail_at={fail_at}: lock lags sync"
                );
            }

            // (c) a clean re-run converges to new bytes + applied advanced.
            let fs2 = RealFs;
            materialize(
                &fs2,
                &req("topos_cg", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            assert_eq!(
                dir_snapshot(&placement),
                Some(expected(NEW)),
                "fail_at={fail_at}: no converge"
            );
            let s2: SyncState = load_versioned(&std::fs::read(&d.sp.sync).unwrap(), 1).unwrap();
            assert_eq!(
                s2.applied, g_new,
                "fail_at={fail_at}: re-run did not advance applied"
            );
        }
    }

    /// The rename-dance fallback: faults leave old / new / (briefly) absent — never torn or mixed — and a
    /// clean re-run converges.
    #[test]
    fn crash_gate_rename_dance_is_never_mixed() {
        let g_new = Generation { epoch: 1, seq: 2 };
        let new_digest = digest_hex(NEW);
        let n_ops = {
            let parent = Scratch::new("dance-count");
            let home = Scratch::new("dance-count-home");
            let placement = parent.0.join("demo");
            install_old(&placement);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_d", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &placement.to_string_lossy(),
                &digest_hex(OLD),
                SwapCapability::RenameDance,
            );
            let d = docs_under(&home.0, "topos_d");
            let fs = FaultFs::new(0);
            materialize(
                &fs,
                &req("topos_d", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            fs.ops_attempted()
        };

        for fail_at in 1..=n_ops {
            let parent = Scratch::new(&format!("dance-{fail_at}"));
            let home = Scratch::new(&format!("dance-{fail_at}-home"));
            let placement = parent.0.join("demo");
            install_old(&placement);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_d", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &placement.to_string_lossy(),
                &digest_hex(OLD),
                SwapCapability::RenameDance,
            );
            let d = docs_under(&home.0, "topos_d");

            let fs = FaultFs::new(fail_at);
            let _ = materialize(
                &fs,
                &req("topos_d", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            );

            let snap = dir_snapshot(&placement);
            let ok = snap.is_none() // the brief absent window
                || snap.as_deref() == Some(&expected(OLD))
                || snap.as_deref() == Some(&expected(NEW));
            assert!(
                ok,
                "fail_at={fail_at}: dance left torn/mixed bytes: {snap:?}"
            );

            // Converge.
            materialize(
                &RealFs,
                &req("topos_d", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            assert_eq!(
                dir_snapshot(&placement),
                Some(expected(NEW)),
                "fail_at={fail_at}: no converge"
            );
        }
    }

    /// The first-install path (absent placement) under the crash gate: faults leave the placement ABSENT
    /// or NEW-complete — never partial — and `applied` advances only once the bytes are in and the docs are
    /// written; a clean re-run converges.
    #[test]
    fn crash_gate_first_install_leaves_absent_or_new_complete() {
        let g_new = Generation { epoch: 1, seq: 1 };
        let new_digest = digest_hex(NEW);
        let n_ops = {
            let parent = Scratch::new("fi-count");
            let home = Scratch::new("fi-count-home");
            let placement = parent.0.join("demo"); // absent
            let bundle = rendered(NEW);
            let lock = lock_of("topos_fi", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &placement.to_string_lossy(),
                &"0".repeat(64),
                SwapCapability::AtomicExchange,
            );
            let d = docs_under(&home.0, "topos_fi");
            let fs = FaultFs::new(0);
            materialize(
                &fs,
                &req("topos_fi", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            fs.ops_attempted()
        };

        for fail_at in 1..=n_ops {
            let parent = Scratch::new(&format!("fi-{fail_at}"));
            let home = Scratch::new(&format!("fi-{fail_at}-home"));
            let placement = parent.0.join("demo"); // absent
            let bundle = rendered(NEW);
            let lock = lock_of("topos_fi", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &placement.to_string_lossy(),
                &"0".repeat(64),
                SwapCapability::AtomicExchange,
            );
            let d = docs_under(&home.0, "topos_fi");
            // Seed an OLD sync so a pre-commit fault leaves a readable lagging `applied`.
            doc::write_doc(
                &RealFs,
                &d.sp.sync,
                &sync_at(
                    Generation { epoch: 0, seq: 0 },
                    Generation { epoch: 0, seq: 0 },
                    &"0".repeat(64),
                    &"0".repeat(64),
                ),
            )
            .unwrap();

            let fs = FaultFs::new(fail_at);
            let _ = materialize(
                &fs,
                &req("topos_fi", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            );

            // (a) absent or new-complete — never a partial directory.
            let snap = dir_snapshot(&placement);
            let ok = snap.is_none() || snap.as_deref() == Some(&expected(NEW));
            assert!(
                ok,
                "fail_at={fail_at}: first-install left a partial dir: {snap:?}"
            );

            // (b) `applied` advances only with the new bytes in place AND all docs written.
            if let Ok(bytes) = std::fs::read(&d.sp.sync)
                && let Ok(s) = load_versioned::<SyncState>(&bytes, 1)
                && s.applied == g_new
            {
                assert_eq!(
                    snap.as_deref(),
                    Some(&expected(NEW)[..]),
                    "fail_at={fail_at}"
                );
            }

            // (c) a clean re-run converges.
            materialize(
                &RealFs,
                &req("topos_fi", &placement, &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            assert_eq!(
                dir_snapshot(&placement),
                Some(expected(NEW)),
                "fail_at={fail_at}: no converge"
            );
        }
    }
}
