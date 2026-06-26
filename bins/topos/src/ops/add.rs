//! `add <path>` — adopt a local skill, offline. Mint an id + name, scan + import to the embedded-git
//! store, snapshot the genesis version, and write the sidecar docs — all staged and published with one
//! directory rename, so adoption is all-or-nothing and the user's source bytes are never touched.

use std::path::Path;

use topos_core::digest::to_hex;
use topos_core::sign::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_types::persisted::{
    Lock, LockedFile, PlacementMap, RecordedTuple, SwapCapability, SyncState,
};
use topos_types::results::AddData;
use topos_types::{Generation, SCHEMA_VERSION};

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

    // Mint identity. The name prefers SKILL.md frontmatter, then the dir basename, then the id.
    let skill_id = ctx.ids.new_skill_id();
    let name = bundle
        .name_hint
        .clone()
        .or_else(|| dir_basename(&source_abs))
        .unwrap_or_else(|| skill_id.clone());

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

    // Make the git objects durable BEFORE any doc references them (the ordering invariant).
    let batch = store.durability_set()?;
    for file in &batch.files {
        ctx.fs.fsync_file(file)?;
    }
    for dir in &batch.dirs {
        ctx.fs.fsync_dir(dir)?;
    }

    // Write the docs (sync → map → lock), lock LAST as the commit marker.
    let version_hex = to_hex(&version_id);
    let digest_hex = to_hex(&bundle.bundle_digest);
    let genesis = Generation { epoch: 0, seq: 0 };
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: SCHEMA_VERSION,
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
    doc::write_doc(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: SCHEMA_VERSION,
            placements: vec![source_abs.to_string_lossy().into_owned()],
            applied_commit: version_hex.clone(),
            materialized_sha: digest_hex.clone(),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
        },
    )?;
    doc::write_doc(
        ctx.fs,
        &sp.lock,
        &Lock {
            schema_version: SCHEMA_VERSION,
            skill_id: skill_id.clone(),
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
            "skill_id": skill_id,
            "name": name,
            "version_id": version_hex,
            "at": ctx.clock.now_unix_millis(),
        }),
    )?;

    Ok(AddData {
        skill_id,
        name,
        version_id: version_hex,
        bundle_digest: digest_hex,
        tracked: true,
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
