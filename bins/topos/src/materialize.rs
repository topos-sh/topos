//! The crash-safe byte-writing materializer: place a verified bundle's exact bytes onto EVERY target
//! placement via a **namespace-atomic directory swap** per dir, then advance the durable docs — so a
//! crash at any boundary leaves each placement holding OLD-or-NEW *complete* bytes, never a torn,
//! mixed, or half-written tree.
//!
//! `atomic.rs` (the single-*file* crash-safe write) is unchanged; this module owns the crash-safe
//! *sequence* for whole directories. The raw swap syscall is the [`FsOps::exchange_dir`] seam op.
//!
//! ## The order is the safety
//!
//! For EACH target placement, in map order:
//! 1. Build a staging dir as a **sibling in the placement's PARENT** (guaranteed same filesystem) and
//!    `fsync` every staged file AND every staging directory.
//! 2. **Atomic swap** the staging dir with the placement dir ([`SwapCapability::AtomicExchange`]) — one
//!    namespace operation. (A first install renames into an absent dir; a swap-incapable FS degrades to
//!    the logged [`SwapCapability::RenameDance`] with a brief *absent*, never *mixed*, window.)
//! 3. `fsync` the parent so the swap is durable.
//! 4. Drop the old bytes the swap parked at the staging path.
//! 5. Record THAT placement's new per-placement state (`map.json` only) — the crash-progress marker,
//!    so a re-run heals landed dirs (bytes already at target ⇒ record, no second swap) and swaps the
//!    rest.
//!
//! Then, once EVERY target holds the new bytes:
//! 6. Commit the docs **map → lock → sync** ([`commit_docs`]); `applied` advances only at the final
//!    sync write, strictly after the new bytes are durably on disk everywhere.
//!
//! A fault before step 6's sync write leaves `applied` naming the OLD generation while some (or all)
//! placements hold NEW bytes; the next pull re-derives each dir's state, sees the landed dirs already
//! equal the target, and HEALS forward (the kernel `sync::refine_after_fetch` `AlreadyAtTarget` path
//! when every dir landed; the swap loop's skip arm otherwise) rather than mistaking the bytes for a
//! draft. Before OVERWRITING a dir whose bytes differ from ITS recorded per-placement sha (an edit no
//! snapshot has captured yet), the caller-supplied [`MaterializeReq::snapshot`] seam commits those
//! bytes into the sidecar store — never a lost byte. The per-skill writer flock (held by the caller,
//! living OUTSIDE the swapped dirs) serializes topos writers across the whole sequence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use topos_core::digest::to_hex;
use topos_gitstore::{FileMode, RenderedBundle};
use topos_types::persisted::{Lock, PlacementMap, PlacementState, SwapCapability, SyncState};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::{FsOps, PathKind};
use crate::scan::{self, ScannedBundle};
use crate::sidecar::SkillPaths;

/// The pre-overwrite snapshot seam's shape (see [`MaterializeReq::snapshot`]).
pub(crate) type SnapshotFn<'a> = &'a dyn Fn(&ScannedBundle) -> Result<(), ClientError>;

/// The consented-takeover revalidation's shape (see [`MaterializeReq::takeover`]).
pub(crate) type TakeoverFn<'a> = &'a dyn Fn(&Path) -> bool;

/// Everything the materializer needs for one apply. The engine has already fetched + `render_verified`'d
/// `bundle` (so the bytes are authenticated), reconciled `next_map`'s placement set, and computed the
/// complete `next_lock` + `next_sync` target.
pub(crate) struct MaterializeReq<'a> {
    /// The stable skill id (names the staging / graveyard / probe siblings).
    pub skill_id: &'a str,
    /// Indices into `next_map.placements` this apply lands the bundle on — the MANAGED set (a recorded
    /// placement outside the current plan is frozen: never written, never deleted).
    pub target_indices: &'a [usize],
    /// The verified bytes to place.
    pub bundle: &'a RenderedBundle,
    /// The engine-computed next map: the reconciled placements + the PRIOR per-placement states (each
    /// landed index's state is updated here) + the map-level `applied_commit`/`materialized_sha`
    /// already advanced. The first placement's map-level mirrors are refreshed on write.
    pub next_map: PlacementMap,
    /// The lock to write (built by the engine from the bundle — kept in step with the placed bytes).
    pub next_lock: &'a Lock,
    /// The complete target sync state (the engine computed `applied`/`base_commit`/`work_hash`/…).
    pub next_sync: &'a SyncState,
    /// Where the durable docs live.
    pub sp: &'a SkillPaths,
    /// The pre-overwrite snapshot seam: invoked with the scan of a dir whose on-disk bytes differ from
    /// BOTH its recorded per-placement sha and the bundle being placed (an edit nothing has captured) —
    /// the never-a-lost-byte rail. `None` only where the caller has already snapshotted every copy
    /// (reset / go-back / the merge, whose snapshot-on-touch runs first).
    pub snapshot: Option<SnapshotFn<'a>>,
    /// Consent to OVERWRITE an occupied target the record never materialized (snapshot-first) —
    /// the one disclosed takeover path: the built-in's consented `follow topos --yes` adoption of a
    /// marked downloaded copy. The predicate RE-PROVES the consent against the LIVE dir immediately
    /// before the overwrite (the built-in re-checks the downloaded-copy marker), so a dir that
    /// changed since the describe fails closed. Everywhere else `None`: a first install into an
    /// occupied dir whose bytes differ from the target REFUSES (the never-clobber backstop) —
    /// including an adopted dir whose occupant raced to change (the consent was for an IDENTICAL
    /// copy).
    pub takeover: Option<TakeoverFn<'a>>,
}

/// What the materializer actually did (so the engine can log / record the effective capability).
/// Mirrors the FIRST landed placement (the map-level summary fields carry the same).
#[derive(Debug, Clone)]
pub(crate) struct MaterializeReport {
    pub swap_capability: SwapCapability,
    pub pre_existing_sha: Option<String>,
}

/// The sha of whatever was in a placement dir BEFORE topos first wrote into it — restored on
/// uninstall. **Sticky:** once captured it never changes. On the first overwrite (the state has no
/// `pre_existing_sha` yet but a directory was present) the prior recorded `materialized_sha` *is* the
/// user's original bytes (adopt-in-place wrote nothing into the dir, so the recorded sha equals what
/// is there). A genuine first install into an absent dir has nothing pre-existing. Computed from the
/// DURABLE prior state, never from post-swap disk.
pub(crate) fn derive_pre_existing_state(
    prior: &PlacementState,
    dir_was_present: bool,
) -> Option<String> {
    prior.pre_existing_sha.clone().or_else(|| {
        if dir_was_present {
            prior.materialized_sha.clone()
        } else {
            None
        }
    })
}

/// Write the three durable docs in the load-bearing order **map → lock → sync**.
///
/// ORDER IS LOAD-BEARING. An apply mutates these docs IN PLACE (there is no enclosing staging-dir rename,
/// unlike `add`, whose whole-directory publish rename makes its internal doc order crash-irrelevant).
/// `sync.json` is the COMMIT POINT: `applied` advances only here, and only after the new bytes are durably
/// swapped onto disk. `map` + `lock` are written FIRST so a crash between any two leaves `applied` still
/// naming the OLD generation — the next pull re-derives each placement's state, sees the landed dirs
/// already equal the target, and HEALS forward instead of mistaking the new bytes for a draft. Were
/// `sync` written first, a crash before `map` would leave `applied` current while `map` / `lock` stayed
/// stale forever (uninstall and go-back would then restore the wrong bytes).
///
/// Each individual write is itself crash-safe (atomic temp → fsync → rename → fsync-dir).
pub(crate) fn commit_docs(
    fs: &dyn FsOps,
    sp: &SkillPaths,
    next_map: &PlacementMap,
    next_lock: &Lock,
    next_sync: &SyncState,
) -> Result<(), ClientError> {
    doc::write_map(fs, &sp.map, next_map)?;
    doc::write_doc(fs, &sp.lock, next_lock)?;
    doc::write_doc(fs, &sp.sync, next_sync)?;
    Ok(())
}

/// Refresh the map-level summary fields from the FIRST placement's state (the v1-legible mirror).
pub(crate) fn mirror_first_placement(map: &mut PlacementMap) {
    if let Some(first) = map.placement_state.first() {
        if let Some(sha) = &first.materialized_sha {
            map.materialized_sha = sha.clone();
        }
        map.pre_existing_sha = first.pre_existing_sha.clone();
        map.swap_capability = first.swap_capability;
    }
}

/// Materialize `req.bundle`'s bytes onto every target placement and commit the docs. Returns the
/// effective capability + the recorded prior-bytes sha of the first placement.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] if a placement is a non-directory, an unresolvable symlink,
/// an unscannable occupied dir, an occupied FIRST-install target holding non-target bytes (the
/// never-clobber backstop, unless [`MaterializeReq::takeover`] re-proves consent), or on a filesystem with
/// no safe swap; otherwise the underlying [`FsOps`] failure (which the crash gate injects). On any
/// error `applied` has NOT advanced (the sync write is the last, all-or-nothing step);
/// already-landed placements are recorded in the map, so a re-run converges without a second swap
/// of those dirs.
pub(crate) fn materialize(
    fs: &dyn FsOps,
    req: &MaterializeReq<'_>,
) -> Result<MaterializeReport, ClientError> {
    let mut map = req.next_map.clone();
    let target_hex = to_hex(&req.bundle.bundle_digest);

    for &i in req.target_indices {
        let placement_dir = PathBuf::from(&map.placements[i]);
        let kind = fs.path_kind(&placement_dir)?;
        let target = resolve_target(fs, &placement_dir, kind)?;
        let parent = target.parent.clone();

        // Clear any leftover litter from a prior crashed apply of THIS skill (under the caller's flock).
        cleanup_litter(fs, &parent, req.skill_id)?;

        if target.dir_was_present {
            // Pre-swap scan: heal a dir that already holds the target bytes (a crash after a prior
            // swap, or an idempotent re-apply) with NO second swap; snapshot an uncaptured edit
            // before it is overwritten (never a lost byte). An unscannable occupied dir is refused —
            // we cannot prove what we would destroy.
            match scan::scan(&target.dir) {
                Ok(scanned) => {
                    let on_disk = to_hex(&scanned.bundle_digest);
                    if on_disk == target_hex {
                        let prior = map.placement_state[i].clone();
                        map.placement_state[i] = PlacementState {
                            materialized_sha: Some(target_hex.clone()),
                            pre_existing_sha: derive_pre_existing_state(&prior, true),
                            ..prior
                        };
                        mirror_first_placement(&mut map);
                        doc::write_map(fs, &sp_map(req), &map)?;
                        continue;
                    }
                    // The never-clobber backstop: a target the record NEVER materialized is not
                    // ours to replace — a first install must never overwrite an occupant (the
                    // naming discipline avoids occupied dirs, and an adopted dir whose bytes raced
                    // to change since the describe fails closed here: the consent was for an
                    // IDENTICAL copy; the next describe re-probes and re-namespaces). The one
                    // exception is the caller's disclosed takeover, re-proven against the LIVE dir.
                    let recorded = map.placement_state[i].materialized_sha.as_deref();
                    if recorded.is_none() && !req.takeover.is_some_and(|t| t(&target.dir)) {
                        return Err(ClientError::PlacementUnsupported {
                            reason: format!(
                                "the placement {} is occupied by content topos never placed; \
                                 refusing to overwrite it",
                                target.dir.display()
                            ),
                        });
                    }
                    if recorded != Some(on_disk.as_str())
                        && let Some(snapshot) = req.snapshot
                    {
                        snapshot(&scanned)?;
                    }
                }
                Err(_) => {
                    return Err(ClientError::PlacementUnsupported {
                        reason: format!(
                            "the placement {} cannot be read; refusing to overwrite it",
                            target.dir.display()
                        ),
                    });
                }
            }
        }

        // Trust the cached per-placement capability; probe only a genesis `Unsupported` placeholder.
        let mut cap = map.placement_state[i].swap_capability;
        if cap == SwapCapability::Unsupported {
            cap = probe_capability(fs, &parent, req.skill_id)?;
        }

        // Build + fsync the staging dir (a same-filesystem sibling of the placement).
        let staging = staging_path(&parent, req.skill_id);
        build_staging(fs, &staging, req.bundle)?;

        // Place the bytes.
        if target.dir_was_present {
            cap = place_update(fs, &staging, &target.dir, &parent, req.skill_id, cap)?;
        } else {
            // First install: an atomic create — no prior bytes to mix.
            fs.rename_dir_noreplace(&staging, &target.dir)
                .map_err(|e| ClientError::Io(format!("first-install rename: {e}")))?;
            fs.fsync_dir(&parent)?;
        }

        // Record THIS placement's landing (map only — the crash-progress marker; `applied` waits).
        let prior = map.placement_state[i].clone();
        map.placement_state[i] = PlacementState {
            materialized_sha: Some(target_hex.clone()),
            pre_existing_sha: derive_pre_existing_state(&prior, target.dir_was_present),
            swap_capability: cap,
            ..prior
        };
        mirror_first_placement(&mut map);
        doc::write_map(fs, &sp_map(req), &map)?;
    }

    // Commit map → lock → sync (the commit point; `applied` advances only here).
    commit_docs(fs, req.sp, &map, req.next_lock, req.next_sync)?;

    let first = map.placement_state.first();
    Ok(MaterializeReport {
        swap_capability: first.map_or(SwapCapability::Unsupported, |s| s.swap_capability),
        pre_existing_sha: first.and_then(|s| s.pre_existing_sha.clone()),
    })
}

fn sp_map(req: &MaterializeReq<'_>) -> PathBuf {
    req.sp.map.clone()
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
            // the swap replaces the directory's contents, never the symlink itself. A DANGLING symlink
            // is a first install into its resolved target's place — but with no resolvable target we
            // refuse (the caller's classification already treats it as absent).
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
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_core::digest::{self, FileMode, ManifestEntry};
    use topos_gitstore::RenderedFile;
    use topos_types::PLACEMENT_MAP_SCHEMA_VERSION;
    use topos_types::persisted::{LockedFile, PlacementKind};

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

    /// Read a placement dir into a sorted (rel-path, bytes) list, or `None` if absent.
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

    fn sync_at(applied: u64, observed: u64, base: &str, work: &str) -> SyncState {
        SyncState {
            schema_version: 1,
            observed,
            observed_version_id: base.to_owned(),
            applied,
            base_commit: base.to_owned(),
            work_hash: work.to_owned(),
            held: false,
        }
    }

    /// A prior map over `dirs`, every placement recorded at `materialized` with capability `cap`.
    fn prior_map(dirs: &[&Path], materialized: &str, cap: SwapCapability) -> PlacementMap {
        PlacementMap {
            schema_version: PLACEMENT_MAP_SCHEMA_VERSION,
            placements: dirs
                .iter()
                .map(|d| d.to_string_lossy().into_owned())
                .collect(),
            applied_commit: "0".repeat(64),
            materialized_sha: materialized.to_owned(),
            pre_existing_sha: None,
            swap_capability: cap,
            placement_state: dirs
                .iter()
                .map(|_| PlacementState {
                    kind: PlacementKind::Native,
                    agent: None,
                    materialized_sha: Some(materialized.to_owned()),
                    pre_existing_sha: None,
                    swap_capability: cap,
                })
                .collect(),
            harness: None,
            harness_layer: None,
            harness_slug: None,
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

    /// A single-target request over `placement` (index 0 of a one-entry map).
    fn req<'a>(
        skill_id: &'a str,
        indices: &'a [usize],
        bundle: &'a RenderedBundle,
        prior: &PlacementMap,
        next_lock: &'a Lock,
        next_sync: &'a SyncState,
        sp: &'a SkillPaths,
    ) -> MaterializeReq<'a> {
        MaterializeReq {
            skill_id,
            target_indices: indices,
            bundle,
            next_map: PlacementMap {
                applied_commit: "1".repeat(64),
                materialized_sha: digest_hex(NEW),
                ..prior.clone()
            },
            next_lock,
            next_sync,
            sp,
            snapshot: None,
            takeover: None,
        }
    }

    #[test]
    fn first_install_places_exact_bytes_and_modes() {
        let parent = Scratch::new("first");
        let home = Scratch::new("first-home");
        let placement = parent.0.join("demo"); // absent
        let bundle = rendered(NEW);
        let lock = lock_of("topos_first", NEW, &"1".repeat(64));
        let g = 1;
        let sync = sync_at(g, g, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::Unsupported);
        prior.placement_state[0].materialized_sha = None; // never placed
        let d = docs_under(&home.0, "topos_first");

        let report = materialize(
            &RealFs,
            &req("topos_first", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
        // Docs committed; the per-placement state records the landing.
        let written: SyncState = load_versioned(&std::fs::read(&d.sp.sync).unwrap(), 1).unwrap();
        assert_eq!(written.applied, g);
        let m = crate::doc::read_map(&RealFs, &d.sp.map).unwrap().unwrap();
        assert_eq!(
            m.placement_state[0].materialized_sha.as_deref(),
            Some(digest_hex(NEW).as_str())
        );
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
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        // prior map mimics `add`: pre_existing None, materialized = the adopted (old) bytes.
        let prior = prior_map(
            &[&placement],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_upd");

        let report = materialize(
            &RealFs,
            &req("topos_upd", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
    fn a_first_install_into_an_occupied_dir_refuses_and_writes_nothing() {
        let parent = Scratch::new("occ");
        let home = Scratch::new("occ-home");
        let placement = parent.0.join("demo");
        install_old(&placement); // an occupant topos never placed
        let bundle = rendered(NEW);
        let lock = lock_of("topos_occ", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::Unsupported);
        prior.placement_state[0].materialized_sha = None; // this apply would be the FIRST install
        let d = docs_under(&home.0, "topos_occ");

        let err = materialize(
            &RealFs,
            &req("topos_occ", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        )
        .unwrap_err();
        assert!(matches!(err, ClientError::PlacementUnsupported { .. }));
        assert!(
            err.to_string().contains(&placement.display().to_string()),
            "the refusal names the dir: {err}"
        );
        // The occupant is byte-untouched and no doc advanced (`sync.json` was never written).
        assert_eq!(dir_snapshot(&placement), Some(expected(OLD)));
        assert!(!d.sp.sync.exists(), "nothing committed");
    }

    #[test]
    fn a_takeover_predicate_that_answers_false_still_refuses() {
        // The takeover is a per-target REVALIDATION against the live dir, not a blanket consent:
        // a predicate that cannot re-prove the disclosed condition (e.g. the built-in's downloaded
        // -copy marker vanished since the describe) fails closed exactly like no takeover at all.
        let parent = Scratch::new("occ-tk");
        let home = Scratch::new("occ-tk-home");
        let placement = parent.0.join("demo");
        install_old(&placement);
        let bundle = rendered(NEW);
        let lock = lock_of("topos_occt", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::Unsupported);
        prior.placement_state[0].materialized_sha = None;
        let d = docs_under(&home.0, "topos_occt");

        let deny: &dyn Fn(&Path) -> bool = &|_| false;
        let req = MaterializeReq {
            takeover: Some(deny),
            ..req("topos_occt", &[0], &bundle, &prior, &lock, &sync, &d.sp)
        };
        let err = materialize(&RealFs, &req).unwrap_err();
        assert!(matches!(err, ClientError::PlacementUnsupported { .. }));
        assert_eq!(dir_snapshot(&placement), Some(expected(OLD)));
    }

    #[test]
    fn a_first_install_over_target_equal_bytes_still_heals_in_place() {
        let parent = Scratch::new("occ-heal");
        let home = Scratch::new("occ-heal-home");
        let placement = parent.0.join("demo");
        // The occupant already IS the target (an adopted identical copy): heal, never refuse.
        std::fs::create_dir_all(&placement).unwrap();
        for (p, m, b) in NEW {
            let dest = placement.join(p);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(&dest, b).unwrap();
            if *m == FileMode::Executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let bundle = rendered(NEW);
        let lock = lock_of("topos_occh", NEW, &"1".repeat(64));
        let g = 1;
        let sync = sync_at(g, g, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::Unsupported);
        prior.placement_state[0].materialized_sha = None;
        let d = docs_under(&home.0, "topos_occh");

        materialize(
            &RealFs,
            &req("topos_occh", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        )
        .unwrap();
        assert_eq!(dir_snapshot(&placement), Some(expected(NEW)));
        let m = crate::doc::read_map(&RealFs, &d.sp.map).unwrap().unwrap();
        assert_eq!(
            m.placement_state[0].materialized_sha.as_deref(),
            Some(digest_hex(NEW).as_str()),
            "the heal advanced the record with no swap"
        );
    }

    #[test]
    fn refuses_a_non_directory_placement() {
        let parent = Scratch::new("file");
        let home = Scratch::new("file-home");
        let placement = parent.0.join("demo");
        std::fs::write(&placement, b"i am a file").unwrap(); // Other, not a dir
        let bundle = rendered(NEW);
        let lock = lock_of("topos_file", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &[&placement],
            &"0".repeat(64),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_file");

        let err = materialize(
            &RealFs,
            &req("topos_file", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &[&placement],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_link");

        materialize(
            &RealFs,
            &req("topos_link", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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

    /// MULTI-PLACEMENT: one apply lands the SAME bundle in every target dir, each its own staged
    /// swap; the per-placement states all record the landing, and `applied` advances once at the end.
    #[test]
    fn multi_placement_apply_lands_every_target() {
        let parent = Scratch::new("multi");
        let home = Scratch::new("multi-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let shared = parent.0.join("agents").join("demo");
        let native_a = parent.0.join("a").join("demo");
        let native_b = parent.0.join("b").join("demo"); // absent → first install
        install_old(&shared);
        install_old(&native_a);
        let bundle = rendered(NEW);
        let lock = lock_of("topos_multi", NEW, &"1".repeat(64));
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(
            &[&shared, &native_a, &native_b],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        prior.placement_state[2].materialized_sha = None; // the appended, never-placed target
        let d = docs_under(&home.0, "topos_multi");

        materialize(
            &RealFs,
            &req(
                "topos_multi",
                &[0, 1, 2],
                &bundle,
                &prior,
                &lock,
                &sync,
                &d.sp,
            ),
        )
        .unwrap();

        for dir in [&shared, &native_a, &native_b] {
            assert_eq!(dir_snapshot(dir), Some(expected(NEW)), "{}", dir.display());
        }
        let m = crate::doc::read_map(&RealFs, &d.sp.map).unwrap().unwrap();
        for st in &m.placement_state {
            assert_eq!(
                st.materialized_sha.as_deref(),
                Some(digest_hex(NEW).as_str())
            );
        }
        let s: SyncState = load_versioned(&std::fs::read(&d.sp.sync).unwrap(), 1).unwrap();
        assert_eq!(s.applied, 2);
    }

    /// PER-PLACEMENT SNAPSHOT-BEFORE-OVERWRITE: a dir whose bytes differ from ITS recorded sha is
    /// handed to the snapshot seam BEFORE the swap — never a lost byte — while an unedited dir is not.
    #[test]
    fn snapshot_seam_fires_only_for_an_uncaptured_edit() {
        let parent = Scratch::new("snap");
        let home = Scratch::new("snap-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let edited = parent.0.join("edited").join("demo");
        let clean = parent.0.join("clean").join("demo");
        install_old(&edited);
        std::fs::write(edited.join("SKILL.md"), b"# my local edit\n").unwrap();
        install_old(&clean);
        let bundle = rendered(NEW);
        let lock = lock_of("topos_snap", NEW, &"1".repeat(64));
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &[&edited, &clean],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_snap");

        let snapshots: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let snap = |scanned: &ScannedBundle| {
            snapshots
                .borrow_mut()
                .push(digest::to_hex(&scanned.bundle_digest));
            Ok(())
        };
        let mut r = req("topos_snap", &[0, 1], &bundle, &prior, &lock, &sync, &d.sp);
        r.snapshot = Some(&snap);
        materialize(&RealFs, &r).unwrap();

        let taken = snapshots.borrow();
        assert_eq!(taken.len(), 1, "exactly the edited dir is snapshotted");
        assert_ne!(
            taken[0],
            digest_hex(OLD),
            "the snapshot carries the EDITED bytes"
        );
        assert_eq!(dir_snapshot(&edited), Some(expected(NEW)));
        assert_eq!(dir_snapshot(&clean), Some(expected(NEW)));
    }

    /// The release-blocker crash gate: fault every materialize boundary across TWO placements and
    /// assert each placement holds the OLD-or-NEW *complete* bytes (never torn/mixed), `applied`
    /// advances ONLY once every dir holds the new bytes and all docs are written, and a clean re-run
    /// converges (already-landed dirs skip the swap).
    #[test]
    fn crash_gate_atomic_exchange_leaves_old_or_new_complete() {
        let probe = Scratch::new("probe");
        if !swap_supported(&probe.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let g_old = 1;
        let g_new = 2;
        let new_digest = digest_hex(NEW);

        // Size the sweep from a clean run.
        let n_ops = {
            let parent = Scratch::new("cg-count");
            let home = Scratch::new("cg-count-home");
            let p1 = parent.0.join("one").join("demo");
            let p2 = parent.0.join("two").join("demo");
            install_old(&p1);
            install_old(&p2);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_cg", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &[&p1, &p2],
                &digest_hex(OLD),
                SwapCapability::AtomicExchange,
            );
            let d = docs_under(&home.0, "topos_cg");
            let fs = FaultFs::new(0);
            materialize(
                &fs,
                &req("topos_cg", &[0, 1], &bundle, &prior, &lock, &sync, &d.sp),
            )
            .unwrap();
            fs.ops_attempted()
        };
        assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

        for fail_at in 1..=n_ops {
            let parent = Scratch::new(&format!("cg-{fail_at}"));
            let home = Scratch::new(&format!("cg-{fail_at}-home"));
            let p1 = parent.0.join("one").join("demo");
            let p2 = parent.0.join("two").join("demo");
            install_old(&p1);
            install_old(&p2);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_cg", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(
                &[&p1, &p2],
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
                &req("topos_cg", &[0, 1], &bundle, &prior, &lock, &sync, &d.sp),
            );

            // (a) EACH placement is old-complete or new-complete — never torn/mixed.
            let mut all_new = true;
            for p in [&p1, &p2] {
                let snap = dir_snapshot(p);
                let is_old = snap.as_deref() == Some(&expected(OLD));
                let is_new = snap.as_deref() == Some(&expected(NEW));
                assert!(
                    is_old || is_new,
                    "fail_at={fail_at}: {} is torn/mixed: {snap:?}",
                    p.display()
                );
                all_new &= is_new;
            }

            // (b) `applied` advances only when EVERY dir holds the new bytes AND every doc is written.
            if let Some(bytes) = std::fs::read(&d.sp.sync).ok()
                && let Ok(s) = load_versioned::<SyncState>(&bytes, 1)
                && s.applied == g_new
            {
                assert!(
                    all_new,
                    "fail_at={fail_at}: applied advanced without all dirs new"
                );
                let m = crate::doc::read_map(&RealFs, &d.sp.map).unwrap().unwrap();
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

            // (c) a clean re-run converges to new bytes everywhere + applied advanced. The re-run
            // reads the crash-progress map (a landed dir skips its swap).
            let prior2 = crate::doc::read_map(&RealFs, &d.sp.map)
                .unwrap()
                .unwrap_or_else(|| prior.clone());
            materialize(
                &RealFs,
                &req("topos_cg", &[0, 1], &bundle, &prior2, &lock, &sync, &d.sp),
            )
            .unwrap();
            for p in [&p1, &p2] {
                assert_eq!(
                    dir_snapshot(p),
                    Some(expected(NEW)),
                    "fail_at={fail_at}: no converge at {}",
                    p.display()
                );
            }
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
        let g_new = 2;
        let new_digest = digest_hex(NEW);
        let n_ops = {
            let parent = Scratch::new("dance-count");
            let home = Scratch::new("dance-count-home");
            let placement = parent.0.join("demo");
            install_old(&placement);
            let bundle = rendered(NEW);
            let lock = lock_of("topos_d", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let prior = prior_map(&[&placement], &digest_hex(OLD), SwapCapability::RenameDance);
            let d = docs_under(&home.0, "topos_d");
            let fs = FaultFs::new(0);
            materialize(
                &fs,
                &req("topos_d", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
            let prior = prior_map(&[&placement], &digest_hex(OLD), SwapCapability::RenameDance);
            let d = docs_under(&home.0, "topos_d");

            let fs = FaultFs::new(fail_at);
            let _ = materialize(
                &fs,
                &req("topos_d", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
            let prior2 = crate::doc::read_map(&RealFs, &d.sp.map)
                .unwrap()
                .unwrap_or_else(|| prior.clone());
            materialize(
                &RealFs,
                &req("topos_d", &[0], &bundle, &prior2, &lock, &sync, &d.sp),
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
        let g_new = 1;
        let new_digest = digest_hex(NEW);
        let n_ops = {
            let parent = Scratch::new("fi-count");
            let home = Scratch::new("fi-count-home");
            let placement = parent.0.join("demo"); // absent
            let bundle = rendered(NEW);
            let lock = lock_of("topos_fi", NEW, &"1".repeat(64));
            let sync = sync_at(g_new, g_new, &"1".repeat(64), &new_digest);
            let mut prior = prior_map(
                &[&placement],
                &"0".repeat(64),
                SwapCapability::AtomicExchange,
            );
            prior.placement_state[0].materialized_sha = None;
            let d = docs_under(&home.0, "topos_fi");
            let fs = FaultFs::new(0);
            materialize(
                &fs,
                &req("topos_fi", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
            let mut prior = prior_map(
                &[&placement],
                &"0".repeat(64),
                SwapCapability::AtomicExchange,
            );
            prior.placement_state[0].materialized_sha = None;
            let d = docs_under(&home.0, "topos_fi");
            // Seed an OLD sync so a pre-commit fault leaves a readable lagging `applied`.
            doc::write_doc(
                &RealFs,
                &d.sp.sync,
                &sync_at(0, 0, &"0".repeat(64), &"0".repeat(64)),
            )
            .unwrap();

            let fs = FaultFs::new(fail_at);
            let _ = materialize(
                &fs,
                &req("topos_fi", &[0], &bundle, &prior, &lock, &sync, &d.sp),
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
            let prior2 = crate::doc::read_map(&RealFs, &d.sp.map)
                .unwrap()
                .unwrap_or_else(|| prior.clone());
            materialize(
                &RealFs,
                &req("topos_fi", &[0], &bundle, &prior2, &lock, &sync, &d.sp),
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
