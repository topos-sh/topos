//! The crash-safe byte-writing materializer: place a verified bundle's exact bytes onto EVERY target
//! placement via a **namespace-atomic directory swap** per dir, then advance the durable docs — so a
//! crash at any boundary leaves each placement holding OLD-or-NEW *complete* bytes, never a torn,
//! mixed, or half-written tree.
//!
//! `atomic.rs` (the single-*file* crash-safe write) is unchanged; this module owns the crash-safe
//! *sequence* for whole directories. The raw swap syscall is the [`FsOps::exchange_at`] seam op.
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
//!
//! ## The residual window a rename cannot close (documented, not hoped away)
//!
//! Parking a directory takes it out of every PATH, but POSIX cannot take it away from an
//! already-open FILE DESCRIPTOR: a process that opened `SKILL.md` before the swap can keep writing
//! the renamed inode for as long as it likes. So a park is never judged once and destroyed on that
//! judgment — [`verify_parked_old`] re-reads the park until two CONSECUTIVE scans agree, accounts
//! for every distinct content it saw (snapshot, baseline, target), and only then removes it; a park
//! that keeps moving, or that cannot be accounted for, is preserved on disk (`.topos-kept-*`)
//! rather than deleted. What remains is irreducible: a write that lands through an open fd BETWEEN
//! the final verifying scan and the completion of the unlink walk can still die unexamined. No
//! sequence of path syscalls closes that window; narrowing it to one scan-to-unlink beat — and
//! preserving anything observed moving — is the honest limit.
//!
//! ## The proof-to-write boundary (no-follow writes; the held parent handle)
//!
//! The containment proofs above are statements about a PATH at one instant, and re-proving a path
//! harder never closes the gap to the syscall that writes through it — an ancestor swapped for a
//! symlink in that gap re-aims the same spelling at a different tree. So this module writes
//! through the seam's no-follow boundary instead (see `fs_seam`'s module doc): staged files open
//! `O_NOFOLLOW`, the staging tree is built at HELD HANDLES end to end — the staging root one
//! fd-walked level below the held parent handle, every component below it descended from the
//! staging handle's own fd ([`crate::fs_seam::FsOps::create_dir_nofollow_at`] — the stage's
//! mutable pathname is never re-resolved during the build) — and every landing rename/exchange
//! runs *at a held directory handle* opened on the placement's parent immediately after the proof
//! ([`crate::fs_seam::DirHandle`] — `(dev, ino)` captured at proof time, re-checked against the
//! path immediately before the namespace op, the op itself `renameat` against the held fd). The
//! landing additionally proves the SOURCE: the staging leaf is re-opened from the parent's fd
//! and must still be the very directory object this run staged
//! ([`crate::fs_seam::FsOps::exchange_at_src`] / `rename_at_noreplace_src`), so a completed
//! stage moved aside and substituted at its predictable name refuses, never lands. A
//! swap between the proof and the landing is therefore met as itself and refused, never followed.
//! The litter sweep and the capability probes ride the SAME handle: their removals, creates, and
//! exchanges are fd-anchored (`remove_dir_all_at` / `create_dir_at` / `exchange_at`, each
//! identity-verified), and the settle rail's content scans — path reads by nature — re-verify
//! the handle immediately before every pass.
//! Named residuals (also in `fs_seam`): a whole REAL directory relocated after the proof keeps
//! its identity — the held fd then lands bytes inside that same (moved) directory object, never
//! through the swapped path; the settle rail's content scans are path reads between handle
//! verifies (never deletions); a non-unix port would fall back to path-based checks and must
//! revisit this boundary.

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
    /// the one disclosed takeover path: the built-in's consented `add topos` adoption of a
    /// marked downloaded copy. The predicate RE-PROVES the consent against the LIVE dir immediately
    /// before the overwrite (the built-in re-checks the downloaded-copy marker), so a dir that
    /// changed since the describe fails closed. Everywhere else `None`: a first install into an
    /// occupied dir whose bytes differ from the target REFUSES (the never-clobber backstop) —
    /// including an adopted dir whose occupant raced to change (the consent was for an IDENTICAL
    /// copy).
    pub takeover: Option<TakeoverFn<'a>>,
    /// Whether each staged tree carries the SELF-IGNORE sentinel (project-scope placements are
    /// local, never committed — the placed dir ignores itself; see [`crate::scan::IGNORE_SENTINEL`]).
    /// Injected only when the bundle ships no root ignore file of its own (a bundle's `.gitignore`
    /// is content); a dir healed in place gets no write at all, so a user's file at that path is
    /// never overwritten.
    pub self_ignore: bool,
    /// Per-target EXPECTED on-disk state — the opportunistic-fanout arm (the settled-draft sync):
    /// `(target index, Some(digest) | None-for-absent)` as the caller's scan observed it. When set,
    /// the pre-swap re-scan compares against it and a target whose bytes MOVED in the window (or
    /// cannot be read) is SKIPPED — recorded nothing, never refused, never frozen; the next sweep
    /// reconciles. A target already holding the bundle's bytes still heals in place. `None` (the
    /// default) keeps the strict arms: refuse/heal exactly as documented above.
    pub expected: Option<&'a [(usize, Option<String>)]>,
    /// PROJECT scope only: the checkout every placement of this apply must provably resolve
    /// inside. The plan already refused escaping roots ([`crate::placement::within_project`]), but
    /// a record is a memory, not a permission — an ancestor can become a symlink between the plan
    /// and this write — so the SAME containment proof is re-run here, at the write boundary:
    /// once before any staging byte lands in the placement's parent, and again immediately before
    /// the swap/rename that installs. The proof covers the STAGED path transitively: staging is a
    /// sibling in the placement's REAL parent, so a placement that proves containment pins its
    /// staging dir inside the checkout with it. A placement that no longer proves containment
    /// REFUSES (typed, nothing destroyed, `applied` not advanced) — never redirected. `None` =
    /// the person scope (home dirs carry no such rail).
    pub project_root: Option<&'a Path>,
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
        // The expected-state row for this target, when the opportunistic arm is armed. A target
        // the caller did not describe is skipped outright.
        let expectation: Option<&Option<String>> = match req.expected {
            None => None,
            Some(rows) => match rows.iter().find(|(idx, _)| *idx == i) {
                Some((_, exp)) => Some(exp),
                None => continue,
            },
        };
        let placement_dir = PathBuf::from(&map.placements[i]);
        // The containment rail, re-proven at the WRITE boundary (see [`MaterializeReq::project_root`]):
        // resolve_target below creates + canonicalizes the parent, which is already a write into
        // whatever the path resolves to — so the proof comes first (and resolve_target's own
        // parent creation walks no-follow under the proven root).
        prove_containment(req.project_root, &placement_dir)?;
        let kind = fs.path_kind(&placement_dir)?;
        let target = resolve_target(fs, &placement_dir, kind, req.project_root)?;
        let parent = target.parent.clone();
        // The PROOF-TO-WRITE anchor (see the module doc): the proven parent is pinned OPEN here,
        // and every namespace op that lands or removes bytes in it below runs at this handle —
        // verified `(dev, ino)`-identical to the path immediately before each op — so an
        // ancestor swapped for a symlink after this line is met as itself and refused.
        let parent_handle = fs
            .open_dir_handle(&parent)
            .map_err(|e| ClientError::Io(format!("open placement parent: {e}")))?;

        // A dir the caller expected PRESENT that has since vanished: bytes moved in the window —
        // skip, never guess (the next sweep reconciles from a fresh scan).
        if !target.dir_was_present && matches!(expectation, Some(Some(_))) {
            continue;
        }

        // Clear any leftover litter from a prior crashed apply of THIS skill (under the caller's
        // flock). The staging/graveyard names are PARKS a crashed run may have left mid-judgment —
        // a crash after the swap but before `verify_parked_old` concluded strands the OLD tree
        // (raced edits included) there — so they are JUDGED with the same park-then-verify rail,
        // never deleted on their name alone; only the probe dirs (throwaway empties this
        // materializer mints itself) are cleared blind. The whole sweep runs AT the held parent
        // handle (verified per op), so it comes AFTER the pin above by construction — a parent
        // moved-and-replaced by an outward symlink cannot aim these removals outside the proof.
        cleanup_litter(
            fs,
            &parent_handle,
            &parent,
            req.skill_id,
            &target_hex,
            map.placement_state[i].materialized_sha.as_deref(),
            req.snapshot,
        )?;

        // The tree as the pre-swap scan below saw it — a STAT-only fingerprint, re-taken
        // immediately before the swap so the decision the scan made is re-proven against the bytes
        // actually about to be destroyed (see the re-stat below).
        let mut observed: Option<Vec<DirStat>> = None;
        // What a snapshot on this placement has ALREADY captured (by digest) — read again by the
        // park verification below, so bytes committed once are not committed twice.
        let mut captured: Option<String> = None;
        if target.dir_was_present {
            // Pre-swap scan: heal a dir that already holds the target bytes (a crash after a prior
            // swap, or an idempotent re-apply) with NO second swap; snapshot an uncaptured edit
            // before it is overwritten (never a lost byte). An unscannable occupied dir is refused —
            // we cannot prove what we would destroy.
            match scan::scan(&target.dir) {
                Ok(scanned) => {
                    let on_disk = to_hex(&scanned.bundle_digest);
                    observed = dir_fingerprint(&target.dir);
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
                    // The opportunistic arm's re-stat: the dir must still hold exactly what the
                    // caller's scan observed — anything else means bytes moved in the window, and
                    // the target is SKIPPED (never overwritten, never refused).
                    if let Some(exp) = expectation
                        && exp.as_deref() != Some(on_disk.as_str())
                    {
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
                        if expectation.is_some() {
                            continue; // the fanout arm skips what it may not claim
                        }
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
                        captured = Some(on_disk);
                    }
                }
                Err(_) => {
                    if expectation.is_some() {
                        continue; // unreadable now = moved in the window — skip, never freeze
                    }
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
            cap = probe_capability(fs, &parent_handle, &parent, req.skill_id)?;
        }

        // Build + fsync the staging dir (a same-filesystem sibling of the placement), descending
        // from the HELD parent handle — the stage and every component below it are fd-walked, so
        // no path below the proven parent is re-resolved during the build. The verify here is
        // the early spelled-path check (a parent already swapped refuses before any byte is
        // staged); the landing re-proves both parent and stage identity inside the op.
        parent_handle
            .verify_unmoved()
            .map_err(|e| ClientError::Io(format!("staging parent check: {e}")))?;
        let staging = staging_path(&parent, req.skill_id);
        let staging_src = build_staging(fs, &parent_handle, &staging, req.bundle, req.self_ignore)?;

        // THE PRE-MUTATION RE-STAT. Everything decided above — heal, snapshot, never-clobber,
        // takeover — rode a scan taken BEFORE the capability probe and the staging build, both of
        // which take real time. The invariant is that no byte differing from its recorded baseline
        // is destroyed unless a snapshot taken AFTER the last revalidation holds it, so the tree is
        // re-fingerprinted here, immediately before the swap, and a tree that MOVED in that window
        // is either captured afresh or left alone:
        //
        // - an explicit destructive path that still has a snapshotter commits the bytes that are
        //   REALLY there (the earlier snapshot, if any, captured a tree that no longer exists);
        // - a sweep's opportunistic arm, a caller with no snapshotter, and a dir that has become
        //   unreadable all SKIP the placement — its recorded state stays behind the bytes, so the
        //   next sweep reconciles it from a fresh scan. Nothing unaccounted-for is overwritten.
        //
        // This is the DECISION rail (skip or proceed), and a decision must be made before the
        // mutation. The residual window it cannot close — between this stat and the swap syscall —
        // is closed on the other side: the swap PARKS the old tree instead of deleting it, and
        // `verify_parked_old` reads what is really in it before anything is dropped.
        if target.dir_was_present {
            let now = dir_fingerprint(&target.dir);
            if now.is_none() || now != observed {
                let fresh = scan::scan(&target.dir).ok();
                match (req.snapshot, &fresh) {
                    (Some(snapshot), Some(scanned))
                        if expectation.is_none()
                            && to_hex(&scanned.bundle_digest) != target_hex =>
                    {
                        snapshot(scanned)?;
                        captured = Some(to_hex(&scanned.bundle_digest));
                    }
                    _ => {
                        // The staging removal rides the same anchor: with the parent's identity
                        // unverifiable the staged tree stays (preserved, never deleted through a
                        // possibly-swapped path) — and the removal itself runs AT the held
                        // handle, so a swap after this check cannot re-aim it either.
                        if parent_handle.verify_unmoved().is_ok() {
                            fs.remove_dir_all_at(&parent_handle, leaf_name(&staging)?)?;
                        }
                        continue;
                    }
                }
            }
        }

        // The containment proof AGAIN, immediately before the namespace op that installs the
        // bytes — the staging build took real time, and an ancestor that became a symlink inside
        // that window would route the rename through it. (The op itself then runs at the held
        // parent handle, so even a swap in the beat after this proof cannot re-aim it.)
        if let Err(e) = prove_containment(req.project_root, &placement_dir) {
            if parent_handle.verify_unmoved().is_ok() {
                fs.remove_dir_all_at(&parent_handle, leaf_name(&staging)?)?;
            }
            return Err(e);
        }

        // Place the bytes — every namespace op at the held parent handle, the staged SOURCE
        // leaf re-proven as the very directory this run built inside each op (`_src`).
        if target.dir_was_present {
            let baseline = map.placement_state[i].materialized_sha.clone();
            let (next_cap, parked) = place_update(
                fs,
                &parent_handle,
                &staging_src,
                &target.dir,
                &parent,
                req.skill_id,
                cap,
            )?;
            cap = next_cap;
            // The old tree is parked, not gone: judge it, then drop it (at the held handle).
            verify_parked_old(
                fs,
                &parked,
                &target_hex,
                baseline.as_deref(),
                captured.as_deref(),
                req.snapshot,
                Some(&parent_handle),
            )?;
        } else {
            // First install: an atomic create — no prior bytes to mix. `_src`: a stage
            // substituted at its predictable name in the final beat refuses, never lands.
            let (from, to) = (leaf_name(&staging)?, leaf_name(&target.dir)?);
            fs.rename_at_noreplace_src(&parent_handle, from, to, &staging_src)
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

/// One entry of a directory's stat-only fingerprint: its path relative to the root plus the
/// `(mtime_ns, ctime_ns, size)` tuple. `ctime` is the load-bearing member — a content write always
/// bumps it, and `utimensat` cannot move it backwards — so a forged `mtime` never makes a changed
/// tree look unchanged (the same reasoning the stat cache's own rows rest on).
type DirStat = (String, crate::stat_cache::StatKey);

/// A STAT-ONLY fingerprint of a directory tree: every entry as a [`DirStat`], sorted. Reads no file
/// CONTENT, so the pre-mutation re-check costs O(stat) rather than a second full hash of every
/// placement on every sweep. `None` when the tree cannot be walked — which the caller must treat as
/// "moved", never as "unchanged" (an unreadable tree is exactly the one we must not destroy).
fn dir_fingerprint(dir: &Path) -> Option<Vec<DirStat>> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<DirStat>) -> Option<()> {
        for entry in std::fs::read_dir(d).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            let rel = path.strip_prefix(base).ok()?.to_string_lossy().into_owned();
            out.push((rel, crate::stat_cache::StatKey::from_metadata(&meta)));
            if meta.file_type().is_dir() {
                walk(base, &path, out)?;
            }
        }
        Some(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    // Sorted by PATH — directory order is not stable across filesystems, and the tuple's own
    // ordering is meaningless.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Some(out)
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

/// The final path component as `&str` — the leaf name the fd-anchored `*_at` ops take.
fn leaf_name(path: &Path) -> Result<&str, ClientError> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ClientError::PlacementUnsupported {
            reason: format!("{} has no usable final component", path.display()),
        })
}

/// [`FsOps::create_dir_nofollow`] under a PROVEN root, absorbing the one spelling wrinkle
/// [`contained_in`] names: a path recorded canonically under a root held raw (macOS `/var` vs
/// `/private/var`) strips against the CANONICAL root instead. Nothing is weakened — whichever
/// base matches, every component below it is fd-walked (`openat`/`mkdirat` from the parent's
/// held descriptor) one level at a time.
pub(crate) fn create_dir_contained(fs: &dyn FsOps, root: &Path, dir: &Path) -> std::io::Result<()> {
    if dir.strip_prefix(root).is_ok() {
        return fs.create_dir_nofollow(root, dir).map(|_| ());
    }
    match root.canonicalize() {
        Ok(canon) if dir.strip_prefix(&canon).is_ok() => {
            fs.create_dir_nofollow(&canon, dir).map(|_| ())
        }
        // fails with the not-under-base refusal
        _ => fs.create_dir_nofollow(root, dir).map(|_| ()),
    }
}

/// Resolve the placement to a real directory, or detect a first install, or refuse a non-directory.
fn resolve_target(
    fs: &dyn FsOps,
    placement_dir: &Path,
    kind: Option<PathKind>,
    project_root: Option<&Path>,
) -> Result<Target, ClientError> {
    match kind {
        None => {
            // First install: canonicalize the PARENT (must exist) so ancestor symlinks resolve, then
            // re-join the leaf. Create the parent if absent (the harness skills dir may not exist yet).
            // A project-scope parent is created with the NO-FOLLOW walk under the proven checkout —
            // `mkdir -p` through an ancestor swapped since the proof is refused, never followed.
            let parent_raw =
                placement_dir
                    .parent()
                    .ok_or_else(|| ClientError::PlacementUnsupported {
                        reason: "placement path has no parent directory".into(),
                    })?;
            match project_root {
                Some(root) => create_dir_contained(fs, root, parent_raw)?,
                None => fs.create_dir_all(parent_raw)?,
            }
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

/// Place new bytes over an existing directory, self-healing a stale `AtomicExchange` to
/// `RenameDance`. Returns the effective capability AND the path the OLD tree is now PARKED at —
/// both swap shapes park rather than delete, so the caller inspects those bytes before they go
/// (see [`verify_parked_old`]). Every namespace op runs at the caller's HELD parent handle
/// (verified against the path immediately before each op), so a parent swapped after the
/// containment proof refuses instead of re-aiming the op — and the staged SOURCE `src` (the
/// handle [`build_staging`] returned) is re-proven as the very directory this run built,
/// inside each op (`_src`): a substituted stage refuses, never lands, the staged bytes left to
/// the litter judge's park rail.
fn place_update(
    fs: &dyn FsOps,
    h: &crate::fs_seam::DirHandle,
    src: &crate::fs_seam::DirHandle,
    dir: &Path,
    parent: &Path,
    skill_id: &str,
    cap: SwapCapability,
) -> Result<(SwapCapability, PathBuf), ClientError> {
    let staging = src.path();
    let (staging_leaf, dir_leaf) = (leaf_name(staging)?, leaf_name(dir)?);
    match cap {
        SwapCapability::AtomicExchange => {
            match fs.exchange_at_src(h, staging_leaf, dir_leaf, src) {
                Ok(()) => {
                    fs.fsync_dir(parent)?;
                    // The swap PARKED the old bytes at the staging path — the caller judges them.
                    Ok((SwapCapability::AtomicExchange, staging.to_path_buf()))
                }
                Err(e) if is_unsupported(&e) => {
                    // The cached capability is stale (the placement moved onto a swap-incapable
                    // FS). Fall back to the rename-dance, reusing the already-built staging.
                    let parked = do_dance(fs, h, src, dir, parent, skill_id)?;
                    Ok((SwapCapability::RenameDance, parked))
                }
                Err(e) => Err(ClientError::Io(format!("atomic directory swap: {e}"))),
            }
        }
        SwapCapability::RenameDance => {
            let parked = do_dance(fs, h, src, dir, parent, skill_id)?;
            Ok((SwapCapability::RenameDance, parked))
        }
        SwapCapability::Unsupported => Err(ClientError::PlacementUnsupported {
            reason: "no safe directory swap on this filesystem".into(),
        }),
    }
}

/// The degraded fallback when no atomic swap exists: park the old dir, move the new in, hand the
/// park back. Each `rename` is atomic, so the dir is never *mixed*; between the two renames it is
/// briefly **absent** (the named, logged residual). A crash in that window leaves the dir absent →
/// the next pull takes the first-install branch and restores the new bytes (the old version is
/// still in the sidecar store).
fn do_dance(
    fs: &dyn FsOps,
    h: &crate::fs_seam::DirHandle,
    src: &crate::fs_seam::DirHandle,
    dir: &Path,
    parent: &Path,
    skill_id: &str,
) -> Result<PathBuf, ClientError> {
    let graveyard = graveyard_path(parent, skill_id);
    // The graveyard clear runs AT the held handle (identity-verified inside the op) — a leftover
    // was already judged by the litter rail this run; this only clears what that judgment left
    // droppable.
    fs.remove_dir_all_at(h, leaf_name(&graveyard)?)
        .map_err(|e| ClientError::Io(format!("rename-dance graveyard clear: {e}")))?;
    let (dir_leaf, grave_leaf, staging_leaf) = (
        leaf_name(dir)?,
        leaf_name(&graveyard)?,
        leaf_name(src.path())?,
    );
    // The staged SOURCE's identity, proven BEFORE the old tree is touched — a substituted stage
    // refuses here with the placement still in place (the install rename below re-proves it
    // in-op).
    src.verify_leaf_is(h, staging_leaf)
        .map_err(|e| ClientError::Io(format!("rename-dance stage check: {e}")))?;
    fs.rename_at(h, dir_leaf, grave_leaf)
        .map_err(|e| ClientError::Io(format!("rename-dance park old: {e}")))?;
    // --- the brief ABSENT (never mixed) window is between these two atomic renames ---
    fs.rename_at_noreplace_src(h, staging_leaf, dir_leaf, src)
        .map_err(|e| ClientError::Io(format!("rename-dance install new: {e}")))?;
    fs.fsync_dir(parent)?;
    Ok(graveyard)
}

/// What became of a judged park (see [`settle_park_among`]).
pub(crate) enum ParkFate {
    /// Every byte accounted for, two consecutive reads agreed — the park was removed (or had
    /// already vanished: a concurrent command concluded it first).
    Dropped,
    /// The bytes could not be dropped (unreadable, unaccountable, or still moving) and the park
    /// was renamed to the carried `.topos-kept-*` sibling no sweep touches.
    Kept(PathBuf),
    /// Same, but the rename ALSO failed — the park still sits under its original name. Nothing
    /// was deleted; the caller decides whether that name being occupied is fatal.
    Stuck,
}

/// PARK-THEN-VERIFY's judging half over an explicit ACCOUNTED set: account for every byte a
/// parked tree holds, then drop it — and only on TWO CONSECUTIVE AGREEING READS, the second
/// immediately before the removal. The one settle rail every recovery/apply deletion of a park
/// rides (command-start recovery included), so no path ever deletes on a single scan.
///
/// A rename takes the tree out of every PATH, but not away from an already-open file descriptor
/// (see the module doc's residual-window section): a process that opened a file before the park
/// can keep writing the renamed inode, so one scan is a judgment about bytes that may already be
/// gone. The loop re-reads until the tree holds still across two scans, absorbing every distinct
/// content it sees along the way:
///
/// - bytes whose digest sits in `accounted` (the target, a recorded baseline, an already-taken
///   snapshot — the caller says which), or that an earlier loop pass absorbed → accounted;
/// - anything else → snapshotted right then (the callers that destroy carry a snapshotter);
/// - unreadable, unaccountable with no snapshotter, or never settling within the bound → the park
///   is preserved (renamed to a `.topos-kept-*` sibling), never deleted;
/// - a park that VANISHED mid-judgment was concluded by a concurrent command — nothing left to
///   account for.
///
/// `anchor` — the HELD handle of the park's parent, where the caller has one: every pass
/// re-verifies the handle before its (path-based) content scan, the removal runs fd-anchored at
/// it ([`FsOps::remove_dir_all_at`]), and a preserve rename rides [`FsOps::rename_at`] — so a
/// parent whose path is swapped mid-judgment REFUSES (typed, nothing deleted through the
/// swapped spelling) instead of re-aiming the scan or the removal.
pub(crate) fn settle_park_among(
    fs: &dyn FsOps,
    parked: &Path,
    accounted: &[String],
    snapshot: Option<SnapshotFn<'_>>,
    anchor: Option<&crate::fs_seam::DirHandle>,
) -> Result<ParkFate, ClientError> {
    /// Two agreeing reads authorize the drop; a tree still moving after this many passes is
    /// preserved instead (an fd-writer that active never settles inside one command).
    const SETTLE_PASSES: usize = 4;
    let mut prev: Option<String> = None;
    let mut absorbed: Vec<String> = Vec::new();
    for _ in 0..SETTLE_PASSES {
        // The scan below is a PATH read — with a held anchor, prove the parent still is the
        // proven object immediately before it (the ops that mutate re-verify again themselves).
        if let Some(h) = anchor {
            h.verify_unmoved()
                .map_err(|e| ClientError::Io(format!("settle park parent check: {e}")))?;
        }
        if !fs.exists(parked) {
            return Ok(ParkFate::Dropped);
        }
        let Ok(scanned) = scan::scan(parked) else {
            // An unreadable park is not a park we may delete.
            return keep_parked(fs, parked, anchor);
        };
        let hex = to_hex(&scanned.bundle_digest);
        let is_accounted =
            accounted.iter().any(|a| a == &hex) || absorbed.iter().any(|a| a == &hex);
        if !is_accounted {
            match snapshot {
                Some(snapshot) => {
                    snapshot(&scanned)?;
                    absorbed.push(hex.clone());
                }
                None => return keep_parked(fs, parked, anchor),
            }
        }
        if prev.as_deref() == Some(hex.as_str()) {
            match anchor {
                Some(h) => fs
                    .remove_dir_all_at(h, leaf_name(parked)?)
                    .map_err(|e| ClientError::Io(format!("drop settled park: {e}")))?,
                None => fs.remove_dir_all(parked)?,
            }
            return Ok(ParkFate::Dropped);
        }
        prev = Some(hex);
    }
    keep_parked(fs, parked, anchor)
}

/// [`settle_park_among`] with the apply path's accounted shape: the target bytes, this
/// placement's recorded baseline, and what a snapshot already captured.
fn settle_park(
    fs: &dyn FsOps,
    parked: &Path,
    target_hex: &str,
    baseline: Option<&str>,
    captured: Option<&str>,
    snapshot: Option<SnapshotFn<'_>>,
    anchor: Option<&crate::fs_seam::DirHandle>,
) -> Result<ParkFate, ClientError> {
    let mut accounted: Vec<String> = vec![target_hex.to_owned()];
    accounted.extend(baseline.map(str::to_owned));
    accounted.extend(captured.map(str::to_owned));
    settle_park_among(fs, parked, &accounted, snapshot, anchor)
}

/// [`settle_park`] at the swap's own call site: whatever the fate, the apply proceeds — a Kept or
/// Stuck park holds preserved bytes beside the placement (the next apply's litter judge re-reads
/// a Stuck one), and the new bytes are already installed.
fn verify_parked_old(
    fs: &dyn FsOps,
    parked: &Path,
    target_hex: &str,
    baseline: Option<&str>,
    captured: Option<&str>,
    snapshot: Option<SnapshotFn<'_>>,
    anchor: Option<&crate::fs_seam::DirHandle>,
) -> Result<(), ClientError> {
    settle_park(fs, parked, target_hex, baseline, captured, snapshot, anchor).map(|_| ())
}

/// Move a park out of every sweep's reach and leave it — the "we could not account for these
/// bytes" arm. The kept name ladders past existing siblings (a prior kept park is never deleted
/// to make room); a park that cannot be renamed at all stays where it is (still not deleted) and
/// the caller learns it via [`ParkFate::Stuck`].
fn keep_parked(
    fs: &dyn FsOps,
    parked: &Path,
    anchor: Option<&crate::fs_seam::DirHandle>,
) -> Result<ParkFate, ClientError> {
    Ok(match preserve_park(fs, parked, anchor) {
        Some(kept) => ParkFate::Kept(kept),
        None => ParkFate::Stuck,
    })
}

/// Rename a park to a `.topos-kept-*` sibling no sweep touches, laddering past existing kept
/// siblings, and return the new path — `None` when it could not be moved (the park stays under
/// its own name; NOTHING is deleted either way). The shared preserve primitive recovery and the
/// litter judge use for bytes they cannot account for. With a held `anchor` (the park's parent)
/// the rename runs fd-anchored ([`FsOps::rename_at`]); recovery's journal arm, which meets
/// parks at arbitrary recorded paths, passes `None`.
pub(crate) fn preserve_park(
    fs: &dyn FsOps,
    parked: &Path,
    anchor: Option<&crate::fs_seam::DirHandle>,
) -> Option<PathBuf> {
    let name = parked
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dir".to_owned());
    let mut kept = parked.with_file_name(format!(".topos-kept-{name}"));
    let mut n = 1u32;
    while fs.exists(&kept) {
        n += 1;
        if n > 64 {
            return None;
        }
        kept = parked.with_file_name(format!(".topos-kept-{name}.{n}"));
    }
    match anchor {
        Some(h) => {
            let (from, to) = (leaf_name(parked).ok()?, leaf_name(&kept).ok()?);
            fs.rename_at(h, from, to).ok().map(|()| kept)
        }
        None => fs.rename(parked, &kept).ok().map(|()| kept),
    }
}

/// The write-boundary containment proof (see [`MaterializeReq::project_root`]): a project-scope
/// placement whose RECORDED path no longer provably resolves inside the checkout refuses, typed —
/// nothing staged, nothing swapped, nothing destroyed.
fn prove_containment(project_root: Option<&Path>, placement: &Path) -> Result<(), ClientError> {
    let Some(root) = project_root else {
        return Ok(());
    };
    if contained_in(root, placement) {
        return Ok(());
    }
    Err(ClientError::PlacementUnsupported {
        reason: crate::placement::escape_line("the placement", placement),
    })
}

/// [`crate::placement::within_project`] with the one spelling wrinkle a RECORD can carry: a
/// placement recorded in its CANONICAL form under a root held raw (macOS `/var` vs
/// `/private/var`) fails the lexical prefix while the containment is real — so a miss retries
/// against the canonicalized root before it counts. Both passes run the full proof (symlink-free
/// components + canonical containment); nothing is weakened, only the prefix spelling is aligned.
pub(crate) fn contained_in(root: &Path, candidate: &Path) -> bool {
    if crate::placement::within_project(root, candidate) {
        return true;
    }
    root.canonicalize()
        .is_ok_and(|canon| canon != root && crate::placement::within_project(&canon, candidate))
}

/// Whether ignore-file BYTES ignore the whole directory they sit in — a line that is exactly `*`
/// or `/*` (the node_modules/venv idiom the sentinel uses). The disclosure predicate for a bundle
/// shipping its OWN root ignore file: one that does NOT self-ignore leaves the placement visible
/// to git, and the sweep says so (bundle content is never edited to fix it).
pub(crate) fn ignores_all(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    text.lines().map(str::trim).any(|l| l == "*" || l == "/*")
}

/// Build a fresh staging dir holding the bundle's exact bytes, fsync every file AND every staging dir.
/// With `self_ignore`, the staged tree additionally carries the root self-ignore sentinel — UNLESS
/// the bundle ships its own root ignore file (a bundle's `.gitignore` is content, never overlaid).
/// Returns the staging root's HELD handle: the landing re-proves the staging leaf against it
/// (`_src`), so a stage substituted at its predictable name after this build cannot land.
///
/// `pub(crate)` for ONE other caller — [`crate::ops::merge_resolve`]'s conflict copy, which writes
/// a complete tree into the scope's own `conflicts/` dir under the same build-then-rename
/// discipline (there is no second staged-tree builder to drift from this one).
pub(crate) fn build_staging(
    fs: &dyn FsOps,
    parent_h: &crate::fs_seam::DirHandle,
    staging: &Path,
    bundle: &RenderedBundle,
    self_ignore: bool,
) -> Result<crate::fs_seam::DirHandle, ClientError> {
    // The staging name was cleared by the litter judge above (which refuses when a park occupying
    // it cannot be accounted for) — never deleted blind here: a leftover at this name is a PARK
    // from an interrupted run, and blind removal is exactly the loss the judge exists to prevent.
    if fs.exists(staging) {
        return Err(ClientError::PlacementUnsupported {
            reason: format!(
                "{} already exists and was not cleared by the litter judge; refusing to delete it \
                 blind",
                staging.display()
            ),
        });
    }
    // NO-FOLLOW, HANDLE-DESCENDED creates throughout the staged tree (see the module doc's
    // proof-to-write boundary): the staging dir is one fd-walked level below the HELD parent
    // handle, every file-parent below it descends from the STAGING handle's own fd — never the
    // stage's mutable pathname, so a stage swapped for an outward symlink mid-build cannot
    // re-aim the walk — and every file WRITE runs AT the held handle its walk returned
    // (`write_staged_at`: openat-exclusive, fd-fsyncs), so no path-based write survives below
    // the proven base.
    let staging_handle = fs.create_dir_nofollow_at(parent_h, staging)?;
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    dirs.insert(staging.to_path_buf());
    if self_ignore && !bundle.files.iter().any(|f| f.path == scan::IGNORE_FILE) {
        // The write rides the HELD handle the create walk returned (fd-fsyncs included) — no
        // path-based write below the proven parent.
        fs.write_staged_at(
            &staging_handle,
            scan::IGNORE_FILE,
            scan::IGNORE_SENTINEL,
            false,
        )?;
    }
    for f in &bundle.files {
        let dest = staging.join(&f.path);
        let file_parent = dest
            .parent()
            .ok_or_else(|| ClientError::PlacementUnsupported {
                reason: format!("{} has no parent directory", dest.display()),
            })?;
        // The walk DESCENDS FROM THE HELD STAGING HANDLE and returns the file-parent's own held
        // handle; the leaf write runs AT it — the stage's pathname is never re-resolved, so a
        // stage swapped for a symlink after its creation cannot re-aim the walk or the write.
        let parent_handle = fs.create_dir_nofollow_at(&staging_handle, file_parent)?;
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
        let leaf = leaf_name(&dest)?;
        fs.write_staged_at(
            &parent_handle,
            leaf,
            &f.bytes,
            f.mode == FileMode::Executable,
        )?;
    }
    for d in &dirs {
        fs.fsync_dir(d)?;
    }
    Ok(staging_handle)
}

// ---------------------------------------------------------------------------------------------
// PARK-THEN-VERIFY — the one primitive behind every destructive path
// ---------------------------------------------------------------------------------------------

/// Choose the unique sibling name a park of `dir` would take (the ladder past existing parks) —
/// split out of [`park_aside_journaled`] so the JOURNAL can record the name durably BEFORE the
/// rename.
fn park_name(fs: &dyn FsOps, dir: &Path, tag: &str) -> Result<PathBuf, ClientError> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dir".to_owned());
    let mut to = dir.with_file_name(format!(".topos-{tag}-{name}"));
    let mut n = 1u32;
    while fs.exists(&to) {
        n += 1;
        if n > 64 {
            return Err(ClientError::Io(format!(
                "park {}: too many `.topos-{tag}-*` siblings beside it — remove the stale ones \
                 first (they hold bytes topos declined to delete)",
                dir.display()
            )));
        }
        to = dir.with_file_name(format!(".topos-{tag}-{name}.{n}"));
    }
    Ok(to)
}

/// Move `dir` ASIDE, atomically, to a unique `.`-prefixed sibling — the first half of
/// park-then-verify — with a PARK-JOURNAL entry written durably BEFORE the rename.
///
/// Verify-then-delete cannot be made safe by checking harder: between the last look and the
/// `rm -rf` there is always a window, and whatever lands in it dies unexamined. A `rename` has no
/// such window — after it, the tree is out of every path anyone else can write through, and the
/// caller can inspect it at leisure and decide: drop it, snapshot it, or put it back. The sibling
/// name is UNIQUE (an existing park is never deleted to make room) and dot-prefixed (no harness
/// discovery reads it as a skill) — which is exactly why a park is invisible to every name-keyed
/// sweep, and why a crash between this rename and the caller's conclusion would otherwise strand
/// the tree's only bytes undisclosed. The journal closes that: the next run's recovery
/// ([`crate::sidecar::recover`]) restores the park to its original path, or preserves +
/// discloses it. The caller SETTLES the entry ([`crate::sidecar::settle_park_journal`]) once the
/// park is dropped or restored.
///
/// `owner` is the skill whose per-skill writer lock the calling operation HOLDS across this park —
/// recovery's liveness fence: while that lock is held, recovery leaves the entry alone rather than
/// restoring a live operation's park out from under it. `None` only for parks taken outside any
/// per-skill lock (a pre-adoption import destination).
///
/// # Errors
/// The journal write (fail-closed: an unjournalable park is not taken) or the rename.
pub(crate) fn park_aside_journaled(
    fs: &dyn FsOps,
    layout: &crate::sidecar::Layout,
    dir: &Path,
    tag: &str,
    restore: bool,
    owner: Option<&crate::id::SkillId>,
) -> Result<PathBuf, ClientError> {
    let to = park_name(fs, dir, tag)?;
    // ONE critical section: the entry-write AND the rename run under the journal lock. Recovery
    // takes the same lock for its whole read→act→rewrite, so the in-between state — an entry
    // whose park does not exist yet — is unobservable: without this, a concurrent recovery
    // landing in that beat reads the entry, finds no park on disk, concludes "absent =
    // concluded", drops the entry, and the rename that then follows strands the tree's only
    // bytes with no journal anywhere (sharpest for OWNERLESS parks, which no per-skill liveness
    // fence protects). Lock ORDER is unchanged (see `Layout::park_journal_lock_file`):
    // operations hold their per-skill lock and then this one; recovery holds this one and takes
    // per-skill locks only via TRY-lock — no cycle.
    let guard = fs.lock_exclusive(&layout.park_journal_lock_file())?;
    crate::sidecar::journal_park_locked(fs, layout, dir, &to, restore, owner)?;
    let renamed = fs.rename(dir, &to);
    match renamed {
        Ok(()) => {
            drop(guard);
            Ok(to)
        }
        Err(e) => {
            crate::sidecar::settle_park_journal_locked(fs, layout, &to);
            drop(guard);
            Err(ClientError::Io(format!("park {}: {e}", dir.display())))
        }
    }
}

/// Put a parked tree back where it came from — the "this run may not have it" arm. Best-effort by
/// contract: a destination that has since been re-created is NOT overwritten (the park stays on
/// disk, named, rather than clobbering whatever took its place), and the caller's refusal says so.
pub(crate) fn restore_parked(fs: &dyn FsOps, parked: &Path, orig: &Path) -> bool {
    !fs.exists(orig) && fs.rename(parked, orig).is_ok()
}

/// Clear the leftover staging / graveyard / probe siblings of THIS skill (idempotent).
///
/// The probe dirs are throwaway empties this materializer mints — removed blind. The staging and
/// graveyard names are PARKS: a crash between a prior run's swap and its `verify_parked_old`
/// strands the OLD tree there, raced edits included, so each is JUDGED with the settle rail —
/// accounted bytes drop, novel bytes are snapshotted (or the park is preserved as a
/// `.topos-kept-*` sibling), and a park that can neither be accounted for nor moved aside REFUSES
/// this placement rather than being deleted for occupying a name we need.
///
/// Every removal and preserve-rename runs AT the caller's HELD parent handle
/// ([`FsOps::remove_dir_all_at`] / [`FsOps::rename_at`]), and each content scan re-verifies it —
/// a proven parent whose pathname is swapped for an outward symlink before this sweep can no
/// longer aim a matching-name deletion outside the checkout: the handle check refuses instead.
fn cleanup_litter(
    fs: &dyn FsOps,
    h: &crate::fs_seam::DirHandle,
    parent: &Path,
    skill_id: &str,
    target_hex: &str,
    baseline: Option<&str>,
    snapshot: Option<SnapshotFn<'_>>,
) -> Result<(), ClientError> {
    for park in [
        staging_path(parent, skill_id),
        graveyard_path(parent, skill_id),
    ] {
        if !fs.exists(&park) {
            continue;
        }
        match settle_park(fs, &park, target_hex, baseline, None, snapshot, Some(h))? {
            ParkFate::Dropped | ParkFate::Kept(_) => {}
            ParkFate::Stuck => {
                return Err(ClientError::PlacementUnsupported {
                    reason: format!(
                        "{} holds bytes a prior interrupted run parked there that topos cannot \
                         account for, and it could not be moved aside — inspect or move it by \
                         hand before retrying",
                        park.display()
                    ),
                });
            }
        }
    }
    fs.remove_dir_all_at(h, leaf_name(&probe_path(parent, skill_id, 'a'))?)
        .map_err(|e| ClientError::Io(format!("clear probe dir: {e}")))?;
    fs.remove_dir_all_at(h, leaf_name(&probe_path(parent, skill_id, 'b'))?)
        .map_err(|e| ClientError::Io(format!("clear probe dir: {e}")))?;
    Ok(())
}

/// Probe the placement's filesystem ONCE for an atomic directory swap, by exchanging two throwaway
/// sibling directories. Any failure of the EXCHANGE itself (the syscall is unsupported, or
/// anything else) means "no atomic swap" → degrade to the rename-dance. Self-cleaning. Every
/// create/remove/exchange runs AT the caller's held parent handle, so a parent path swapped after
/// the containment proof refuses (typed) instead of minting or deleting probe dirs through the
/// swapped spelling.
fn probe_capability(
    fs: &dyn FsOps,
    h: &crate::fs_seam::DirHandle,
    parent: &Path,
    skill_id: &str,
) -> Result<SwapCapability, ClientError> {
    let a = probe_path(parent, skill_id, 'a');
    let b = probe_path(parent, skill_id, 'b');
    let (la, lb) = (leaf_name(&a)?.to_owned(), leaf_name(&b)?.to_owned());
    fs.remove_dir_all_at(h, &la)?;
    fs.remove_dir_all_at(h, &lb)?;
    fs.create_dir_at(h, &la)?;
    fs.create_dir_at(h, &lb)?;
    let supported = fs.exchange_at(h, &la, &lb).is_ok();
    fs.remove_dir_all_at(h, &la)?;
    fs.remove_dir_all_at(h, &lb)?;
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
            draft_observed: None,
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
                    adopted_source: false,
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
        let ok = RealFs
            .open_dir_handle(parent)
            .and_then(|h| RealFs.exchange_at(&h, ".swcheck-a", ".swcheck-b"))
            .is_ok();
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
            self_ignore: false,
            expected: None,
            project_root: None,
        }
    }

    /// The RACE cases for the pre-mutation re-stat: the placement's bytes move AFTER the pre-swap
    /// scan decided what to do with them, while the staging tree is being built.
    #[test]
    fn a_placement_that_moves_during_staging_is_captured_or_skipped_never_destroyed() {
        // (a) NO SNAPSHOTTER (the reset / merge callers, which snapshot before they call): the
        //     placement is SKIPPED rather than overwritten — its recorded state stays behind the
        //     bytes, so the next sweep reconciles it from a fresh scan.
        let parent = Scratch::new("race-skip");
        let home = Scratch::new("race-skip-home");
        let placement = parent.0.join("demo");
        install_old(&placement);
        let bundle = rendered(NEW);
        let lock = lock_of("topos_race", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(&[&placement], &digest_hex(OLD), SwapCapability::Unsupported);
        let d = docs_under(&home.0, "topos_race");
        // The racer writes into the placement the first time a staged file is written — i.e.
        // inside the window between the pre-swap scan and the swap.
        let racing = placement.clone();
        let fs = crate::fs_seam::HookFs::new(move || {
            std::fs::write(racing.join("SKILL.md"), b"# raced in\n").unwrap();
        });
        materialize(
            &fs,
            &req("topos_race", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(placement.join("SKILL.md")).unwrap(),
            b"# raced in\n",
            "the bytes that moved in the window are still there"
        );
        assert!(
            placement.join("legacy.txt").exists(),
            "the placement was skipped whole, not half-swapped"
        );

        // (b) WITH a snapshotter (the ordinary sweep): the bytes that are REALLY there are
        //     captured — by a snapshot taken after the last revalidation — and only then replaced.
        let parent = Scratch::new("race-snap");
        let home = Scratch::new("race-snap-home");
        let placement = parent.0.join("demo");
        install_old(&placement);
        let prior = prior_map(&[&placement], &digest_hex(OLD), SwapCapability::Unsupported);
        let d = docs_under(&home.0, "topos_race2");
        let racing = placement.clone();
        let fs = crate::fs_seam::HookFs::new(move || {
            std::fs::write(racing.join("SKILL.md"), b"# raced in\n").unwrap();
        });
        let captured: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let snapshot = |b: &ScannedBundle| -> Result<(), ClientError> {
            captured.borrow_mut().push(digest::to_hex(&b.bundle_digest));
            Ok(())
        };
        let mut request = req("topos_race2", &[0], &bundle, &prior, &lock, &sync, &d.sp);
        request.snapshot = Some(&snapshot);
        materialize(&fs, &request).unwrap();
        assert_eq!(
            dir_snapshot(&placement),
            Some(expected(NEW)),
            "the swap happened once the raced bytes were captured"
        );
        // The RACED digest is what was captured — the pre-swap scan's older one would be a lie.
        let raced = {
            let mut files: Vec<(String, FileMode, Vec<u8>)> = OLD
                .iter()
                .map(|(p, m, b)| ((*p).to_owned(), *m, b.to_vec()))
                .collect();
            for f in &mut files {
                if f.0 == "SKILL.md" {
                    f.2 = b"# raced in\n".to_vec();
                }
            }
            let entries: Vec<ManifestEntry> = files
                .iter()
                .map(|(p, m, b)| ManifestEntry {
                    path: p.clone(),
                    mode: *m,
                    content_sha256: digest::sha256(b),
                })
                .collect();
            digest::to_hex(&digest::bundle_digest(&entries).unwrap())
        };
        assert!(
            captured.borrow().contains(&raced),
            "the snapshot holds what was actually destroyed: {:?}",
            captured.borrow()
        );
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

    /// Whether ignore bytes self-ignore the directory (`*` / `/*` on a line of their own).
    #[test]
    fn ignores_all_reads_only_whole_dir_patterns() {
        assert!(ignores_all(b"*\n"));
        assert!(ignores_all(b"# note\n/*\n"));
        assert!(ignores_all(crate::scan::IGNORE_SENTINEL));
        assert!(!ignores_all(b"*.log\n"));
        assert!(!ignores_all(b"build/\n"));
        assert!(!ignores_all(b""));
    }

    /// `git init` + `git status --porcelain` over a repo — the REAL visibility witness. `None`
    /// when no usable git binary is on this machine (the caller skips).
    fn git_status(repo: &Path) -> Option<String> {
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .output()
            .ok()?;
        if !init.status.success() {
            return None;
        }
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain", "-uall"])
            .current_dir(repo)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A self-ignoring apply stages the sentinel beside the bundle — a first install lands it, the
    /// scanner treats it as metadata (the placed dir reads clean), GIT genuinely does not see the
    /// placement, and a bundle shipping its OWN root ignore file is placed verbatim (never
    /// overlaid) — visible to git when that file does not self-ignore.
    #[test]
    fn self_ignore_stages_the_sentinel_unless_the_bundle_ships_one() {
        let parent = Scratch::new("selfig");
        let home = Scratch::new("selfig-home");
        let placement = parent.0.join("demo");
        let bundle = rendered(NEW);
        let lock = lock_of("topos_sig", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::Unsupported);
        prior.placement_state[0].materialized_sha = None;
        let d = docs_under(&home.0, "topos_sig");
        let mut r = req("topos_sig", &[0], &bundle, &prior, &lock, &sync, &d.sp);
        r.self_ignore = true;
        materialize(&RealFs, &r).unwrap();
        assert_eq!(
            std::fs::read(placement.join(crate::scan::IGNORE_FILE)).unwrap(),
            crate::scan::IGNORE_SENTINEL,
            "the placed project dir self-ignores"
        );
        // The scanner reads the placed dir as EXACTLY the bundle (the sentinel is metadata).
        assert_eq!(
            scan::scan(&placement).unwrap().bundle_digest,
            bundle.bundle_digest
        );

        // A bundle carrying its own root ignore file keeps it byte-exact — no overlay.
        let own: &[(&str, FileMode, &[u8])] = &[
            (".gitignore", FileMode::Regular, b"*.log\n"),
            ("SKILL.md", FileMode::Regular, b"# own\n"),
        ];
        let placement2 = parent.0.join("own");
        let bundle2 = rendered(own);
        let lock2 = lock_of("topos_sig2", own, &"1".repeat(64));
        let sync2 = sync_at(1, 1, &"1".repeat(64), &digest_hex(own));
        let mut prior2 = prior_map(&[&placement2], &"0".repeat(64), SwapCapability::Unsupported);
        prior2.placement_state[0].materialized_sha = None;
        let d2 = docs_under(&home.0, "topos_sig2");
        let mut r2 = req(
            "topos_sig2",
            &[0],
            &bundle2,
            &prior2,
            &lock2,
            &sync2,
            &d2.sp,
        );
        r2.self_ignore = true;
        materialize(&RealFs, &r2).unwrap();
        assert_eq!(
            std::fs::read(placement2.join(".gitignore")).unwrap(),
            b"*.log\n",
            "the bundle's own ignore file is content, never replaced by the sentinel"
        );

        // The REAL visibility check: git itself must not see the sentinel placement, and MUST
        // see the shipped-ignore one (its `*.log` does not self-ignore) — the assertion that was
        // previously vacuous.
        match git_status(&parent.0) {
            Some(status) => {
                assert!(
                    !status.contains("demo"),
                    "the sentinel placement is invisible to git:\n{status}"
                );
                assert!(
                    status.contains("own"),
                    "a shipped non-self-ignoring root ignore leaves the placement visible:\n{status}"
                );
            }
            None => eprintln!("skipping git-visibility half: no usable git binary"),
        }
    }

    /// The opportunistic arm: a target whose bytes moved between the caller's scan and the swap is
    /// SKIPPED (nothing recorded, nothing overwritten, no error), while a target still holding the
    /// expected bytes lands.
    #[test]
    fn an_expected_mismatch_skips_the_target_instead_of_refusing() {
        let parent = Scratch::new("expect");
        let home = Scratch::new("expect-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let steady = parent.0.join("steady").join("demo");
        let moved = parent.0.join("moved").join("demo");
        install_old(&steady);
        install_old(&moved);
        // The caller observed OLD in both; then "moved" changes in the window.
        std::fs::write(moved.join("SKILL.md"), b"# raced edit\n").unwrap();
        let bundle = rendered(NEW);
        let lock = lock_of("topos_exp", NEW, &"1".repeat(64));
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &[&steady, &moved],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_exp");
        let exp_rows = vec![
            (0usize, Some(digest_hex(OLD))),
            (1usize, Some(digest_hex(OLD))),
        ];
        let mut r = req("topos_exp", &[0, 1], &bundle, &prior, &lock, &sync, &d.sp);
        r.expected = Some(&exp_rows);
        materialize(&RealFs, &r).unwrap();
        assert_eq!(dir_snapshot(&steady), Some(expected(NEW)));
        assert_eq!(
            std::fs::read(moved.join("SKILL.md")).unwrap(),
            b"# raced edit\n",
            "the raced dir is skipped, its bytes untouched"
        );
        let m = crate::doc::read_map(&RealFs, &d.sp.map).unwrap().unwrap();
        assert_eq!(
            m.placement_state[0].materialized_sha.as_deref(),
            Some(digest_hex(NEW).as_str())
        );
        assert_eq!(
            m.placement_state[1].materialized_sha.as_deref(),
            Some(digest_hex(OLD).as_str()),
            "the skipped target's record is untouched"
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

    /// THE FD WINDOW (the module doc's residual): a rename takes a tree out of every path, but a
    /// process that opened a file BEFORE the park can keep writing the renamed inode. The settle
    /// rail must therefore re-read the park until two consecutive scans agree, absorbing every
    /// distinct content it sees — a write landing between the judging scan and the removal is
    /// captured, never destroyed. The snapshot closure plays the fd-writer here: it fires the
    /// instant after the scan that judged the park, exactly where an open-fd write would land.
    #[test]
    fn a_park_written_through_an_open_fd_after_the_rename_is_absorbed_not_destroyed() {
        let parent = Scratch::new("fd-settle");
        let parked = parent.0.join(".topos-old-demo");
        install_old(&parked); // novel relative to both target and baseline below
        let fd_write: &[u8] = b"# written through a pre-park fd\n";
        let mutated = RefCell::new(false);
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let snap = |s: &ScannedBundle| -> Result<(), ClientError> {
            seen.borrow_mut().push(digest::to_hex(&s.bundle_digest));
            if !*mutated.borrow() {
                *mutated.borrow_mut() = true;
                // The "fd write": lands AFTER scan(parked) judged the tree, BEFORE the removal.
                std::fs::write(parked.join("SKILL.md"), fd_write).unwrap();
            }
            Ok(())
        };
        verify_parked_old(
            &RealFs,
            &parked,
            &digest_hex(NEW),
            Some(&"0".repeat(64)),
            None,
            Some(&snap),
            None,
        )
        .unwrap();
        assert!(!parked.exists(), "the settled park was dropped in the end");
        // BOTH contents were captured: the pre-write tree AND the tree the fd write produced.
        let after_write = {
            let mut files: Vec<(String, FileMode, Vec<u8>)> = OLD
                .iter()
                .map(|(p, m, b)| ((*p).to_owned(), *m, b.to_vec()))
                .collect();
            for f in &mut files {
                if f.0 == "SKILL.md" {
                    f.2 = fd_write.to_vec();
                }
            }
            let entries: Vec<ManifestEntry> = files
                .iter()
                .map(|(p, m, b)| ManifestEntry {
                    path: p.clone(),
                    mode: *m,
                    content_sha256: digest::sha256(b),
                })
                .collect();
            digest::to_hex(&digest::bundle_digest(&entries).unwrap())
        };
        let seen = seen.borrow();
        assert!(
            seen.contains(&digest_hex(OLD)),
            "pre-write tree captured: {seen:?}"
        );
        assert!(
            seen.contains(&after_write),
            "the fd write was captured too: {seen:?}"
        );
    }

    /// P1: the held parent handle governs the LITTER SWEEP and the CAPABILITY PROBES, not just
    /// the final landing move. The swap lands at the EXACT boundary the finding names —
    /// immediately AFTER the handle is pinned, BEFORE `cleanup_litter`/`probe_capability` run —
    /// replacing the proven parent's pathname with an outward symlink whose target carries
    /// matching-name litter and probe dirs. Every scan/removal in that sweep must refuse at the
    /// handle check rather than resolve through the swapped path: the victim's dirs survive.
    #[test]
    fn a_parent_swapped_after_the_pin_cannot_aim_the_litter_sweep_or_probes_outside() {
        let parent = Scratch::new("pin-swap");
        let victim = Scratch::new("pin-swap-victim");
        let home = Scratch::new("pin-swap-home");
        let placement = parent.0.join("demo");
        install_old(&placement);
        let parent_canon = parent.0.canonicalize().unwrap();
        let placement_canon = parent_canon.join("demo");
        // Look-alike litter OUTSIDE the proven parent, under the exact names the sweep acts on.
        let v_staging = victim.0.join(".topos-staging-topos_pinswap1");
        let v_grave = victim.0.join(".topos-old-topos_pinswap1");
        let v_probe_a = victim.0.join(".topos-probe-topos_pinswap1-a");
        let v_probe_b = victim.0.join(".topos-probe-topos_pinswap1-b");
        for d in [&v_staging, &v_grave, &v_probe_a, &v_probe_b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("KEEP.md"), b"# not topos's to delete\n").unwrap();
        }

        let bundle = rendered(NEW);
        let d = docs_under(&home.0, "topos_pinswap1");
        let lock = lock_of("topos_pinswap1", NEW, &"1".repeat(64));
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        // Capability deliberately Unsupported so the probe arm WOULD run — were the sweep not
        // refused at the handle first.
        let prior = prior_map(
            &[&placement_canon],
            &digest_hex(OLD),
            SwapCapability::Unsupported,
        );
        let moved = parent_canon.with_file_name(format!(
            "{}-moved",
            parent_canon.file_name().unwrap().to_string_lossy()
        ));
        let (pc, mv, vic) = (parent_canon.clone(), moved.clone(), victim.0.clone());
        let fs = crate::fs_seam::HookFs::after_dir_handle_open(&parent_canon, move || {
            std::fs::rename(&pc, &mv).unwrap();
            std::os::unix::fs::symlink(&vic, &pc).unwrap();
        });

        let err = materialize(
            &fs,
            &req("topos_pinswap1", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        );
        assert!(err.is_err(), "the swapped parent refuses the apply");
        // NOTHING outside the proven parent was scanned into a deletion: every victim dir and
        // its bytes survive.
        for dir in [&v_staging, &v_grave, &v_probe_a, &v_probe_b] {
            assert_eq!(
                std::fs::read(dir.join("KEEP.md")).unwrap(),
                b"# not topos's to delete\n",
                "{} was touched",
                dir.display()
            );
        }
        // And the real (moved) parent still holds the placement, bytes intact.
        assert_eq!(
            std::fs::read(moved.join("demo").join("SKILL.md")).unwrap(),
            b"# old\n"
        );
        let _ = std::fs::remove_dir_all(&moved);
    }

    /// With NO snapshotter, unaccountable bytes are PRESERVED — the park is renamed to a
    /// `.topos-kept-*` sibling no sweep touches, never deleted.
    #[test]
    fn an_unaccountable_park_is_preserved_never_deleted() {
        let parent = Scratch::new("keep");
        let parked = parent.0.join(".topos-old-demo");
        install_old(&parked);
        verify_parked_old(&RealFs, &parked, &digest_hex(NEW), None, None, None, None).unwrap();
        assert!(!parked.exists(), "the park left its original name");
        let kept = parent.0.join(".topos-kept-.topos-old-demo");
        assert_eq!(
            dir_snapshot(&kept),
            Some(expected(OLD)),
            "the bytes sit whole under the kept name"
        );
    }

    /// THE CRASH FINDING: an edit lands immediately before the swap, and the run dies before
    /// `verify_parked_old` concludes (here: the snapshot fails — the same window a crash or an
    /// fsync fault opens). The park then holds the ONLY copy of the raced edit — and the next
    /// materialization's litter judge must absorb it, never delete it on its name.
    #[test]
    fn a_crash_before_park_verification_never_loses_an_edit_raced_in_before_the_swap() {
        let parent = Scratch::new("crash-park");
        let home = Scratch::new("crash-park-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let placement = parent.0.join("demo");
        install_old(&placement);
        let placement_canon = placement.canonicalize().unwrap();
        let bundle = rendered(NEW);
        let lock = lock_of("topos_crashp", NEW, &"1".repeat(64));
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &[&placement],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_crashp");
        // The RACE: the edit lands immediately before the exchange — after the pre-mutation
        // re-stat, so the park (not the pre-swap scan) is the only thing that ever sees it.
        let raced: &[u8] = b"# raced in just before the swap\n";
        let racing = placement_canon.clone();
        let fs = crate::fs_seam::HookFs::before_first_move_of(&placement_canon, move || {
            std::fs::write(racing.join("SKILL.md"), raced).unwrap();
        });
        // The CRASH: the snapshot that would absorb the parked edit fails — verify never
        // concludes, the run errors out, and the park stays behind.
        let crashing = |_: &ScannedBundle| -> Result<(), ClientError> {
            Err(ClientError::Io("injected crash".into()))
        };
        let mut r = req("topos_crashp", &[0], &bundle, &prior, &lock, &sync, &d.sp);
        r.snapshot = Some(&crashing);
        materialize(&fs, &r).unwrap_err();
        let park = staging_path(placement_canon.parent().unwrap(), "topos_crashp");
        assert_eq!(
            std::fs::read(park.join("SKILL.md")).unwrap(),
            raced,
            "the park holds the raced edit's only copy"
        );

        // The NEXT materialization judges the park through the litter rail: the raced bytes are
        // snapshotted (absorbed) before the name is reused — the old code deleted them blind.
        let captured: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let absorbing = |s: &ScannedBundle| -> Result<(), ClientError> {
            captured.borrow_mut().push(digest::to_hex(&s.bundle_digest));
            Ok(())
        };
        let mut r2 = req("topos_crashp", &[0], &bundle, &prior, &lock, &sync, &d.sp);
        r2.snapshot = Some(&absorbing);
        materialize(&RealFs, &r2).unwrap();
        let raced_digest = {
            let mut files: Vec<(String, FileMode, Vec<u8>)> = OLD
                .iter()
                .map(|(p, m, b)| ((*p).to_owned(), *m, b.to_vec()))
                .collect();
            for f in &mut files {
                if f.0 == "SKILL.md" {
                    f.2 = raced.to_vec();
                }
            }
            let entries: Vec<ManifestEntry> = files
                .iter()
                .map(|(p, m, b)| ManifestEntry {
                    path: p.clone(),
                    mode: *m,
                    content_sha256: digest::sha256(b),
                })
                .collect();
            digest::to_hex(&digest::bundle_digest(&entries).unwrap())
        };
        assert!(
            captured.borrow().contains(&raced_digest),
            "the litter judge absorbed the stranded edit: {:?}",
            captured.borrow()
        );
        assert_eq!(dir_snapshot(&placement_canon), Some(expected(NEW)));
        assert!(!park.exists(), "the judged park was then dropped");
    }

    /// The write-boundary containment rail (project scope): a `.claude` that became a symlink out
    /// of the checkout AFTER planning refuses at materialize time — nothing is created or written
    /// through the link.
    #[test]
    fn a_project_placement_whose_ancestor_became_a_symlink_refuses_at_the_write_boundary() {
        let proj = Scratch::new("escape-proj");
        let outside = Scratch::new("escape-outside");
        std::os::unix::fs::symlink(&outside.0, proj.0.join(".claude")).unwrap();
        let placement = proj.0.join(".claude").join("skills").join("demo");
        let home = Scratch::new("escape-home");
        let bundle = rendered(NEW);
        let lock = lock_of("topos_esc", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::Unsupported);
        prior.placement_state[0].materialized_sha = None;
        let d = docs_under(&home.0, "topos_esc");
        let mut r = req("topos_esc", &[0], &bundle, &prior, &lock, &sync, &d.sp);
        r.project_root = Some(&proj.0);
        let err = materialize(&RealFs, &r).unwrap_err();
        assert!(
            err.to_string().contains("PLACEMENT_ESCAPES_PROJECT"),
            "{err}"
        );
        assert!(
            !outside.0.join("skills").exists(),
            "nothing was created through the symlink"
        );
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

    /// P1: the NO-FOLLOW creation walk — a symlink component refuses the whole create (nothing
    /// lands beyond it), a `..` climb refuses, and an ordinary nested create still works.
    #[test]
    fn create_dir_nofollow_refuses_symlink_components_and_climbs() {
        let base = Scratch::new("nofollow");
        let victim = Scratch::new("nofollow-victim");

        // Ordinary nested create works.
        RealFs
            .create_dir_nofollow(&base.0, &base.0.join("a").join("b").join("c"))
            .unwrap();
        assert!(base.0.join("a/b/c").is_dir());

        // A symlink component is met as itself and refused — nothing created through it.
        std::os::unix::fs::symlink(&victim.0, base.0.join("link")).unwrap();
        let err = RealFs.create_dir_nofollow(&base.0, &base.0.join("link").join("inner"));
        assert!(err.is_err(), "a symlink component must refuse");
        assert_eq!(
            std::fs::read_dir(&victim.0).unwrap().count(),
            0,
            "nothing was created through the link"
        );

        // A `..` climb out of the base refuses outright.
        let err = RealFs.create_dir_nofollow(&base.0, &base.0.join("..").join("escape"));
        assert!(err.is_err(), "a climb out of the base must refuse");
    }

    /// P1: THE PROOF-TO-WRITE ANCHOR. The placement parent is swapped for an outward symlink
    /// AFTER the containment proof and the staging build, immediately before the landing rename —
    /// the held parent handle detects the swap and the landing REFUSES: nothing is written
    /// through the link, and the staged bytes stay whole inside the (moved) real parent.
    #[test]
    fn a_parent_swapped_after_the_proof_refuses_the_landing_rename() {
        let parent = Scratch::new("swap-anchor");
        let home = Scratch::new("swap-anchor-home");
        let victim = Scratch::new("swap-anchor-victim");
        let skills = parent.0.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let skills_canon = skills.canonicalize().unwrap();
        let placement = skills.join("demo"); // absent → first install
        let bundle = rendered(NEW);
        let lock = lock_of("topos_swapp1", NEW, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NEW));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::RenameDance);
        prior.placement_state[0].materialized_sha = None;
        let d = docs_under(&home.0, "topos_swapp1");

        // The swap: the REAL parent is moved aside and an outward symlink takes its place, in
        // the beat between the staging build and the landing rename.
        let staging = skills_canon.join(".topos-staging-topos_swapp1");
        let moved = parent.0.join("moved-aside");
        let (link_from, link_to, moved_to) =
            (skills_canon.clone(), victim.0.clone(), moved.clone());
        let fs = crate::fs_seam::HookFs::before_first_move_of(&staging, move || {
            std::fs::rename(&link_from, &moved_to).unwrap();
            std::os::unix::fs::symlink(&link_to, &link_from).unwrap();
        });

        let err = materialize(
            &fs,
            &req("topos_swapp1", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        );
        assert!(err.is_err(), "the swapped parent must refuse the landing");
        assert_eq!(
            std::fs::read_dir(&victim.0).unwrap().count(),
            0,
            "nothing landed through the symlink"
        );
        assert_eq!(
            dir_snapshot(&moved.join(".topos-staging-topos_swapp1")),
            Some(expected(NEW)),
            "the staged bytes stay whole inside the moved real parent — preserved, not deleted"
        );
    }

    /// P1: the staging BUILD must never re-resolve the stage's mutable pathname. The stage is
    /// swapped for an outward symlink AFTER its creation — at the exact beat before the walk
    /// that creates a nested file-parent, BEFORE the first staged write — where a path-based
    /// `create_dir_nofollow` (its base open carries only `O_DIRECTORY`) would follow the link
    /// and land bundle bytes outside the proven parent. The fd-descended walk keeps every
    /// create inside the directory object this run built, and the staged write's identity check
    /// refuses the build: typed refusal, zero bytes outside.
    #[test]
    fn a_stage_swapped_for_an_outward_symlink_mid_build_cannot_aim_writes_outside() {
        let parent = Scratch::new("stage-swap");
        let home = Scratch::new("stage-swap-home");
        let victim = Scratch::new("stage-swap-victim");
        let skills = parent.0.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let skills_canon = skills.canonicalize().unwrap();
        let placement = skills.join("demo"); // absent → first install
        const NESTED: &[(&str, FileMode, &[u8])] =
            &[("ref/data.txt", FileMode::Regular, b"nested new\n")];
        let bundle = rendered(NESTED);
        let lock = lock_of("topos_stswap1", NESTED, &"1".repeat(64));
        let sync = sync_at(1, 1, &"1".repeat(64), &digest_hex(NESTED));
        let mut prior = prior_map(&[&placement], &"0".repeat(64), SwapCapability::RenameDance);
        prior.placement_state[0].materialized_sha = None;
        let d = docs_under(&home.0, "topos_stswap1");
        let staging = skills_canon.join(".topos-staging-topos_stswap1");
        let moved = parent.0.join("stolen-stage");
        let (st, mv, vic) = (staging.clone(), moved.clone(), victim.0.clone());
        // The swap lands immediately before the nested file-parent's create walk — after the
        // stage was created, before the first staged write.
        let fs =
            crate::fs_seam::HookFs::before_nth_create_dir_all(&staging.join("ref"), 1, move || {
                std::fs::rename(&st, &mv).unwrap();
                std::os::unix::fs::symlink(&vic, &st).unwrap();
            });
        let err = materialize(
            &fs,
            &req("topos_stswap1", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        );
        assert!(err.is_err(), "the swapped stage must refuse the build");
        assert_eq!(
            std::fs::read_dir(&victim.0).unwrap().count(),
            0,
            "zero bytes landed outside — the symlink was never followed"
        );
        assert!(!placement.exists(), "nothing was installed");
    }

    /// P1: the landing verifies the STAGE's leaf identity, not just the parent's. The completed
    /// stage is moved aside and a substitute directory put at its predictable name immediately
    /// BEFORE the landing exchange (inside the op, after the before-move beat) — the in-op
    /// source proof refuses: the old placement is untouched, the substitute never lands, and no
    /// digest is recorded.
    #[test]
    fn a_stage_substituted_at_its_leaf_before_the_exchange_refuses_and_lands_nothing() {
        let parent = Scratch::new("leaf-sub");
        let home = Scratch::new("leaf-sub-home");
        if !swap_supported(&parent.0) {
            eprintln!("skipping: temp FS lacks atomic dir exchange");
            return;
        }
        let placement = parent.0.join("demo");
        install_old(&placement);
        let placement_canon = placement.canonicalize().unwrap();
        let parent_canon = placement_canon.parent().unwrap().to_path_buf();
        let bundle = rendered(NEW);
        let lock = lock_of("topos_leafsub1", NEW, &"1".repeat(64));
        let sync = sync_at(2, 2, &"1".repeat(64), &digest_hex(NEW));
        let prior = prior_map(
            &[&placement],
            &digest_hex(OLD),
            SwapCapability::AtomicExchange,
        );
        let d = docs_under(&home.0, "topos_leafsub1");
        let staging = parent_canon.join(".topos-staging-topos_leafsub1");
        let stolen = parent_canon.join("stolen-stage");
        let (st, sto) = (staging.clone(), stolen.clone());
        // The substitution lands INSIDE the landing op, immediately before the exchange syscall.
        let fs = crate::fs_seam::HookFs::before_first_move_of(&placement_canon, move || {
            std::fs::rename(&st, &sto).unwrap();
            std::fs::create_dir_all(&st).unwrap();
            std::fs::write(st.join("SKILL.md"), b"# substituted\n").unwrap();
        });
        let err = materialize(
            &fs,
            &req("topos_leafsub1", &[0], &bundle, &prior, &lock, &sync, &d.sp),
        );
        assert!(err.is_err(), "a substituted stage must refuse the landing");
        assert_eq!(
            dir_snapshot(&placement_canon),
            Some(expected(OLD)),
            "the old placement is untouched"
        );
        assert_eq!(
            std::fs::read(staging.join("SKILL.md")).unwrap(),
            b"# substituted\n",
            "the substitute never landed and was not deleted"
        );
        assert!(
            crate::doc::read_map(&RealFs, &d.sp.map).unwrap().is_none(),
            "no digest was recorded for a landing that refused"
        );
    }
}
