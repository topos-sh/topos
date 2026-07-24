//! The BUILT-IN `topos` skill — the meta-skill that teaches an agent what topos is and how to
//! drive it. Its source lives at the repo TOP LEVEL (`skills/topos/` — an authored `SKILL.md` +
//! `INSTALL.md`, downloadable straight from the public repo by skill installers), and the binary
//! embeds THOSE files: one source of truth, so a downloaded copy and a binary-placed copy carry
//! the same authored bytes. The bundle's third file is the generated verb reference `docs/cli.md`
//! carries, rendered from this binary's real clap tree. It lands through the ordinary placement
//! engine at the moments the auto-update triggers arm, and re-syncs on every bare sweep. It is
//! FORCE-SYNCED to the binary: it documents THIS binary's verb surface, so any divergence — a
//! hand edit, an old binary's bytes — is overwritten on the next sweep (an edited copy is still
//! snapshotted into the sidecar store first; it just never becomes a draft). A pre-existing
//! `topos` dir is NEVER written by the sweep (the Foreign freeze — marker or not): one whose
//! SKILL.md frontmatter carries the public copy's provenance marker (a `metadata:` entry,
//! `topos: builtin`) is a stale DOWNLOADED copy that the CONSENTED `topos add topos`
//! adopts — snapshot-first, then force-synced and managed; without the marker the dir is
//! someone else's and stays a frozen Foreign reservation.
//!
//! Device-local surface: `topos remove topos` opts this machine out durably
//! (`state/builtin.json`), `topos add topos` re-places it, and the `--agent` include/exclude
//! scoping works exactly as on a followed skill (the scope lives in the same state doc — the
//! built-in has no `follows.json` row: it is not a subscription, and the plane never hears of it).
//! The name `topos` is reserved end-to-end (the placement naming discipline client-side, the
//! catalog name mint plane-side), so a workspace skill can never shadow it.

use serde::{Deserialize, Serialize};
use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::identity::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, PlacementMap, SyncState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::materialize::{self, MaterializeReq};
use crate::placement::{self, AgentScope, ScanStatus};
use crate::scan::{ScannedBundle, ScannedFile};
use crate::{doc, sidecar};

use super::sync_engine;

/// The reserved name — the skill id AND the tracked name AND the placement dir name.
pub(crate) const BUILTIN_NAME: &str = "topos";

/// The fixed, controlled-ASCII commit message for a built-in version (folded into the `version_id`
/// preimage, like `add`'s).
const BUILTIN_MESSAGE: &str = "topos: builtin";

/// The authored halves of the bundle, embedded from the repo-top-level source (`skills/topos/` —
/// the SAME files a skill installer downloads from the public repo, so placed and downloaded
/// copies match byte-for-byte).
const SKILL_MD: &str = include_str!("../../../../skills/topos/SKILL.md");
const INSTALL_MD: &str = include_str!("../../../../skills/topos/INSTALL.md");

/// The provenance line the public SKILL.md carries in its frontmatter (a `metadata` entry, which
/// skill installers copy verbatim). A pre-existing `topos` placement dir WITH the marker is a
/// stale downloaded copy of THIS bundle — adopted only by the consented `add topos`,
/// snapshot-first; the silent sweep never writes it. Without it the dir is someone else's and the
/// Foreign freeze stands everywhere.
const PROVENANCE_MARKER: &str = "topos: builtin";

/// Whether a skill id names the built-in (ordinary minted ids are `topos_<hex>`, so the bare name
/// can never collide).
pub(crate) fn is_builtin(id: &str) -> bool {
    id == BUILTIN_NAME
}

/// Whether a Foreign-scanned placement holds a DOWNLOADED copy of this skill (see
/// [`marker_in_frontmatter`]). Gates only the CONSENTED `add topos` adoption — the
/// silent sweep never writes a Foreign dir, marker or not. Best-effort and fail-closed: an absent
/// or unreadable file answers `false` (never adopt on doubt).
fn is_downloaded_copy(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(dir.join("SKILL.md"))
        .map(|text| marker_in_frontmatter(&text))
        .unwrap_or(false)
}

/// Whether a SKILL.md's TERMINATED leading frontmatter block carries the provenance marker as a
/// DIRECT `metadata:` entry — the exact shape the public copy publishes and skill installers copy
/// verbatim. A tiny top-level-key state machine, fail-closed: the file must open with `---`;
/// scanning stops at the closing `---` (an unterminated header answers `false`); an unindented
/// line sets the current top-level key; under `metadata:`, the FIRST indented line fixes the
/// direct-child indent (space-only), and the marker counts ONLY at exactly that indent — a
/// root-level `topos: builtin`, one inside another key's block scalar, or one nested DEEPER under
/// `metadata:` (e.g. inside a `notes: |` scalar) never matches; a tab in leading whitespace
/// rejects the line outright.
pub(crate) fn marker_in_frontmatter(text: &str) -> bool {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    let mut in_metadata = false;
    let mut child_indent: Option<usize> = None;
    let mut found = false;
    for line in lines {
        if line.trim_end() == "---" {
            return found; // the terminated block's verdict
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if !in_metadata {
                continue;
            }
            let after_spaces = line.trim_start_matches(' ');
            if after_spaces.starts_with('\t') {
                continue; // a tab in leading whitespace: not the published shape
            }
            let indent = line.len() - after_spaces.len();
            // The first indented line under `metadata:` fixes the direct-child indent; anything
            // deeper is nested content (a sub-key's block scalar), never a direct entry.
            let direct = *child_indent.get_or_insert(indent);
            if indent == direct && line.trim() == PROVENANCE_MARKER {
                found = true;
            }
        } else {
            // Any unindented line moves the top-level context (a non-`metadata:` line clears it).
            in_metadata = line.trim_end() == "metadata:";
            child_indent = None;
        }
    }
    false // the frontmatter never closed — not the published shape
}

fn builtin_sid() -> Result<SkillId, ClientError> {
    SkillId::parse(BUILTIN_NAME)
}

// ---------------------------------------------------------------------------------------------
// The durable device-local state (`state/builtin.json`) — the opt-out + the agent scope. NOT a
// `follows.json` row: the built-in is not a subscription.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BuiltinState {
    pub schema_version: u32,
    /// `topos remove topos` — the durable opt-out; no sweep re-places while set.
    #[serde(default)]
    pub removed: bool,
    /// The `--agent` include-list (empty = unscoped).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// The per-agent exclusions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_agents: Vec<String>,
}

impl Default for BuiltinState {
    fn default() -> Self {
        Self {
            schema_version: PERSISTED_SCHEMA_VERSION,
            removed: false,
            agents: Vec::new(),
            excluded_agents: Vec::new(),
        }
    }
}

pub(crate) fn read_state(ctx: &Ctx<'_>) -> Result<BuiltinState, ClientError> {
    Ok(doc::read_doc(ctx.fs, &ctx.layout.builtin_state_path())?.unwrap_or_default())
}

pub(crate) fn write_state(ctx: &Ctx<'_>, state: &BuiltinState) -> Result<(), ClientError> {
    ctx.fs.create_dir_all(&ctx.layout.state_dir())?;
    doc::write_doc(ctx.fs, &ctx.layout.builtin_state_path(), state)
}

// ---------------------------------------------------------------------------------------------
// The rendered bundle — deterministic for a given binary.
// ---------------------------------------------------------------------------------------------

/// Render the bundle bytes from the binary: the embedded `SKILL.md` + `INSTALL.md` (verbatim —
/// carrying no version stamp, so the committed source IS the placed bytes) + the generated verb
/// reference (the same renderer `cargo xtask gen-cli-ref` writes `docs/cli.md` with — one
/// implementation, so the placed reference can never drift from what this binary parses).
fn rendered_bundle() -> Result<ScannedBundle, ClientError> {
    // Sorted by raw path bytes, the scanner's invariant ("I" < "S" < "r").
    let files = vec![
        ScannedFile {
            path: "INSTALL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: INSTALL_MD.as_bytes().to_vec(),
        },
        ScannedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: SKILL_MD.as_bytes().to_vec(),
        },
        ScannedFile {
            path: "reference.md".to_owned(),
            mode: FileMode::Regular,
            bytes: crate::cli_ref::cli_ref_md().into_bytes(),
        },
    ];
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let bundle_digest = digest::bundle_digest(&entries)?;
    Ok(ScannedBundle {
        files,
        bundle_digest,
        name_hint: Some(BUILTIN_NAME.to_owned()),
    })
}

// ---------------------------------------------------------------------------------------------
// ensure — create/refresh the sidecar entry and converge every planned placement.
// ---------------------------------------------------------------------------------------------

/// What a sync did (the quiet hook ORs `changed` into its `reloadSkills` decision).
#[derive(Debug, Default)]
pub(crate) struct BuiltinSync {
    pub changed: bool,
}

/// What the converge may do to a Foreign-scanned placement dir (one the record says the built-in
/// never wrote).
#[derive(Clone, Copy, PartialEq)]
enum ForeignPosture {
    /// The silent sweep: never write it, marker or not.
    Freeze,
    /// The consented `add topos` restore: adopt a MARKED downloaded copy
    /// (snapshot-first); an unmarked dir stays frozen exactly as under [`Self::Freeze`].
    AdoptMarked,
}

/// Place/refresh the built-in skill: create the sidecar entry on first contact, commit a new
/// version when the binary's rendered bytes moved (upgrade or downgrade — the binary is
/// authoritative), then converge every planned placement, overwriting ANY divergent copy
/// (snapshot-first). Honors the durable opt-out. Runs at the trigger-arming moments (`add`'s adopt
/// receipt, the enrollment receipt) and on every bare `update` sweep — always with the Foreign
/// freeze: a dir the record says we never wrote is never written here.
pub(crate) fn ensure_builtin(ctx: &Ctx<'_>) -> Result<BuiltinSync, ClientError> {
    ensure_inner(ctx, &rendered_bundle()?, ForeignPosture::Freeze)
}

/// [`ensure_builtin`] over an explicit bundle — the seam the tests drive a "binary changed" refresh
/// through (production always renders from the binary and goes through [`ensure_builtin`] /
/// the restore's adopting call, so this wrapper is test-only).
#[cfg(test)]
pub(crate) fn ensure_with(
    ctx: &Ctx<'_>,
    bundle: &ScannedBundle,
) -> Result<BuiltinSync, ClientError> {
    ensure_inner(ctx, bundle, ForeignPosture::Freeze)
}

fn ensure_inner(
    ctx: &Ctx<'_>,
    bundle: &ScannedBundle,
    posture: ForeignPosture,
) -> Result<BuiltinSync, ClientError> {
    let state = read_state(ctx)?;
    if state.removed {
        return Ok(BuiltinSync::default());
    }
    let sid = builtin_sid()?;
    ctx.fs.create_dir_all(ctx.layout.home())?;
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, &sid)?;
    let digest_hex = to_hex(&bundle.bundle_digest);
    let sp = ctx.layout.published(&sid);

    if !ctx.fs.exists(&ctx.layout.skill_dir(&sid)) {
        create_builtin(ctx, &sid, bundle)?;
    }

    let mut lock: Lock = doc::read_doc(ctx.fs, &sp.lock)?
        .ok_or_else(|| ClientError::Corrupt("built-in skill: missing lock".into()))?;
    let mut sync: SyncState = doc::read_doc(ctx.fs, &sp.sync)?
        .ok_or_else(|| ClientError::Corrupt("built-in skill: missing sync state".into()))?;
    let map = sync_engine::read_map_required(ctx, &sp)?;

    // The binary's bytes moved — commit the new version forward on the built-in's local history.
    if lock.bundle_digest != digest_hex {
        let parent = super::parse_hex32(&lock.base_commit)?;
        let version_id = identity::commit_id(&Commit {
            parents: &[parent],
            tree: bundle.bundle_digest,
            author: &ctx.device_id,
            message: BUILTIN_MESSAGE,
        })
        .map_err(|_| ClientError::Corrupt("built-in commit id preimage".into()))?;
        let store = Store::open(&sp.store)?;
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
        store.commit(
            version_id,
            &[parent],
            &tree,
            &ctx.device_id,
            BUILTIN_MESSAGE,
        )?;
        sync_engine::fsync_batch(ctx, &store.version_durability(&version_id)?)?;
        let version_hex = to_hex(&version_id);
        lock = Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: sid.to_string(),
            name: BUILTIN_NAME.to_owned(),
            base_commit: version_hex.clone(),
            bundle_digest: digest_hex.clone(),
            files: super::add::locked_files(bundle),
        };
        sync = SyncState {
            observed_version_id: version_hex.clone(),
            base_commit: version_hex,
            work_hash: digest_hex.clone(),
            ..sync
        };
    }

    // Plan through the ONE engine (shared-dir-first; the state doc's agent scope), reconcile, and
    // land the bytes on every managed target that is absent or divergent — force-sync.
    let plan = placement::plan_targets(
        ctx,
        sid.as_str(),
        topos_harness::PlacementNaming {
            name: Some(BUILTIN_NAME),
            workspace_slug: None,
        },
        AgentScope {
            agents: &state.agents,
            excluded: &state.excluded_agents,
        },
        Some(&map),
        None,
    );
    let next = placement::reconcile_map(&map, &plan);
    let managed = placement::managed_indices(&next, &plan);
    let scans = placement::scan_placements(ctx, &next)?;
    let targets: Vec<usize> = managed
        .into_iter()
        .filter(|&i| match &scans[i].status {
            ScanStatus::Absent => true,
            ScanStatus::Clean { digest } => to_hex(digest) != digest_hex,
            ScanStatus::Modified { scanned } => to_hex(&scanned.bundle_digest) != digest_hex,
            // Never a foreign dir (not ours to write) — the ONE exception is the consented
            // `add topos` restore, whose AdoptMarked posture takes over a dir holding a
            // DOWNLOADED copy of this very skill (the public SKILL.md's provenance marker): the
            // materializer snapshots its bytes into the sidecar store first, then force-syncs
            // like any divergent copy. The silent sweep always passes Freeze. Never an unreadable
            // dir (fail open here — the sweep must not brick a session start over one odd
            // placement).
            ScanStatus::Foreign => {
                posture == ForeignPosture::AdoptMarked && is_downloaded_copy(&scans[i].dir)
            }
            ScanStatus::Unscannable => false,
        })
        .collect();

    if targets.is_empty() {
        // Nothing to land; persist any doc-level movement (a refreshed version with no detected
        // placement, a reconciled record) in the load-bearing order.
        if lock.bundle_digest != map.materialized_sha
            || next.placements.len() != map.placements.len()
        {
            let next_map = PlacementMap {
                applied_commit: lock.base_commit.clone(),
                materialized_sha: digest_hex,
                ..next
            };
            materialize::commit_docs(ctx.fs, &sp, &next_map, &lock, &sync)?;
        }
        return Ok(BuiltinSync::default());
    }

    let base = super::parse_hex32(&lock.base_commit)?;
    let store = Store::open(&sp.store)?;
    let rendered = store.render_verified(base, bundle.bundle_digest)?;
    sync_engine::fsync_batch(ctx, &store.version_durability(&base)?)?;
    let next_map = PlacementMap {
        applied_commit: lock.base_commit.clone(),
        materialized_sha: digest_hex,
        ..next
    };
    let lock_ref = &lock;
    materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id: sid.as_str(),
            target_indices: &targets,
            bundle: &rendered,
            next_map,
            next_lock: lock_ref,
            next_sync: &sync,
            sp: &sp,
            // Force-sync is still never a lost byte: an edited copy is committed into the sidecar
            // store before its dir is overwritten.
            snapshot: Some(&|s: &crate::scan::ScannedBundle| {
                sync_engine::snapshot_draft(ctx, &sp, lock_ref, s).map(|_| ())
            }),
            // The consented `add topos` restore takes over the marked downloaded copy —
            // an occupied, never-materialized dir the target filter admitted only under
            // AdoptMarked. The predicate re-proves the marker against the LIVE dir immediately
            // before the overwrite, so a copy that lost it since the describe fails closed. The
            // silent sweep (Freeze) never targets such a dir and passes no takeover.
            takeover: (posture == ForeignPosture::AdoptMarked)
                .then_some(&is_downloaded_copy as &dyn Fn(&std::path::Path) -> bool),
        },
    )?;
    Ok(BuiltinSync { changed: true })
}

/// First contact: stage the whole sidecar entry (store + docs, EMPTY placements — the converge in
/// [`ensure_builtin`] lands the dirs) and publish it with one rename, exactly like `add`.
fn create_builtin(ctx: &Ctx<'_>, sid: &SkillId, bundle: &ScannedBundle) -> Result<(), ClientError> {
    let version_id = identity::commit_id(&Commit {
        parents: &[],
        tree: bundle.bundle_digest,
        author: &ctx.device_id,
        message: BUILTIN_MESSAGE,
    })
    .map_err(|_| ClientError::Corrupt("built-in commit id preimage".into()))?;

    let (staging_base, sp) = ctx.layout.staging(sid);
    if ctx.fs.exists(&staging_base) {
        ctx.fs.remove_dir_all(&staging_base)?;
    }
    ctx.fs.create_dir_all(&sp.store)?;
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
    store.commit(version_id, &[], &tree, &ctx.device_id, BUILTIN_MESSAGE)?;
    sync_engine::fsync_batch(ctx, &store.durability_set()?)?;

    let version_hex = to_hex(&version_id);
    let digest_hex = to_hex(&bundle.bundle_digest);
    let genesis: u64 = 0;
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: PERSISTED_SCHEMA_VERSION,
            observed: genesis,
            observed_version_id: version_hex.clone(),
            applied: genesis,
            base_commit: version_hex.clone(),
            work_hash: digest_hex.clone(),
            held: false,
        },
    )?;
    doc::write_map(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: Vec::new(),
            applied_commit: version_hex.clone(),
            materialized_sha: digest_hex.clone(),
            pre_existing_sha: None,
            swap_capability: topos_types::persisted::SwapCapability::Unsupported,
            placement_state: Vec::new(),
            harness: None,
            harness_layer: None,
            harness_slug: None,
        },
    )?;
    doc::write_doc(
        ctx.fs,
        &sp.lock,
        &Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: sid.to_string(),
            name: BUILTIN_NAME.to_owned(),
            base_commit: version_hex,
            bundle_digest: digest_hex,
            files: super::add::locked_files(bundle),
        },
    )?;
    ctx.fs
        .rename_dir_noreplace(&staging_base, &ctx.layout.skill_dir(sid))
        .map_err(|e| ClientError::Io(format!("publish {sid}: {e}")))?;
    ctx.fs.fsync_dir(&ctx.layout.skills_dir())?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `add topos` — re-place after a remove / repair in place.
// ---------------------------------------------------------------------------------------------

/// The placement dirs the built-in actually MATERIALIZED (what `remove topos` and `uninstall`
/// clean); empty when never placed. An occupied dir the built-in never wrote can sit in the
/// record as a frozen reservation (the reserved-name fallback resolves to the same `topos` dir,
/// and a Foreign scan keeps it byte-untouched) — it carries no materialized sha and is NEVER ours
/// to delete.
pub(crate) fn placement_dirs(ctx: &Ctx<'_>) -> Result<Vec<String>, ClientError> {
    let sid = builtin_sid()?;
    let sp = ctx.layout.published(&sid);
    Ok(doc::read_map(ctx.fs, &sp.map)?
        .map(|m| {
            m.placements
                .iter()
                .zip(&m.placement_state)
                .filter(|(_, st)| st.materialized_sha.is_some())
                .map(|(dir, _)| dir.clone())
                .collect()
        })
        .unwrap_or_default())
}

/// `add topos` — the opt-out's literal inverse and the downloaded-copy adoption: clear the
/// durable opt-out, then place/refresh with the AdoptMarked posture (a dir whose SKILL.md carries
/// the public copy's provenance marker is taken over snapshot-first; an unmarked foreign dir
/// stays frozen). Idempotent — an already-placed built-in just re-syncs.
pub(crate) fn restore_builtin(ctx: &Ctx<'_>) -> Result<BuiltinSync, ClientError> {
    let mut st = read_state(ctx)?;
    if st.removed {
        st.removed = false;
        write_state(ctx, &st)?;
    }
    ensure_inner(ctx, &rendered_bundle()?, ForeignPosture::AdoptMarked)
}

/// `remove topos --yes` — the durable opt-out: mark the state doc FIRST (the fact that must
/// survive), then delete the placements and the sidecar entry. Idempotent.
pub(crate) fn remove_builtin(ctx: &Ctx<'_>) -> Result<(), ClientError> {
    let mut st = read_state(ctx)?;
    st.removed = true;
    write_state(ctx, &st)?;
    let sid = builtin_sid()?;
    for dir in placement_dirs(ctx)? {
        let p = std::path::Path::new(&dir);
        if ctx.fs.exists(p) {
            ctx.fs.remove_dir_all(p)?;
        }
    }
    let skill_dir = ctx.layout.skill_dir(&sid);
    if ctx.fs.exists(&skill_dir) {
        ctx.fs.remove_dir_all(&skill_dir)?;
    }
    Ok(())
}
