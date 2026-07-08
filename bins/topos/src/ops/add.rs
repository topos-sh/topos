//! `add <path>` — adopt a local skill, offline. Mint an id + name, scan + import to the embedded-git
//! store, snapshot the genesis version, and write the sidecar docs — all staged and published with one
//! directory rename, so adoption is all-or-nothing and the user's source bytes are never touched.

use std::path::Path;

use topos_core::digest::to_hex;
use topos_core::sign::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_harness::DiscoveredPlacement;
use topos_types::persisted::{
    Lock, LockedFile, PlacementMap, RecordedTuple, SwapCapability, SyncState,
};
use topos_types::results::AddData;
use topos_types::{Generation, PERSISTED_SCHEMA_VERSION};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::scan::{self, ScannedBundle};
use crate::{doc, logfile, sidecar};

/// The fixed, controlled-ASCII commit message for a genesis adopt — folded into the `version_id`
/// preimage, so it must stay constant for a deterministic id.
const ADD_MESSAGE: &str = "topos: add";

/// Adopt the skill rooted at `source`.
///
/// # Errors
/// [`ClientError::SourceOverlap`] if `source` overlaps `~/.topos/`; [`ClientError::EmptyBundle`] /
/// [`ClientError::Scan`] from the scan; [`ClientError::SkillExists`] on an id collision; otherwise a
/// store/io failure.
pub(crate) fn add(ctx: &Ctx<'_>, source: &Path) -> Result<AddData, ClientError> {
    // Establish the home, then refuse a source that overlaps it (canonicalized — catches symlinks), so
    // uninstall can never delete user bytes and the footprint oracle never collapses.
    ctx.fs.create_dir_all(ctx.layout.home())?;
    reject_overlap(source, ctx.layout.home())?;

    let bundle = scan::scan(source)?;
    let source_abs = source
        .canonicalize()
        .map_err(|e| ClientError::Io(format!("canonicalize {}: {e}", source.display())))?;

    // Adopt-in-place is non-destructive, so re-adopting the same directory would mint a SECOND record
    // tracking one mutable dir — refuse, pointing at the skill already tracking it.
    reject_already_tracked(ctx, &source_abs)?;

    // Recognize a known harness: a source that IS one of the harness's discovered skill placements
    // (canonical equality — never a prefix, so a subdir is not mistaken for the skill) is tagged so
    // currency applies to it. A plain/unrecognized dir is tracked in place with no harness association.
    let recognized = recognize(ctx, &source_abs);

    // Mint identity. A recognized harness skill is keyed by its DIRECTORY name (the command name the
    // harness invokes); a plain dir keeps the frontmatter-first-then-basename order. The minted id is
    // parsed through the validated newtype like any other (the id source mints `topos_<hex>`, which
    // always fits — the parse is the type-level proof the path joins below demand).
    let skill_id = crate::id::SkillId::parse(&ctx.ids.new_skill_id())?;
    let name = match &recognized {
        Some(placement) => dir_basename(&placement.path).unwrap_or_else(|| skill_id.to_string()),
        None => bundle
            .name_hint
            .clone()
            .or_else(|| dir_basename(&source_abs))
            .unwrap_or_else(|| skill_id.to_string()),
    };

    // version_id depends ONLY on the bytes + device id + the fixed message — never the id/time/RNG — so a
    // fixed fixture pins it while ids stay free.
    let version_id = sign::commit_id(&Commit {
        parents: &[],
        tree: bundle.bundle_digest,
        author: &ctx.device_id,
        message: ADD_MESSAGE,
    })
    .map_err(|_| ClientError::Corrupt("commit id preimage".into()))?;

    // Serialize this id's writers; the lock lives outside skills/<id>/ so the publish rename can't drop it.
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &skill_id)?;
    if ctx.fs.exists(&ctx.layout.skill_dir(&skill_id)) {
        return Err(ClientError::SkillExists);
    }

    // Build the whole skill in a staging dir; a leftover from a prior crash is ours to clear (we hold the lock).
    let (staging_base, sp) = ctx.layout.staging(&skill_id);
    if ctx.fs.exists(&staging_base) {
        ctx.fs.remove_dir_all(&staging_base)?;
    }
    ctx.fs.create_dir_all(&sp.store)?;

    // Import + snapshot into the embedded git store.
    let store = Store::init(&sp.store)?;
    let import: Vec<ImportFile<'_>> = bundle
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store.commit(version_id, &[], &tree, &ctx.device_id, ADD_MESSAGE)?;

    // Make the git objects durable BEFORE any doc references them (the ordering invariant). The
    // full-tree durability set is exactly right HERE (and only here + the `follow` baseline's empty
    // init): a fresh staging store's whole tree IS this op's writes — it never carries history.
    super::sync_engine::fsync_batch(ctx, &store.durability_set()?)?;

    // Write the docs (sync → map → lock), lock LAST as the commit marker.
    let version_hex = to_hex(&version_id);
    let digest_hex = to_hex(&bundle.bundle_digest);
    let genesis = Generation { epoch: 0, seq: 0 };
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: PERSISTED_SCHEMA_VERSION,
            observed: genesis,
            applied: genesis,
            recorded: vec![RecordedTuple {
                generation: genesis,
                commit_id: version_hex.clone(),
            }],
            base_commit: version_hex.clone(),
            work_hash: digest_hex.clone(),
            held: false,
        },
    )?;
    // Attribute the harness. Either the adapter recognized it (adopt-in-place; currency armed below), OR
    // the baked registry places the source under a known harness's skill dir — recorded for forward-compat
    // even when topos has no full adapter for it (a later adapter can arm currency for this adopted skill).
    // A plain dir under no harness stays `None` on every field.
    let harness_slug = match &recognized {
        Some(_) => Some(ctx.harness.id().slug().to_owned()),
        None => registry_attribution(&source_abs).map(|a| a.slug),
    };

    // Record the placement: the harness skill dir for a recognized skill (the path the harness reads),
    // else the canonical source. Topos writes NOTHING into this dir — it stays byte-identical.
    let (placement, harness, harness_layer) = match &recognized {
        Some(p) => (
            p.path.to_string_lossy().into_owned(),
            Some(ctx.harness.id()),
            p.layer.clone(),
        ),
        None => (source_abs.to_string_lossy().into_owned(), None, None),
    };
    doc::write_doc(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: PERSISTED_SCHEMA_VERSION,
            placements: vec![placement],
            applied_commit: version_hex.clone(),
            materialized_sha: digest_hex.clone(),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            harness,
            harness_layer,
            harness_slug: harness_slug.clone(),
        },
    )?;
    doc::write_doc(
        ctx.fs,
        &sp.lock,
        &Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: skill_id.to_string(),
            name: name.clone(),
            base_commit: version_hex.clone(),
            bundle_digest: digest_hex.clone(),
            files: locked_files(&bundle),
        },
    )?;

    // Publish atomically (no-replace), then fsync the parent so the rename is durable.
    ctx.fs
        .rename_dir_noreplace(&staging_base, &ctx.layout.skill_dir(&skill_id))
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                ClientError::SkillExists
            } else {
                ClientError::Io(format!("publish {skill_id}: {e}"))
            }
        })?;
    ctx.fs.fsync_dir(&ctx.layout.skills_dir())?;

    logfile::append_event(
        ctx.fs,
        &ctx.layout.log_path(),
        &serde_json::json!({
            "action": "add",
            "skill_id": skill_id.as_str(),
            "name": name,
            "version_id": version_hex,
            "at": ctx.clock.now_unix_millis(),
        }),
    )?;

    // Arm currency for a recognized harness — a best-effort, idempotent edit of the harness CONFIG
    // (never the skill dir), AFTER the all-or-nothing adoption above, so a settings.json hiccup never
    // rolls back a good adoption. Disclosed in the result (the only write `add` makes outside ~/.topos/).
    let currency = recognized
        .as_ref()
        .map(|_| ctx.harness.install_currency_trigger());

    Ok(AddData {
        skill_id: skill_id.into_string(),
        name,
        version_id: version_hex,
        bundle_digest: digest_hex,
        tracked: true,
        harness,
        harness_slug,
        currency,
    })
}

fn locked_files(bundle: &ScannedBundle) -> Vec<LockedFile> {
    bundle
        .files
        .iter()
        .map(|f| LockedFile {
            path: f.path.clone(),
            mode: f.mode.as_str().to_owned(),
            sha256: to_hex(&topos_core::digest::sha256(&f.bytes)),
            size: u64::try_from(f.bytes.len()).unwrap_or(u64::MAX),
        })
        .collect()
}

fn dir_basename(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Refuse to re-adopt a directory topos already tracks (same canonical path). Best-effort: the writer
/// lock is per fresh skill id, so a rare concurrent `add` of the same dir could still race through to
/// today's same-name `AmbiguousName`; the common re-run is caught here.
///
/// # Errors
/// [`ClientError::AlreadyTracked`] if a tracked skill already records this canonical path; otherwise an
/// [`FsOps`](crate::fs_seam::FsOps) read failure.
fn reject_already_tracked(ctx: &Ctx<'_>, canonical_source: &Path) -> Result<(), ClientError> {
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        let Ok(id) = crate::id::SkillId::parse(id) else {
            continue; // not a topos-minted dir name
        };
        let Some(map) = doc::read_doc::<PlacementMap>(ctx.fs, &ctx.layout.published(&id).map)?
        else {
            continue;
        };
        // Compare canonically (resolving symlinks/firmlinks on both sides), as `Path`, never a lossy
        // string; a placement that no longer resolves on disk is stale, not a match.
        if map.placements.iter().any(|p| {
            Path::new(p)
                .canonicalize()
                .is_ok_and(|c| c == *canonical_source)
        }) {
            return Err(ClientError::AlreadyTracked {
                skill_id: id.into_string(),
            });
        }
    }
    Ok(())
}

/// Match a canonical source dir against the harness's discovered placements by canonical EQUALITY (not
/// a prefix — a subdir of a skill is never tagged as that skill). Returns the matched placement, or
/// `None` for a plain/unrecognized dir.
fn recognize(ctx: &Ctx<'_>, canonical_source: &Path) -> Option<DiscoveredPlacement> {
    ctx.harness
        .discover()
        .into_iter()
        .find(|d| d.path.canonicalize().is_ok_and(|c| c == *canonical_source))
}

/// Which known harness's skill dir `source_abs` sits under (baked registry), using the real env home + cwd.
/// Best-effort provenance: no `$HOME` ⇒ no attribution.
fn registry_attribution(source_abs: &Path) -> Option<topos_harness::registry::HarnessAttribution> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let cwd = std::env::current_dir().ok();
    topos_harness::registry::attribute_path(source_abs, &home, cwd.as_deref())
}

/// Refuse a source path that is equal to, an ancestor of, or a descendant of `~/.topos/` (canonicalized,
/// so a symlink can't obscure the overlap).
fn reject_overlap(source: &Path, home: &Path) -> Result<(), ClientError> {
    let source = source
        .canonicalize()
        .map_err(|e| ClientError::Io(format!("canonicalize {}: {e}", source.display())))?;
    let home = home
        .canonicalize()
        .map_err(|e| ClientError::Io(format!("canonicalize {}: {e}", home.display())))?;
    if source == home || source.starts_with(&home) || home.starts_with(&source) {
        return Err(ClientError::SourceOverlap);
    }
    Ok(())
}
