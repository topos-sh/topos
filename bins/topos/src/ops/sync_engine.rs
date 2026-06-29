//! The per-skill sync engine: `checkForUpdates → plan → apply`, crash-safe and downgrade-proof.
//!
//! For one followed skill, under its writer flock, the engine:
//! 1. **checkForUpdates** — conditional-GET the signed `current` pointer, authenticate it (signature +
//!    workspace/skill scope), evaluate it against the durable anti-rollback floor `observed` (raising the
//!    floor durably ONLY on a verified strictly-higher record), and raise a loud ALARM on a reused tuple.
//! 2. **plan** — drive toward `observed`: classify the working tree (clean / draft / absent / unscannable),
//!    snapshot a draft FIRST, fetch the target's bytes, re-verify them (digest == tree == `commit_id`),
//!    record them durably in the sidecar store, then refine (a crash-after-swap heals, never a false
//!    divergence), and map the situation to a `consent::Situation`.
//! 3. **apply** — act on `consent::decide()`: materialize + advance `applied` (auto / explicit accept),
//!    offer (confirm-each), or snapshot + surface the DIVERGED panel (never clobber).
//!
//! `applied` advances only after a successful swap; `observed` is the floor a follower never crosses. The
//! consent decision is the kernel's one policy — the engine only chooses which row to feed it.

use std::path::Path;

use topos_core::digest::{self, to_hex};
use topos_core::sign::{self, Commit};
use topos_core::sync::{self, ApplyClass, Generation as KGen};
use topos_gitstore::{ImportFile, Store};
use topos_types::Generation;
use topos_types::persisted::{Lock, LockedFile, PlacementMap, RecordedTuple, SyncState};
use topos_types::results::{Conflict, Offer, PullAction, PullSkill};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::materialize::{self, MaterializeReport, MaterializeReq, NextMapCore};
use crate::plane::{FollowContext, FollowMode, PlaneError, PointerFetch, gen_cmp};
use crate::scan::{self, ScannedBundle};
use crate::{doc, logfile, sidecar};

/// The fixed commit message for a draft snapshot (folded into its `version_id`; must stay constant).
const DRAFT_SNAPSHOT_MESSAGE: &str = "topos: draft snapshot";
/// A bound on ancestor backfill — far beyond any real lineage gap; stops a forged cyclic store.
const MAX_BACKFILL: usize = 256;

/// Bring one followed skill current (the sweep + the explicit-accept path).
///
/// `explicit` is `true` for a targeted `topos pull <skill>` (the user's command supplies consent, so a
/// confirm-each skill applies rather than merely offers, and a `held` pin is released); `false` for the
/// bare session-start sweep.
pub(crate) fn sync_one(
    ctx: &Ctx<'_>,
    skill_id: &str,
    follow: &FollowContext,
    explicit: bool,
) -> Result<PullSkill, ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let mut sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let map: PlacementMap = read_required(ctx, &sp.map, "map.json")?;
    validate_recorded_unique(&sync.recorded)?;
    let name = lock.name.clone();

    // An explicit pull resumes a held skill (clears the one-FF suppression); persisted with the next write.
    let mut dirty = false;
    if explicit && sync.held {
        sync.held = false;
        dirty = true;
    }

    // ---- checkForUpdates ----
    match ctx.plane.get_current(skill_id, Some(sync.observed)) {
        Ok(PointerFetch::NotModified) => {}
        Ok(PointerFetch::Record(rec)) => {
            let Some(authed) = authenticate(&rec, skill_id, follow, &ctx.plane_key) else {
                persist_if_dirty(ctx, &sp, &sync, dirty)?;
                return Ok(alarm(&name, &sync, PullAction::Alarm));
            };
            match sync::evaluate_floor(
                kgen(authed.generation),
                authed.version_id,
                kgen(sync.observed),
                &kernel_recorded(&sync)?,
            ) {
                v if v.is_alarm() => {
                    persist_if_dirty(ctx, &sp, &sync, dirty)?;
                    return Ok(alarm(&name, &sync, PullAction::Alarm));
                }
                sync::FloorVerdict::Forward => {
                    // A verified, strictly-higher record raises the floor — durable NOW (it must survive a
                    // failed apply as the retry target), independent of whether the apply succeeds.
                    sync.observed = authed.generation;
                    sync.recorded.push(RecordedTuple {
                        generation: authed.generation,
                        commit_id: to_hex(&authed.version_id),
                    });
                    doc::write_doc(ctx.fs, &sp.sync, &sync)?;
                    dirty = false;
                }
                sync::FloorVerdict::CorruptNoRecord => {
                    return Err(ClientError::Corrupt(
                        "current record at the floor names no recorded commit".into(),
                    ));
                }
                // Replay / StaleReplay / RefuseBelowFloor: no floor change; fall through to drive applied.
                _ => {}
            }
        }
        Err(PlaneError::NotFound) => {
            persist_if_dirty(ctx, &sp, &sync, dirty)?;
            return Ok(state_row(&name, &sync, PullAction::UpToDate));
        }
        Err(PlaneError::Unavailable(m)) => {
            persist_if_dirty(ctx, &sp, &sync, dirty)?;
            if explicit {
                return Err(ClientError::Plane(m));
            }
            return Ok(state_row(&name, &sync, PullAction::UpToDate));
        }
        Err(PlaneError::Malformed(_)) => {
            persist_if_dirty(ctx, &sp, &sync, dirty)?;
            return Ok(alarm(&name, &sync, PullAction::Alarm));
        }
    }

    // ---- plan: drive toward `observed` (even on a 304 — a prior apply may be pending) ----
    if gen_cmp(sync.applied, sync.observed) == core::cmp::Ordering::Equal {
        persist_if_dirty(ctx, &sp, &sync, dirty)?;
        return Ok(state_row(&name, &sync, PullAction::UpToDate));
    }
    // A held skill (a deliberate go-back pin) suppresses exactly one auto fast-forward; only an explicit
    // `topos pull <skill>` resumes it (which cleared `held` above). The bare sweep leaves it put.
    if sync.held && !explicit {
        persist_if_dirty(ctx, &sp, &sync, dirty)?;
        return Ok(state_row(&name, &sync, PullAction::Held));
    }
    let target_commit = recorded_commit(&sync, sync.observed)?;

    let work = compute_work(ctx, &map, &lock)?;
    let work_eq_base = match &work {
        WorkState::Absent => true, // nothing on disk to clobber → a clean install
        WorkState::Present { eq_base, .. } => *eq_base,
        WorkState::Unscannable => {
            // Never silently fast-forward over an unreadable placement — fail closed (no swap).
            persist_if_dirty(ctx, &sp, &sync, dirty)?;
            return Ok(alarm(&name, &sync, PullAction::Alarm));
        }
    };

    // snapshot-on-touch FIRST: a draft is committed to the sidecar store BEFORE any fetch or decision.
    let draft_id: Option<String> = match &work {
        WorkState::Present {
            digest_hex,
            eq_base: false,
        } => Some(snapshot_draft(ctx, &sp, &lock, digest_hex)?),
        _ => None,
    };

    // fetch + record the target durably (the integrity gate: write_bundle + commit re-derive the id and
    // refuse a lying ref; render-on-read re-hashes). Backfill any missing ancestors so `commit` has parents.
    let store = Store::open(&sp.store)?;
    let target_digest = ensure_local(ctx, &store, skill_id, target_commit, 0)?;
    let bundle = store.render_verified(target_commit, target_digest)?;
    let target_digest_hex = to_hex(&target_digest);

    let work_eq_target = match &work {
        WorkState::Present { digest_hex, .. } => *digest_hex == target_digest_hex,
        WorkState::Absent => false,
        WorkState::Unscannable => unreachable!("handled above"),
    };

    // ---- apply ----
    let t = ApplyTarget {
        commit: target_commit,
        digest_hex: &target_digest_hex,
        bundle: &bundle,
    };
    match sync::refine_after_fetch(work_eq_base, work_eq_target) {
        ApplyClass::AlreadyAtTarget => {
            // The bytes already equal the target (a crash after a prior swap, or an idempotent re-pull):
            // advance `applied` with NO swap, never a false DIVERGED.
            heal_forward(ctx, &sp, &map, &lock, &sync, &t)?;
            Ok(applied_row(&name, &sync, target_commit))
        }
        ApplyClass::CleanForward => {
            let situation = situation_for(follow, explicit);
            let decision = topos_core::consent::decide(situation);
            if decision.applies_bytes() {
                apply_forward(ctx, &sp, &map, &lock, &sync, skill_id, &t)?;
                Ok(applied_row(&name, &sync, target_commit))
            } else {
                // confirm-each / TOFU: re-disclose the digest as a one-tap offer; nothing materializes.
                persist_if_dirty(ctx, &sp, &sync, dirty)?;
                Ok(offer_row(&name, &sync, target_commit, &target_digest_hex))
            }
        }
        ApplyClass::Diverged => {
            // ④ local edits AND a newer remote: the draft is already snapshotted; surface, never clobber.
            persist_if_dirty(ctx, &sp, &sync, dirty)?;
            Ok(diverged_row(&name, &sync, target_commit, draft_id))
        }
    }
}

/// `topos pull <skill>@<hash>` — install an older version's exact bytes locally (a deliberate go-back),
/// set `held` to suppress the next auto fast-forward, and **do NOT lower the `observed` floor** (a held
/// copy still rejects downgrades). The target must be in this skill's recorded history (so its generation
/// is known — never a fabricated floor).
pub(crate) fn go_back(
    ctx: &Ctx<'_>,
    skill_id: &str,
    target: [u8; 32],
) -> Result<PullSkill, ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let map: PlacementMap = read_required(ctx, &sp.map, "map.json")?;
    validate_recorded_unique(&sync.recorded)?;
    let name = lock.name.clone();

    let target_hex = to_hex(&target);
    // The go-back generation must be a real recorded one — refuse a version with no known generation.
    let target_gen = sync
        .recorded
        .iter()
        .find(|t| t.commit_id == target_hex)
        .map(|t| t.generation)
        .ok_or_else(|| ClientError::UnknownGoBackVersion {
            version: target_hex.clone(),
        })?;

    // The target's bytes must already be in the local store (a previously-applied version).
    let store = Store::open(&sp.store)?;
    let target_digest = store_bundle_digest(&store, target)?;
    let bundle = store.render_verified(target, target_digest)?;
    let target_digest_hex = to_hex(&target_digest);

    // `ExplicitLocalPull` → `MaterializeLocal`: a direct local command authorizes installing these bytes;
    // the digest is re-bound on materialize. The floor `observed` is untouched (no downgrade); `applied`
    // honestly drops to the installed version's generation so a later bare `pull` sees ② and fast-forwards.
    debug_assert!(
        topos_core::consent::decide(topos_core::consent::Situation::ExplicitLocalPull)
            .applies_bytes()
    );
    let next_sync = SyncState {
        schema_version: sync.schema_version,
        observed: sync.observed,
        applied: target_gen,
        recorded: sync.recorded.clone(),
        base_commit: target_hex.clone(),
        work_hash: target_digest_hex.clone(),
        held: true,
    };
    let next_lock = lock_from_bundle(&lock, target, &bundle);
    let placement = first_placement(&map)?;
    let report = materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            placement_dir: Path::new(&placement),
            bundle: &bundle,
            prior_map: &map,
            next_map_core: map_core(&map, target, &target_digest_hex),
            next_lock: &next_lock,
            next_sync: &next_sync,
            sp: &sp,
        },
    )?;
    log_apply(ctx, skill_id, "pull-goback", target, &report);
    Ok(PullSkill {
        skill: name,
        observed: next_sync.observed,
        applied: next_sync.applied,
        action: PullAction::Held,
        offer: None,
        conflict: None,
    })
}

/// The current local state of a tracked skill as a read-only `PullSkill` (UpToDate) — used when a
/// targeted pull names a tracked-but-unfollowed skill (there is no `current` to pull).
pub(crate) fn current_state(ctx: &Ctx<'_>, skill_id: &str) -> Result<PullSkill, ClientError> {
    let sp = ctx.layout.published(skill_id);
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    Ok(state_row(&lock.name, &sync, PullAction::UpToDate))
}

// ---------------------------------------------------------------------------------------------
// Situation mapping — the engine's only choice; the OUTCOME is the kernel's one policy.
// ---------------------------------------------------------------------------------------------

/// Map the follow-state + invocation to the consent situation. A follower only ever receives an
/// already-approved `current` (the gate is server-side), so a forward move under `review_required` is
/// `ReviewRequiredApproved`; auto is `FollowedAutoNewVersion`; confirm-each is `FollowedConfirmEach`. An
/// explicit `topos pull <skill>` is the user's `ExplicitLocalPull` — a direct local command that
/// authorizes the apply (and re-binds the digest), so it applies even in confirm-each.
fn situation_for(follow: &FollowContext, explicit: bool) -> topos_core::consent::Situation {
    use topos_core::consent::Situation;
    if explicit {
        Situation::ExplicitLocalPull
    } else if follow.review_required {
        Situation::ReviewRequiredApproved
    } else {
        match follow.mode {
            FollowMode::Auto => Situation::FollowedAutoNewVersion,
            FollowMode::ConfirmEach => Situation::FollowedConfirmEach,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Apply / heal.
// ---------------------------------------------------------------------------------------------

/// The verified target of a forward apply / heal — the resolved commit, its digest, and its bytes.
struct ApplyTarget<'a> {
    commit: [u8; 32],
    digest_hex: &'a str,
    bundle: &'a topos_gitstore::RenderedBundle,
}

/// A clean forward apply: materialize the target onto the placement and advance `applied → observed`.
fn apply_forward(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    map: &PlacementMap,
    lock: &Lock,
    sync: &SyncState,
    skill_id: &str,
    t: &ApplyTarget<'_>,
) -> Result<(), ClientError> {
    let next_sync = forwarded_sync(sync, t.commit, t.digest_hex);
    let next_lock = lock_from_bundle(lock, t.commit, t.bundle);
    let placement = first_placement(map)?;
    let report = materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            placement_dir: Path::new(&placement),
            bundle: t.bundle,
            prior_map: map,
            next_map_core: map_core(map, t.commit, t.digest_hex),
            next_lock: &next_lock,
            next_sync: &next_sync,
            sp,
        },
    )?;
    log_apply(ctx, skill_id, "pull", t.commit, &report);
    Ok(())
}

/// A best-effort action-log note (the spec's "quiet note") — the apply already succeeded, so a log hiccup
/// never undoes it. Reads the materialize report (the effective swap capability + whether prior bytes were
/// preserved) so the local `log` shows what landed.
fn log_apply(
    ctx: &Ctx<'_>,
    skill_id: &str,
    action: &str,
    version_id: [u8; 32],
    report: &MaterializeReport,
) {
    let _ = logfile::append_event(
        ctx.fs,
        &ctx.layout.log_path(),
        &serde_json::json!({
            "action": action,
            "skill_id": skill_id,
            "version_id": to_hex(&version_id),
            "swap": format!("{:?}", report.swap_capability),
            "preserved_prior": report.pre_existing_sha.is_some(),
            "at": ctx.clock.now_unix_millis(),
        }),
    );
}

/// The heal: the placement already holds the target bytes (a completed-but-unrecorded apply). Advance the
/// docs (map → lock → sync) with NO swap, via the shared `commit_docs` + `derive_pre_existing_sha`.
fn heal_forward(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    map: &PlacementMap,
    lock: &Lock,
    sync: &SyncState,
    t: &ApplyTarget<'_>,
) -> Result<(), ClientError> {
    let next_sync = forwarded_sync(sync, t.commit, t.digest_hex);
    let next_lock = lock_from_bundle(lock, t.commit, t.bundle);
    let next_map = PlacementMap {
        schema_version: map.schema_version,
        placements: map.placements.clone(),
        applied_commit: to_hex(&t.commit),
        materialized_sha: t.digest_hex.to_owned(),
        // The placement is present (it holds the target bytes), so a prior overwrite is captured stickily.
        pre_existing_sha: materialize::derive_pre_existing_sha(map, true),
        swap_capability: map.swap_capability,
        harness: map.harness,
        harness_layer: map.harness_layer.clone(),
    };
    materialize::commit_docs(ctx.fs, sp, &next_map, &next_lock, &next_sync)
}

/// The forward target sync state: `applied = observed`, base/work move to the target, `held` cleared.
fn forwarded_sync(sync: &SyncState, target: [u8; 32], target_digest_hex: &str) -> SyncState {
    SyncState {
        schema_version: sync.schema_version,
        observed: sync.observed,
        applied: sync.observed,
        recorded: sync.recorded.clone(),
        base_commit: to_hex(&target),
        work_hash: target_digest_hex.to_owned(),
        held: false,
    }
}

fn map_core(map: &PlacementMap, target: [u8; 32], target_digest_hex: &str) -> NextMapCore {
    NextMapCore {
        placements: map.placements.clone(),
        applied_commit: to_hex(&target),
        materialized_sha: target_digest_hex.to_owned(),
        harness: map.harness,
        harness_layer: map.harness_layer.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// The store side: snapshot a draft, backfill + record a fetched version, read a stored digest.
// ---------------------------------------------------------------------------------------------

/// Snapshot the current working bytes into the sidecar store as a commit on `base_commit`, so a draft is
/// never lost when a divergence is surfaced. Returns the snapshot `version_id` (the saved draft).
fn snapshot_draft(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    _work_digest_hex: &str,
) -> Result<String, ClientError> {
    let placement = lock_placement(ctx, sp)?;
    let scanned = scan::scan(Path::new(&placement))?;
    let base = super::parse_hex32(&lock.base_commit)?;
    let draft_id = sign::commit_id(&Commit {
        parents: &[base],
        tree: scanned.bundle_digest,
        author: &ctx.device_id,
        message: DRAFT_SNAPSHOT_MESSAGE,
    })
    .map_err(|_| ClientError::Corrupt("draft snapshot commit id".into()))?;

    let store = Store::open(&sp.store)?;
    let import: Vec<ImportFile<'_>> = scanned
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store.commit(
        draft_id,
        &[base],
        &tree,
        &ctx.device_id,
        DRAFT_SNAPSHOT_MESSAGE,
    )?;
    fsync_store(ctx, &store)?;
    Ok(to_hex(&draft_id))
}

/// Ensure `version_id` (and any missing ancestors) is committed in the local store, so a later go-back,
/// diff, or log can render it. Recursively backfills absent parents (the fixture serves each) so
/// `Store::commit`'s parent-present precondition holds across a multi-generation gap. Returns the version's
/// `bundle_digest` (recomputed over the fetched bytes — the integrity tree hash).
fn ensure_local(
    ctx: &Ctx<'_>,
    store: &Store,
    skill_id: &str,
    version_id: [u8; 32],
    depth: usize,
) -> Result<[u8; 32], ClientError> {
    if depth > MAX_BACKFILL {
        return Err(ClientError::Corrupt(
            "version lineage too deep to backfill".into(),
        ));
    }
    if let Some(existing) = store_bundle_digest_opt(store, version_id)? {
        return Ok(existing);
    }
    let fetched = fetch(ctx, skill_id, version_id)?;
    // Backfill any missing ancestors first (so `commit` sees its parents).
    for parent in &fetched.parents {
        if store_bundle_digest_opt(store, *parent)?.is_none() {
            ensure_local(ctx, store, skill_id, *parent, depth + 1)?;
        }
    }
    let import: Vec<ImportFile<'_>> = fetched
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    // `commit` re-derives the version_id from (parents, tree.bundle_digest, author, message) and refuses a
    // ref that lies about its identity — so tampered bytes / metadata fail here (recompute == version_id).
    store
        .commit(
            version_id,
            &fetched.parents,
            &tree,
            &fetched.author,
            &fetched.message,
        )
        .map_err(|_| {
            ClientError::Corrupt(format!(
                "fetched version {} does not match its id",
                to_hex(&version_id)
            ))
        })?;
    fsync_store(ctx, store)?;
    Ok(tree.bundle_digest)
}

/// The `bundle_digest` of a stored version, or `None` if the version is not present.
fn store_bundle_digest_opt(
    store: &Store,
    version_id: [u8; 32],
) -> Result<Option<[u8; 32]>, ClientError> {
    if !store.list_versions()?.contains(&version_id) {
        return Ok(None);
    }
    Ok(Some(store_bundle_digest(store, version_id)?))
}

/// The `bundle_digest` of a present stored version (recomputed via the tree-structure walk → kernel digest
/// over the recorded content ids). Used to pin `render_verified`.
fn store_bundle_digest(store: &Store, version_id: [u8; 32]) -> Result<[u8; 32], ClientError> {
    let leaves = store.read_tree_structure(version_id)?;
    let mut entries = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        let (_, content_sha256) = store.read_git_blob_verified(leaf.git_oid)?;
        entries.push(digest::ManifestEntry {
            path: leaf.path.clone(),
            mode: leaf.mode,
            content_sha256,
        });
    }
    digest::bundle_digest(&entries)
        .map_err(|r| ClientError::Corrupt(format!("stored digest: {r:?}")))
}

// ---------------------------------------------------------------------------------------------
// Working-tree classification.
// ---------------------------------------------------------------------------------------------

enum WorkState {
    /// The placement directory does not exist — a clean first install (nothing to clobber).
    Absent,
    /// The placement scanned cleanly; `eq_base` is whether it matches the locked base bytes.
    Present { digest_hex: String, eq_base: bool },
    /// The placement exists but cannot be scanned safely — fail closed, never overwrite it.
    Unscannable,
}

/// Classify the placement directory against the locked base digest. Distinguishes ABSENT (a safe install)
/// from UNSCANNABLE (a hazardous tree — fail closed, never clobber), unlike read-only `list`'s two-way
/// `is_draft` which collapses both to "no draft".
fn compute_work(ctx: &Ctx<'_>, map: &PlacementMap, lock: &Lock) -> Result<WorkState, ClientError> {
    let Some(placement) = map.placements.first() else {
        return Ok(WorkState::Absent);
    };
    let p = Path::new(placement);
    if ctx.fs.path_kind(p)?.is_none() {
        return Ok(WorkState::Absent);
    }
    match scan::scan(p) {
        Ok(ScannedBundle { bundle_digest, .. }) => {
            let digest_hex = to_hex(&bundle_digest);
            let eq_base = digest_hex == lock.bundle_digest;
            Ok(WorkState::Present {
                digest_hex,
                eq_base,
            })
        }
        Err(_) => Ok(WorkState::Unscannable),
    }
}

// ---------------------------------------------------------------------------------------------
// Authentication of a served record.
// ---------------------------------------------------------------------------------------------

struct Authed {
    version_id: [u8; 32],
    generation: Generation,
}

/// Authenticate a served `current` record: decode + verify the signature against the pinned plane key,
/// and confirm the record's `(workspace_id, skill_id)` scope is the one we follow (defeating a
/// cross-workspace / cross-skill replay of an otherwise-valid signed pointer). `None` on any failure.
fn authenticate(
    rec: &topos_types::SignedCurrentRecord,
    skill_id: &str,
    follow: &FollowContext,
    plane_key: &[u8; 32],
) -> Option<Authed> {
    if rec.scope.skill_id != skill_id || rec.scope.workspace_id != follow.workspace_id {
        return None;
    }
    let version_id = super::parse_hex32(&rec.record.version_id).ok()?;
    let sig = decode_sig(&rec.signature.value)?;
    let pointer = sign::CurrentPointer {
        workspace_id: &rec.scope.workspace_id,
        skill_id: &rec.scope.skill_id,
        version_id,
        epoch: rec.record.generation.epoch,
        seq: rec.record.generation.seq,
    };
    if sign::verify_pointer(&pointer, &sig, plane_key) {
        Some(Authed {
            version_id,
            generation: rec.record.generation,
        })
    } else {
        None
    }
}

/// Decode the base64url-unpadded `Signature.value` (86 chars) into a raw 64-byte Ed25519 signature.
fn decode_sig(value: &str) -> Option<[u8; 64]> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    bytes.try_into().ok()
}

// ---------------------------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------------------------

fn fetch(
    ctx: &Ctx<'_>,
    skill_id: &str,
    version_id: [u8; 32],
) -> Result<crate::plane::FetchedVersion, ClientError> {
    ctx.plane
        .fetch_version(skill_id, version_id)
        .map_err(|e| match e {
            PlaneError::NotFound => {
                ClientError::Plane(format!("version {} not served", to_hex(&version_id)))
            }
            PlaneError::Unavailable(m) | PlaneError::Malformed(m) => ClientError::Plane(m),
        })
}

fn kgen(g: Generation) -> KGen {
    KGen {
        epoch: g.epoch,
        seq: g.seq,
    }
}

fn kernel_recorded(sync: &SyncState) -> Result<Vec<sync::RecordedTuple>, ClientError> {
    sync.recorded
        .iter()
        .map(|t| {
            Ok(sync::RecordedTuple {
                generation: kgen(t.generation),
                commit_id: super::parse_hex32(&t.commit_id)?,
            })
        })
        .collect()
}

/// Reject a `recorded` list with a duplicate generation naming different commits (local corruption — the
/// reused-tuple ALARM relies on a unique generation → commit map).
fn validate_recorded_unique(recorded: &[RecordedTuple]) -> Result<(), ClientError> {
    for (i, a) in recorded.iter().enumerate() {
        for b in &recorded[i + 1..] {
            if a.generation == b.generation && a.commit_id != b.commit_id {
                return Err(ClientError::Corrupt(
                    "recorded history has a duplicate generation naming two commits".into(),
                ));
            }
        }
    }
    Ok(())
}

fn recorded_commit(sync: &SyncState, wanted: Generation) -> Result<[u8; 32], ClientError> {
    let hex = sync
        .recorded
        .iter()
        .find(|t| t.generation == wanted)
        .map(|t| t.commit_id.clone())
        .ok_or_else(|| ClientError::Corrupt("observed generation has no recorded commit".into()))?;
    super::parse_hex32(&hex)
}

fn lock_from_bundle(
    prior: &Lock,
    version_id: [u8; 32],
    bundle: &topos_gitstore::RenderedBundle,
) -> Lock {
    Lock {
        schema_version: prior.schema_version,
        skill_id: prior.skill_id.clone(),
        name: prior.name.clone(),
        base_commit: to_hex(&version_id),
        bundle_digest: to_hex(&bundle.bundle_digest),
        files: bundle
            .files
            .iter()
            .map(|f| LockedFile {
                path: f.path.clone(),
                mode: f.mode.as_str().to_owned(),
                sha256: to_hex(&f.content_sha256),
                size: f.bytes.len() as u64,
            })
            .collect(),
    }
}

fn first_placement(map: &PlacementMap) -> Result<String, ClientError> {
    map.placements
        .first()
        .cloned()
        .ok_or_else(|| ClientError::Corrupt("placement map has no placement".into()))
}

fn lock_placement(ctx: &Ctx<'_>, sp: &sidecar::SkillPaths) -> Result<String, ClientError> {
    let map: PlacementMap = read_required(ctx, &sp.map, "map.json")?;
    first_placement(&map)
}

fn fsync_store(ctx: &Ctx<'_>, store: &Store) -> Result<(), ClientError> {
    let batch = store.durability_set()?;
    for f in &batch.files {
        ctx.fs.fsync_file(f)?;
    }
    for d in &batch.dirs {
        ctx.fs.fsync_dir(d)?;
    }
    Ok(())
}

fn read_required<T: serde::de::DeserializeOwned>(
    ctx: &Ctx<'_>,
    path: &Path,
    what: &str,
) -> Result<T, ClientError> {
    doc::read_doc(ctx.fs, path)?
        .ok_or_else(|| ClientError::Corrupt(format!("missing {what} for a followed skill")))
}

fn persist_if_dirty(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    sync: &SyncState,
    dirty: bool,
) -> Result<(), ClientError> {
    if dirty {
        doc::write_doc(ctx.fs, &sp.sync, sync)?;
    }
    Ok(())
}

// ---- PullSkill row builders ----

fn state_row(name: &str, sync: &SyncState, action: PullAction) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        observed: sync.observed,
        applied: sync.applied,
        action,
        offer: None,
        conflict: None,
    }
}

fn alarm(name: &str, sync: &SyncState, action: PullAction) -> PullSkill {
    state_row(name, sync, action)
}

fn applied_row(name: &str, sync: &SyncState, _target: [u8; 32]) -> PullSkill {
    // `applied` is now `observed` on disk; report the advanced state.
    PullSkill {
        skill: name.to_owned(),
        observed: sync.observed,
        applied: sync.observed,
        action: PullAction::FastForwarded,
        offer: None,
        conflict: None,
    }
}

fn offer_row(name: &str, sync: &SyncState, target: [u8; 32], target_digest_hex: &str) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        observed: sync.observed,
        applied: sync.applied,
        action: PullAction::Offered,
        offer: Some(Offer {
            version_id: to_hex(&target),
            bundle_digest: target_digest_hex.to_owned(),
        }),
        conflict: None,
    }
}

fn diverged_row(
    name: &str,
    sync: &SyncState,
    target: [u8; 32],
    draft_id: Option<String>,
) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        observed: sync.observed,
        applied: sync.applied,
        action: PullAction::Diverged,
        offer: None,
        conflict: Some(Conflict {
            remote_version_id: to_hex(&target),
            local_version_id: draft_id,
        }),
    }
}
