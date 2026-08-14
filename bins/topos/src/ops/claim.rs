//! `add <path> --as <bundle>` — THE IDENTITY CLAIM: a folder that already holds a copy of a bundle
//! this scope manages becomes one of that bundle's places.
//!
//! Nothing in the folder changes and no version is minted. The claim writes ONE placement row into
//! the record's `map.json` with a BASELINE the folder's own bytes decide ([`claim_baseline`]), and
//! from there the copy rides the ordinary machinery: the converge refreshes a stale one, the draft
//! engine carries an edited one, and the next update lands here like anywhere else.
//!
//! THE BASELINE IS THE WHOLE MECHANIC. A placement row with no `materialized_sha` scans FOREIGN and
//! is frozen out of every plan forever ([`crate::placement::scan_one`]), so a claim always writes
//! one: the current digest when the folder is current, the matching historical digest when it is
//! behind, and — for bytes no version explains — the lock's digest, AFTER snapshotting those bytes
//! into the store, so the copy scans Modified and a stale-replica refresh can never swap edits out.

use std::path::{Path, PathBuf};

use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_types::persisted::{
    Lock, PlacementClaim, PlacementKind, PlacementMap, PlacementState, SwapCapability,
};
use topos_types::results::{AddData, ClaimReceipt, ClaimState, ClaimTwin};

use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::manifest::document::ManifestScope;
use crate::manifest::keys::KeyShape;
use crate::placement::{Drift, ScanStatus};
use crate::scan::{self, ScannedBundle};

use super::add::{RecordOrigin, folder_readers, origin_dir, record_origin, tracked_skill_at};
use super::manifest_edit::AddScope;

/// The record a `--as <bundle>` resolved to, in the scope the invocation acts in.
struct ClaimTarget {
    id: SkillId,
    lock: Lock,
    /// The record's own source — the `source:` line, and the `--as` value an offered line spells.
    origin: Option<RecordOrigin>,
}

/// Record `dir` as a copy of the bundle `as_bundle` names — the `--as` arm of `add`.
///
/// The scope decides everything: which store is searched for the bundle (never a workspace
/// catalog, never the other scope), which manifest a dest-pinned row is extended in, and where the
/// placement row lands.
///
/// # Errors
/// [`ClientError::SourceMissing`] / [`ClientError::LinkedSource`] for the folder;
/// [`ClientError::NoSuchSkill`] / [`ClientError::AmbiguousSource`] resolving `as_bundle`;
/// [`ClientError::ClaimNotAFolder`] for an MCP bundle; [`ClientError::ClaimTaken`] when another
/// record holds the folder (in either scope); [`ClientError::ClaimUnscannable`] when the folder
/// cannot be read; [`ClientError::InvalidArgument`] for a scope/path mismatch; a store or
/// manifest failure.
pub(crate) fn claim(
    ctx: &Ctx<'_>,
    scope: &AddScope,
    dir: &Path,
    as_bundle: &str,
) -> Result<AddData, ClientError> {
    if !ctx.fs.exists(dir) {
        return Err(ClientError::SourceMissing {
            path: dir.display().to_string(),
        });
    }
    // A LINK SHELL claims its original — the folder the bytes really live in, which is the folder
    // every record keys and every scan reads.
    let dir = origin_dir(dir)?;
    let sctx = super::pull::ctx_with_layout(ctx, &scope.layout);
    let target = resolve_target(ctx, &sctx, scope, as_bundle)?;
    refuse_wrong_scope(ctx, scope, &dir, &target)?;
    refuse_mcp(&sctx, &target)?;

    // WHAT THE ROW WRITE BELOW WILL ADD, decided BEFORE the record is written so the record can
    // carry it in the SAME write. The two are one fact — "the claim put this entry there" — and
    // recording it afterwards made it a third write a crash could strand: the detach would then
    // subtract nothing, the row would keep the root, and the next sweep would re-materialize an
    // engine copy moments after the receipt said the folder just stops updating. Recording it
    // speculatively is safe the other way: a crash before the row write leaves a stamp naming an
    // entry the row lacks, and subtracting an entry that is not there is a no-op.
    let planned_dest = planned_dest_entry(ctx, scope, &target, &dir)?;

    // THE RECORD HALF, under this record's writer flock. The ownership questions are asked INSIDE
    // it and immediately before the append, over the map this very call then rewrites — a check
    // outside the lock is a check against a map another writer may already have moved.
    let (state, twin, repaired, stamped) = {
        let sp = scope.layout.published(&target.id);
        let _guard = crate::sidecar::lock_skill(sctx.fs, &scope.layout, &target.id)?;
        // A claim is a re-demand of the bundle it names: a RETIRED record (one the one-time
        // orphan resolution released when nothing claimed it) returns to every surface as this
        // folder joins its places, or the placement below would land in a record no walker reads.
        // Best-effort, like every revive — a courtesy of the claim, never its gate.
        crate::sidecar::revive_record(sctx.fs, &scope.layout, &target.id);
        let Some(mut map) = doc::read_map(sctx.fs, &sp.map)? else {
            return Err(ClientError::Corrupt(format!(
                "'{}' has no placement map to record a copy in",
                target.lock.name
            )));
        };
        // THE FOLDER'S OWN CLAIMANTS: this record is the already-a-place answer (which still
        // repairs every half below), any other record in either scope is the refusal — two
        // engines would otherwise converge one directory.
        let (idx, repaired, stamped) = match standing_index(&map, &dir) {
            Some(idx) => {
                // VERIFY AND REPAIR: a run that crashed before its row write left a record with
                // no stamp. Set, never cleared — an earlier run's answer outranks this one's.
                let stamp = planned_dest.is_some()
                    && map
                        .placement_state
                        .get(idx)
                        .is_some_and(|st| st.claim.as_ref().is_none_or(|c| c.added_dest.is_none()));
                if stamp && let Some(st) = map.placement_state.get_mut(idx) {
                    st.claim = Some(PlacementClaim {
                        added_dest: planned_dest.clone(),
                    });
                    doc::write_map(sctx.fs, &sp.map, &map)?;
                }
                (idx, Repaired::Standing, stamp)
            }
            None => {
                refuse_taken(ctx, &sctx, scope, &dir, &target)?;
                let scanned = scan_claim(ctx, &dir)?;
                let baseline = claim_baseline(&sctx, &sp, &target.lock, &scanned)?;
                map.placements.push(dir.to_string_lossy().into_owned());
                map.placement_state.push(PlacementState {
                    kind: PlacementKind::Native,
                    // The folder's ONE installed reader owns it; a folder several agents read (or
                    // none) has no single owner — the attribution an in-place adopt records.
                    agent: match folder_readers(&dir).as_slice() {
                        [sole] => Some(sole.clone()),
                        _ => None,
                    },
                    materialized_sha: Some(baseline),
                    pre_existing_sha: None,
                    swap_capability: SwapCapability::Unsupported,
                    // The person's own folder: never retired by a clean.
                    adopted_source: true,
                    // …and a CLAIM, which is what makes it a target of every plan — carrying, in
                    // this same write, the row entry the call is about to add.
                    claim: Some(PlacementClaim {
                        added_dest: planned_dest.clone(),
                    }),
                });
                doc::write_map(sctx.fs, &sp.map, &map)?;
                (map.placements.len() - 1, Repaired::Fresh, false)
            }
        };
        // The TWIN check rides BOTH paths, for the reason the row write does: a run that crashed
        // after the record write would otherwise leave the duplicate standing forever.
        let state = claim_state(&sctx, &map, &target.lock, idx);
        let twin = retire_twin(&sctx, &target, &dir, &map)?;
        (state, twin, repaired, stamped)
    };
    let retired_twin = twin.as_ref().is_some_and(|t| t.removed);

    let mut data = receipt(ctx, &target, &dir, state, twin);
    // A DEST-FROZEN row plans from its own entries alone, so a record-only claim would starve: the
    // folder joins the row's destinations through the ordinary additive write. It runs on the
    // ALREADY-A-PLACE path too, which is what makes a re-run REPAIR a claim that crashed between
    // the two writes — the row write is idempotent, and says so when there was nothing to add.
    let added = extend_row_dest(ctx, scope, &target, &dir, &mut data)?;
    // A FRESH claim RECORDED A PLACEMENT — a real change, whatever the row write found. The row's
    // redundancy note would contradict the headline above it, and it names the ROW's source
    // folder, which beside a claim receipt reads as a statement about the folder just claimed. It
    // is not; the claim receipt is the whole answer.
    //
    // NOTHING CHANGED is a whole answer too, and then it is the whole lead — said only where every
    // half of the claim found its work already done.
    let nothing_changed =
        repaired == Repaired::Standing && added.is_none() && !stamped && !retired_twin;
    if nothing_changed {
        data.unchanged = true;
        data.note = Some(format!(
            "{} is already one of {}'s folders — nothing changed",
            crate::ops::inventory::pretty(ctx, &dir),
            target.lock.name
        ));
    } else {
        data.unchanged = false;
        data.note = None;
    }
    Ok(data)
}

/// Whether this call recorded the folder or found it already recorded — the second is a re-run
/// (or the repair of a run that crashed between the record write and the row write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repaired {
    Fresh,
    Standing,
}

/// The index of `dir` among a record's placements, compared the way every other placement lookup
/// compares (canonically, so a symlinked spelling is the same folder).
fn standing_index(map: &PlacementMap, dir: &Path) -> Option<usize> {
    map.placements
        .iter()
        .position(|p| Path::new(p).canonicalize().is_ok_and(|c| c == *dir))
}

/// The `dest` entry the row write is ABOUT to add — the claimed folder's root, when the standing
/// row is destination-frozen and does not already name it. `None` for every other shape, which is
/// exactly the set of cases where the claim puts nothing on the row and its detach must take
/// nothing back.
///
/// Read before the record is written, so the record can carry it in ONE write (see [`claim`]).
///
/// # Errors
/// A manifest read failure, or a file the grammar refuses.
fn planned_dest_entry(
    ctx: &Ctx<'_>,
    scope: &AddScope,
    target: &ClaimTarget,
    dir: &Path,
) -> Result<Option<String>, ClientError> {
    let Some(reference) = row_reference(scope, target) else {
        return Ok(None);
    };
    let Some(row) = standing_row(ctx, scope, &reference)? else {
        return Ok(None);
    };
    let Some(dest) = row.fields().dest else {
        return Ok(None); // the row reaches every agent; the claim adds no rule to it
    };
    let Some(parent) = dir.parent() else {
        return Ok(None);
    };
    let entry = super::manifest_edit::dest_spelling(ctx, &scope.target, parent);
    Ok((!dest.contains(&entry)).then_some(entry))
}

/// The row key this record's own source takes in the invoked scope's file — a reference verbatim,
/// a folder through the one path-key derivation the row write itself uses.
fn row_reference(scope: &AddScope, target: &ClaimTarget) -> Option<String> {
    match target.origin.as_ref()? {
        RecordOrigin::Reference(reference) => Some(reference.clone()),
        RecordOrigin::Folder(source) => {
            Some(super::manifest_edit::path_row_key(&scope.target, source))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Resolution — which record `--as` names, in the invoked scope alone
// ---------------------------------------------------------------------------------------------

/// Resolve `as_bundle` against the records the INVOKED scope's store holds — never a workspace
/// catalog, never the other scope. A bare name is the record resolver's job (the grammar does not
/// own bare names); a REFERENCE is matched against each record's own origin spelling.
fn resolve_target(
    ctx: &Ctx<'_>,
    sctx: &Ctx<'_>,
    scope: &AddScope,
    as_bundle: &str,
) -> Result<ClaimTarget, ClientError> {
    let host = super::manifest_edit::manifest_host(ctx);
    let global = scope.target.scope == ManifestScope::Global;
    // A token the grammar reads as a REFERENCE names ONE record: the shape carries an identity a
    // name cannot collide with, so there is nothing to choose between.
    if let Ok(parsed) = crate::manifest::keys::parse_input(as_bundle, host.as_deref()) {
        return by_reference(ctx, sctx, &parsed.shape, as_bundle);
    }
    let records = super::skills_named(sctx, as_bundle)?;
    let mut found: Vec<ClaimTarget> = Vec::new();
    for (id, lock) in records {
        let origin = record_origin(ctx, sctx, &id, &lock);
        // ONE thing the name can mean per distinct source (a forge import lands a record per
        // destination), exactly as the bare-add ladder counts them.
        if found
            .iter()
            .any(|k| k.origin.is_some() && k.origin == origin)
        {
            continue;
        }
        found.push(ClaimTarget { id, lock, origin });
    }
    match found.len() {
        0 => Err(ClientError::NoSuchSkill {
            name: as_bundle.to_owned(),
        }),
        1 => Ok(found.remove(0)),
        _ => Err(claim_chooser(ctx, as_bundle, global, &found)),
    }
}

/// The record whose OWN origin is exactly this reference — the direct lookup a spelled-out `--as`
/// gets. A local-path reference resolves through the folder it names; every other shape through
/// the one origin join every surface makes.
fn by_reference(
    ctx: &Ctx<'_>,
    sctx: &Ctx<'_>,
    shape: &KeyShape,
    typed: &str,
) -> Result<ClaimTarget, ClientError> {
    let miss = || ClientError::NoSuchSkill {
        name: typed.to_owned(),
    };
    if let KeyShape::LocalPath { raw } = shape {
        let dir = origin_dir(Path::new(&expand_home(ctx, raw)))?;
        let id = tracked_skill_at(sctx, &dir)?.ok_or_else(miss)?;
        let id = SkillId::parse(&id)?;
        let lock = read_lock(sctx, &id)?.ok_or_else(miss)?;
        let origin = record_origin(ctx, sctx, &id, &lock);
        return Ok(ClaimTarget { id, lock, origin });
    }
    let want = shape.canonical();
    for (id, lock) in super::skills_named(sctx, shape.leaf_name())? {
        let origin = record_origin(ctx, sctx, &id, &lock);
        if origin == Some(RecordOrigin::Reference(want.clone())) {
            return Ok(ClaimTarget { id, lock, origin });
        }
    }
    Err(miss())
}

/// A `~/`-prefixed reference against this machine's home — the one expansion a manifest key gets.
fn expand_home(ctx: &Ctx<'_>, raw: &str) -> PathBuf {
    match (raw.strip_prefix("~/"), &ctx.roots) {
        (Some(rest), Some(roots)) => roots.home.join(rest),
        _ => PathBuf::from(raw),
    }
}

/// A record's lock — the document that carries its name and the version it stands at.
fn read_lock(sctx: &Ctx<'_>, id: &SkillId) -> Result<Option<Lock>, ClientError> {
    doc::read_doc::<Lock>(sctx.fs, &sctx.layout.published(id).lock)
}

/// The chooser over several records of one name — THE ambiguity answer every add-side resolution
/// ends in, with each record's own `--as` spelling as one runnable line.
fn claim_chooser(ctx: &Ctx<'_>, name: &str, global: bool, found: &[ClaimTarget]) -> ClientError {
    let mut remote = Vec::new();
    let mut local = Vec::new();
    for target in found {
        match &target.origin {
            Some(RecordOrigin::Reference(r)) => remote.push(r.clone()),
            Some(RecordOrigin::Folder(dir)) => local.push(dir.to_string_lossy().into_owned()),
            None => {}
        }
    }
    super::add::chooser(ctx, name, name, "add", global, remote, local)
}

// ---------------------------------------------------------------------------------------------
// The guards
// ---------------------------------------------------------------------------------------------

/// A claim acts on the scope it stands in, and so does the folder: a folder inside the checkout is
/// the project's business, a folder outside it the machine's. Refuse naming the invocation that
/// owns it — never reroute, because the wrong store's engine would then converge that directory.
fn refuse_wrong_scope(
    ctx: &Ctx<'_>,
    scope: &AddScope,
    dir: &Path,
    target: &ClaimTarget,
) -> Result<(), ClientError> {
    let spelled = crate::ops::inventory::pretty(ctx, dir);
    let name = &target.lock.name;
    if scope.target.scope == ManifestScope::Project {
        if crate::materialize::contained_in(&scope.target.dir, dir) {
            return Ok(());
        }
        return Err(ClientError::InvalidArgument(format!(
            "{spelled} is outside this project — a folder that is not part of the checkout is \
             managed machine-wide (`topos add -g {spelled} --as {name}`)"
        )));
    }
    // `-g` with a folder INSIDE the checkout the invocation stands in: the project's own file is
    // what governs that folder, and its store is where the record belongs.
    let Some(project) = super::manifest_edit::project_target(ctx)? else {
        return Ok(());
    };
    if !crate::materialize::contained_in(&project.dir, dir) {
        return Ok(());
    }
    Err(ClientError::InvalidArgument(format!(
        "{spelled} is inside this project — drop `-g` so the project's own file records it \
         (`topos add {spelled} --as {name}`)"
    )))
}

/// An MCP server is delivered as config entries, not as a folder — there is no folder copy for a
/// claim to be about.
fn refuse_mcp(sctx: &Ctx<'_>, target: &ClaimTarget) -> Result<(), ClientError> {
    let placements = doc::read_map(sctx.fs, &sctx.layout.published(&target.id).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    if crate::bundle_kind::classify(sctx, target.id.as_str(), &placements)
        .or_skill()
        .is_mcp()
    {
        return Err(ClientError::ClaimNotAFolder {
            name: target.lock.name.clone(),
        });
    }
    Ok(())
}

/// ANOTHER record holds the folder — in this scope, or in the other one, whose engine would
/// otherwise converge the same directory from a file this invocation never named.
///
/// Called INSIDE the claimed record's writer flock, immediately before the append, so the answer
/// cannot go stale between the question and the write for THIS record. The residual is the one
/// [`super::add::reject_already_tracked`] already documents and shares: the flock is per record, so
/// two claims of one folder onto two DIFFERENT bundles hold two different locks and can both pass.
/// A scope-wide lock is the only thing that would close it, and the map-write discipline does not
/// have one; the common re-run — the same folder, the same bundle — is caught above this.
fn refuse_taken(
    ctx: &Ctx<'_>,
    sctx: &Ctx<'_>,
    scope: &AddScope,
    dir: &Path,
    target: &ClaimTarget,
) -> Result<(), ClientError> {
    let spelled = crate::ops::inventory::pretty(ctx, dir);
    if let Some(id) = tracked_skill_at(sctx, dir)? {
        return Err(ClientError::ClaimTaken {
            dir: spelled,
            claim: Some(target.lock.name.clone()),
            owner: super::add::record_name(sctx, &id),
            manifest: None,
        });
    }
    // THE CROSS-SCOPE PROBE — the one the path door asks too ([`super::add::refuse_other_scope`]).
    // Both stores manage folders, and neither knows about the other's rows: a folder recorded over
    // there would be converged by two engines at once.
    // The target is proven a folder bundle above ([`refuse_mcp`]), so the probe asks its full
    // question: a record in either scope would converge this directory.
    super::add::refuse_other_scope(
        ctx,
        scope,
        dir,
        Some(&target.lock.name),
        crate::bundle_kind::BundleKind::Skill,
    )
}

/// Scan the folder, refusing an unreadable one BEFORE anything is written: one Unscannable
/// placement wedges the whole bundle's sync, so the claim fails toward the gate.
///
/// EVERY scanner reject becomes the can't-read refusal, naming the folder and the reason — a
/// hazard the scanner found and a directory the kernel refused to open are the same news to the
/// person, and the second used to escape as a bare "a filesystem operation failed", classified
/// transient, inviting an agent to loop on a `chmod 000` that will answer identically forever.
fn scan_claim(ctx: &Ctx<'_>, dir: &Path) -> Result<ScannedBundle, ClientError> {
    scan::scan(dir).map_err(|e| match e {
        ClientError::Scan(reason) => ClientError::ClaimUnscannable {
            dir: crate::ops::inventory::pretty(ctx, dir),
            reason,
        },
        other => other,
    })
}

// ---------------------------------------------------------------------------------------------
// The mechanic: the baseline the folder's own bytes decide
// ---------------------------------------------------------------------------------------------

/// The `materialized_sha` the claimed placement is recorded with — the fact that makes the copy
/// LIVE (a row without one scans Foreign and is frozen out of every plan forever).
///
/// Three answers, and only the third writes anything: bytes at the current version record it, bytes
/// at an OLDER version in this record's history record THAT version's digest (the copy then scans
/// clean-at-stale and the ordinary converge refreshes it), and bytes no version explains are
/// SNAPSHOTTED first — without that the copy would be a draft with no stored ancestor, and a
/// stale-replica refresh could legally swap the edits out — then recorded against the lock, so the
/// copy scans Modified and rides the draft machinery like every other edited copy.
fn claim_baseline(
    sctx: &Ctx<'_>,
    sp: &crate::sidecar::SkillPaths,
    lock: &Lock,
    scanned: &ScannedBundle,
) -> Result<String, ClientError> {
    let digest = to_hex(&scanned.bundle_digest);
    if digest == lock.bundle_digest {
        return Ok(digest);
    }
    let store = Store::open(&sp.store)?;
    if digest_in_history(&store, &digest)? {
        return Ok(digest);
    }
    super::sync_engine::snapshot_draft(sctx, sp, lock, scanned)?;
    Ok(lock.bundle_digest.clone())
}

/// Whether any version this store holds renders to exactly `digest` — the history a claimed
/// folder's bytes are matched against. Short-circuits on the first match.
///
/// # Errors
/// A store read failure.
pub(crate) fn digest_in_history(store: &Store, digest: &str) -> Result<bool, ClientError> {
    for version in store.list_versions()? {
        if super::sync_engine::store_bundle_digest(store, version)
            .is_ok_and(|d| to_hex(&d) == digest)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What the claim at `idx` amounts to, read off the SAME scan the rest of the system reads: that
/// placement's own drift against its recorded baseline, plus the draft classifier's verdict over
/// the whole record — because an edited claim beside another folder's different edits is the
/// placement freeze, and a receipt that promised a spread there would be promising a sync that
/// cannot run.
///
/// Read from the RECORD, never from the value this call happened to compute: a re-run over an
/// already-claimed folder gets the same honest answer as the run that recorded it, instead of
/// repeating that first run's verdict about bytes that have moved since.
fn claim_state(sctx: &Ctx<'_>, map: &PlacementMap, lock: &Lock, idx: usize) -> ClaimState {
    let baseline = map
        .placement_state
        .get(idx)
        .and_then(|st| st.materialized_sha.clone())
        .unwrap_or_default();
    if baseline != lock.bundle_digest {
        return ClaimState::Older;
    }
    // Either the folder is current, or its bytes are a draft recorded against the lock.
    let scans = crate::placement::scan_placements(sctx, map).unwrap_or_default();
    let drift = scans
        .iter()
        .find(|s| s.idx == idx)
        .map(|s| s.status.drift());
    if drift != Some(Drift::Modified) {
        return ClaimState::Current;
    }
    match crate::placement::classify_draft(&scans, map) {
        crate::placement::DraftVerdict::Competitors(_) => ClaimState::Frozen,
        _ => ClaimState::Edited,
    }
}

// ---------------------------------------------------------------------------------------------
// The twin: the engine's own copy of the same bundle, beside the folder just claimed
// ---------------------------------------------------------------------------------------------

/// The DUPLICATE a claim reveals: the engine could not place this bundle at its own name (the
/// person's folder already held it), so it suffixed — `<name>-<workspace>` — and the two copies sat
/// side by side. With the original claimed, an unedited twin is pure duplication and retires
/// through the ordinary park-then-verify rail; an edited one is the person's work and stays, said
/// on the receipt.
fn retire_twin(
    sctx: &Ctx<'_>,
    target: &ClaimTarget,
    claimed: &Path,
    map: &PlacementMap,
) -> Result<Option<ClaimTwin>, ClientError> {
    let Some(parent) = claimed.parent() else {
        return Ok(None);
    };
    let Some(idx) = twin_index(sctx, target, claimed, parent, map) else {
        return Ok(None);
    };
    let dir = PathBuf::from(&map.placements[idx]);
    let folder = crate::ops::inventory::pretty(sctx, &dir);
    let scans = crate::placement::scan_placements(sctx, map)?;
    let clean = scans
        .iter()
        .find(|s| s.idx == idx)
        .is_some_and(|s| matches!(s.status, ScanStatus::Clean { .. }));
    if !clean {
        return Ok(Some(ClaimTwin {
            folder,
            removed: false,
        }));
    }
    super::reconcile::retire_placements(sctx, &target.id, &target.lock, map, &[idx])?;
    Ok(Some(ClaimTwin {
        folder,
        removed: true,
    }))
}

/// The placement index of a sibling of the claimed folder whose dir name is the engine's
/// COLLISION-SUFFIXED choice for this bundle (`<name>-<suffix>`) — the only shape a duplicate can
/// legitimately take. The person's own adopted folders are never candidates.
fn twin_index(
    sctx: &Ctx<'_>,
    target: &ClaimTarget,
    claimed: &Path,
    parent: &Path,
    map: &PlacementMap,
) -> Option<usize> {
    let base = topos_harness::sanitize_skill_dir(&target.lock.name)?;
    let prefix = format!("{base}-");
    map.placements
        .iter()
        .zip(&map.placement_state)
        .position(|(p, st)| {
            let dir = Path::new(p);
            dir != claimed
                && !st.adopted_source
                && st.materialized_sha.is_some()
                && dir.parent() == Some(parent)
                && dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|leaf| leaf.starts_with(&prefix))
                && sctx.fs.exists(dir)
        })
}

// ---------------------------------------------------------------------------------------------
// The receipt + the dest-pinned row's extension
// ---------------------------------------------------------------------------------------------

/// The claim's own receipt: the bundle it is about, where the record's bytes come from, and the
/// claim block the renderer turns into its two lines. No manifest fields — a claim over an
/// unpinned row edits no file, and the pinned case fills them in through the ordinary row write.
fn receipt(
    ctx: &Ctx<'_>,
    target: &ClaimTarget,
    dir: &Path,
    state: ClaimState,
    twin: Option<ClaimTwin>,
) -> AddData {
    AddData {
        skill_id: Some(target.id.as_str().to_owned()),
        name: target.lock.name.clone(),
        version_id: Some(target.lock.base_commit.clone()),
        bundle_digest: Some(target.lock.bundle_digest.clone()),
        tracked: true,
        // Nothing was adopted and no trigger moved: the record already stood.
        currency: None,
        triggers: Vec::new(),
        origin: None,
        // The RECORD's source, absolute where it is a folder — the wire's own form. The TTY
        // abbreviates it under the home, once, in the renderer.
        source: target.origin.as_ref().map(|o| match o {
            RecordOrigin::Reference(r) => r.clone(),
            RecordOrigin::Folder(folder) => folder.display().to_string(),
        }),
        manifest: None,
        scope: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        published_match: None,
        note: None,
        mcp: None,
        dest: Vec::new(),
        dest_resolved: Vec::new(),
        dest_change: None,
        display: None,
        unchanged: false,
        machine_copy: None,
        set_delivery: None,
        claim: Some(ClaimReceipt {
            folder: crate::ops::inventory::pretty(ctx, dir),
            state,
            twin,
        }),
    }
}

/// A DEST-FROZEN row plans from its `dest` entries ALONE, so a record-only claim would be starved
/// by the next sweep: the claimed folder's root joins the row's destinations through C1's additive
/// write. A row that names none reaches every agent already and is left exactly as it is — the
/// file is the recipe, and this claim added no rule to it.
///
/// Returns the entry this call ADDED, and `None` whenever the file was left as it stood (no row, an
/// unpinned row, or a row that already named the root). Idempotent, so a re-run repairs a claim
/// that crashed between the record write and this one.
fn extend_row_dest(
    ctx: &Ctx<'_>,
    scope: &AddScope,
    target: &ClaimTarget,
    dir: &Path,
    data: &mut AddData,
) -> Result<Option<String>, ClientError> {
    // A folder-adopted record's row IS its source folder; a claim adds a second place through the
    // record, and the row's key still names the original.
    let Some(reference) = row_reference(scope, target) else {
        return Ok(None);
    };
    let Some(row) = standing_row(ctx, scope, &reference)? else {
        return Ok(None);
    };
    if row.fields().dest.is_none() {
        return Ok(None);
    }
    write_dest(ctx, scope, &reference, dir, data)
}

/// The row this scope's file already spells for `reference`, if any.
fn standing_row(
    ctx: &Ctx<'_>,
    scope: &AddScope,
    reference: &str,
) -> Result<Option<crate::manifest::document::EntryValue>, ClientError> {
    super::manifest_edit::row_value(ctx, &scope.target, reference)
}

/// The additive `dest` write: the claimed folder's ROOT, in the scope's own dest dialect.
fn write_dest(
    ctx: &Ctx<'_>,
    scope: &AddScope,
    reference: &str,
    dir: &Path,
    data: &mut AddData,
) -> Result<Option<String>, ClientError> {
    let Some(parent) = dir.parent() else {
        return Ok(None);
    };
    let entry = super::manifest_edit::dest_spelling(ctx, &scope.target, parent);
    super::manifest_edit::write_row(
        ctx,
        data,
        &scope.target,
        reference,
        &crate::manifest::document::EntryValue::Fields(crate::manifest::document::EntryFields {
            dest: Some(vec![entry.clone()]),
            ..Default::default()
        }),
    )?;
    // ONE ANSWER PER SURFACE. The row write computes its own destination-subtraction undo, but a
    // claim's inverse is not that write — it is the DETACH, which drops the placement AND takes
    // the entry back, and which a pinned and an unpinned claim both take. The claim receipt names
    // no undo (an unpinned claim writes no file at all, so there is nothing uniform to offer), and
    // a leftover half-inverse in the envelope would be the one surface saying something else.
    data.undo = Vec::new();
    // The row's own answer decides what was ADDED: an extend reports its change, and a row that
    // already named this root reports none (`write_row`'s redundancy arm) — the exact distinction
    // the detach later needs, taken from the writer rather than guessed at afterwards.
    Ok(data
        .dest_change
        .as_ref()
        .filter(|change| change.added.contains(&entry))
        .map(|_| entry))
}
