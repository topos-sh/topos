//! `add <source>` — adopt a skill. The positional is source-polymorphic (classified in
//! [`crate::source`]): a local PATH (`./ ../ ~/ /…`) adopts a directory in place (see
//! [`adopt_path`]); a bare skill NAME resolves against the untracked inventory `list` discovers
//! (see [`resolve_add_target`]); a remote source
//! (`owner/repo`, a github.com URL) fetches + imports it (see [`add_remote`]). Adoption itself
//! ([`add_with_name`]) mints an id + name, scans + imports to the embedded-git store, snapshots the genesis
//! version, and writes the sidecar docs — all staged and published with one directory rename, so it is
//! all-or-nothing and the source bytes are never touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use topos_core::digest::{FileMode, to_hex};
use topos_core::identity::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_harness::DiscoveredPlacement;
use topos_harness::registry::SkillScope;
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, LockedFile, PlacementMap, SwapCapability, SyncState};
use topos_types::results::{
    AddData, KeepAsYoursData, KeepReason, ReceiptScope, SkillOrigin, UntrackedEntry,
};

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::{ClientError, CopyRelation, TargetCandidate, TrackedBy};
use crate::git_source::{GitTarballSource, RepoFile, extract_tree};
use crate::id::SkillId;
use crate::scan::{self, ScannedBundle};
use crate::source::RemoteSpec;
use crate::{doc, logfile, sidecar};

use super::manifest_edit::EditTarget;

/// The fixed, controlled-ASCII commit message for a genesis adopt — folded into the `version_id`
/// preimage, so it must stay constant for a deterministic id.
const ADD_MESSAGE: &str = "topos: add";

/// Whether the invocation SAID what kind it was adding. The server-bundle guard arms only on
/// SILENCE: a folder that is plainly an MCP bundle refuses toward `--kind mcp` when nobody said
/// what it was, and proceeds as exactly what was asked for when somebody did — an explicit word is
/// the person's own, and this door does not second-guess it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KindDeclared {
    /// No `--kind` on the command line — the guard stands.
    No,
    /// `--kind <word>` was given — the guard stands down.
    Yes,
}

/// Whether `dir`'s root carries a `server.json` the plain skill doors ([`add`], an undeclared
/// [`adopt_path`], publish's auto-add) must not read as a skill. Two shapes refuse, both toward
/// the `--kind` word ([`ClientError::KindRequired`]): a `server.json` with NO `SKILL.md` is an MCP
/// bundle the skill door would mis-kind, delivering raw JSON into skills dirs; a folder with BOTH
/// markers says nothing about which it is, and adopting it as a skill drops the server silently.
///
/// # Errors
/// [`ClientError::KindRequired`] naming the `--kind` word(s) that resolve it.
pub(crate) fn refuse_unflagged_mcp_dir(ctx: &Ctx<'_>, source: &Path) -> Result<(), ClientError> {
    let has_server = matches!(ctx.fs.read_opt(&source.join("server.json")), Ok(Some(_)));
    if !has_server {
        return Ok(());
    }
    let dir = source.display().to_string();
    // BOTH markers: the folder itself does not say which kind it is, and the skill door's answer
    // was to take the SKILL.md and drop the server without a word — half of what the person
    // pointed at, silently. Neither word is guessable from the bytes, so both are offered.
    if ctx.fs.exists(&source.join("SKILL.md")) {
        return Err(ClientError::kind_ambiguous(&dir));
    }
    Err(ClientError::kind_required(&dir))
}

/// A source path that is not there refuses TYPED and PERMANENT, before any store work. The absence
/// used to surface as the canonicalize call's generic io failure — deep inside the adopt, after the
/// store home had been created, and classified RETRYABLE, which told an agent that running the same
/// command again was safe. It is not: the same path will be missing the second time too.
///
/// # Errors
/// [`ClientError::SourceMissing`] naming the path as the caller spelled it.
fn refuse_missing_source(ctx: &Ctx<'_>, source: &Path) -> Result<(), ClientError> {
    if ctx.fs.exists(source) {
        return Ok(());
    }
    Err(ClientError::SourceMissing {
        path: source.display().to_string(),
    })
}

/// The bundle-root marker every skill folder carries — and the one file [`origin_dir`] follows.
const SKILL_FILE: &str = "SKILL.md";

/// The FOLDER a named source really stands for.
///
/// An agent's skills dir often holds a LINK SHELL: a real directory whose only entries are links
/// into one other folder (`SKILL.md -> …/gstack/codex/SKILL.md`). Discovery lists it — its root
/// `SKILL.md` resolves to a regular file — so `add` has to take it, and what it takes is the
/// ORIGINAL: the folder those links point into, adopted in place exactly as if it had been named.
/// The scanner stays symlink-rejecting; the dereference happens HERE, once, before any scan, so
/// the folder that IS scanned holds nothing but real files.
///
/// Everything else is the identity. A folder whose root `SKILL.md` is a regular file is its own
/// origin, and so is a shape this rule does not recognize (links scattered over several folders,
/// a link to something that is not a sibling of a real `SKILL.md`) — returned unchanged, for the
/// scanner to refuse by name.
///
/// # Errors
/// [`ClientError::LinkedSource`] when the root `SKILL.md` link is broken or cyclic, or when the
/// shell also holds files of its own — then there are two folders, and only the person knows
/// which one they meant.
pub(crate) fn origin_dir(dir: &Path) -> Result<PathBuf, ClientError> {
    let here = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let marker = here.join(SKILL_FILE);
    let Ok(meta) = std::fs::symlink_metadata(&marker) else {
        return Ok(here); // no root SKILL.md — not this shape (and not this function's refusal)
    };
    if !meta.file_type().is_symlink() {
        return Ok(here);
    }
    let name = dir_basename(&here).unwrap_or_else(|| here.display().to_string());
    // `canonicalize` walks the whole chain, so a cycle and a missing target both land here.
    let Ok(target) = marker.canonicalize() else {
        return Err(ClientError::LinkedSource {
            name,
            reason: format!("its {SKILL_FILE} link is broken (target missing)"),
        });
    };
    let Some(parent) = target.parent() else {
        return Ok(here);
    };
    let Ok(read) = std::fs::read_dir(&here) else {
        return Ok(here);
    };
    let mut one_parent = true;
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(entry_meta) = std::fs::symlink_metadata(&path) else {
            return Ok(here);
        };
        let leaf = entry.file_name();
        // The two the scanner itself drops — a stray one of those is not "files of its own".
        if leaf == ".DS_Store" || leaf == ".git" {
            continue;
        }
        if !entry_meta.file_type().is_symlink() {
            return Err(ClientError::LinkedSource {
                name,
                reason: format!(
                    "{SKILL_FILE} links to a different folder ({}) but {} holds its own files \
                     too; add one folder by path",
                    parent.display(),
                    here.display()
                ),
            });
        }
        if path.canonicalize().ok().as_deref().and_then(Path::parent) != Some(parent) {
            one_parent = false;
        }
    }
    // The original has to be an ordinary skill folder: a REGULAR `SKILL.md`, which is what the
    // scan will read. A link pointing at anything else leaves the shell as it was.
    let regular_marker =
        std::fs::symlink_metadata(parent.join(SKILL_FILE)).is_ok_and(|m| m.file_type().is_file());
    if one_parent && regular_marker {
        return Ok(parent.to_path_buf());
    }
    Ok(here)
}

/// [`origin_dir`] for a reader that has nothing to refuse — a listing, which shows a shell it
/// cannot resolve exactly as it finds it.
pub(crate) fn origin_dir_or_self(dir: &Path) -> PathBuf {
    origin_dir(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Adopt the skill rooted at `source`, naming it from the source itself (a recognized harness dir's name,
/// else frontmatter-then-basename) — the direct-path entry point (a path-shaped positional).
///
/// # Errors
/// [`ClientError::SourceMissing`] when `source` is not on this machine;
/// [`ClientError::KindRequired`] for a server bundle (or a both-marker folder) named without `--kind`;
/// [`ClientError::SourceOverlap`] if `source` overlaps `~/.topos/`; [`ClientError::EmptyBundle`] /
/// [`ClientError::Scan`] from the scan; [`ClientError::SkillExists`] on an id collision; otherwise a
/// store/io failure.
pub(crate) fn add(ctx: &Ctx<'_>, source: &Path) -> Result<AddData, ClientError> {
    refuse_missing_source(ctx, source)?;
    refuse_unflagged_mcp_dir(ctx, &origin_dir(source)?)?;
    add_with_name(ctx, source, None, true, BundleKind::Skill)
}

/// The PATH arm of `add`, against the scope whose file will hold the row: adopt the directory at
/// `source`, or RE-LINK the record a dropped row left behind.
///
/// `remove <path>` edits the manifest and KEEPS the bytes, so a record outlives the row that asked
/// for it — and [`add`]'s already-tracked refusal then fired on a record nothing demanded any more,
/// which made the undo that removal prints (`topos add [-g] <path>`) a command that could not run.
/// So when a record for this folder exists and NO row in `target` claims it, the add re-links to
/// that record: the row is written exactly as a fresh adopt writes it, over the id, lock, history,
/// and draft snapshots already on disk — nothing is minted and no byte moves, so the state after
/// the undo is the state before the remove. A folder `target` STILL spells is refused exactly as
/// before: that add really is a second adoption of one mutable directory.
///
/// `declared` is whether the invocation carried an explicit `--kind` word: the server-bundle guard
/// arms on silence alone, so `--kind skill` adopts a `server.json`-rooted folder AS a skill — the
/// person said which, and that answer outranks the folder's shape.
///
/// # Errors
/// As [`add`], plus a `target` manifest that cannot be read or parsed.
pub(crate) fn adopt_path(
    ctx: &Ctx<'_>,
    scope: &super::manifest_edit::AddScope,
    source: &Path,
    declared: KindDeclared,
) -> Result<AddData, ClientError> {
    if declared == KindDeclared::No {
        // The guard asks about the folder that will actually be adopted — for a link shell, the
        // folder its links point into ([`origin_dir`]) — or a shell over a server bundle would
        // slip past it and land as a skill.
        let sctx = super::pull::ctx_with_layout(ctx, &scope.layout);
        refuse_unflagged_mcp_dir(&sctx, &origin_dir(source)?)?;
    }
    adopt_path_any_kind(ctx, scope, source, BundleKind::Skill)
}

/// [`adopt_path`] minus the server-bundle guard — `add --kind mcp`'s own local door, which adopts
/// exactly that shape deliberately (the flag IS the declaration the guard asks for).
///
/// `ctx` is the OUTER context and `scope` what the invocation resolved to write, exactly as the
/// claim door takes them: the record and the row belong to the scope, and the ownership question
/// spans both stores.
pub(crate) fn adopt_path_any_kind(
    ctx: &Ctx<'_>,
    scope: &super::manifest_edit::AddScope,
    source: &Path,
    kind: BundleKind,
) -> Result<AddData, ClientError> {
    let sctx = &super::pull::ctx_with_layout(ctx, &scope.layout);
    // REFUSAL-FIRST, before the re-link's own revive/stamp/log: a folder that is not there at all,
    // then a record standing for this folder under a DIFFERENT kind (neither is re-linkable).
    refuse_missing_source(sctx, source)?;
    // A link shell IS the folder its links point into, from here down: the kind guard, the
    // ownership probe, and the re-link all ask about a record keyed by the ORIGIN, which is what
    // the adopt records.
    let source = &origin_dir(source)?;
    // A FOLDER THE OTHER SCOPE ALREADY MANAGES is the two-engines state the claim door refuses,
    // met from the path door — including the re-link below, which would put this store's engine on
    // the folder just as an adopt would. Asked only when THIS store has nothing for the folder: a
    // record here is this scope's own business, and its own answer (the re-link, or the
    // already-tracked refusal) is the one the person needs.
    if tracked_skill_at(sctx, source)?.is_none() {
        refuse_other_scope(ctx, scope, source, None, kind)?;
    }
    refuse_kind_change(sctx, source, kind)?;
    match unclaimed_record(sctx, &scope.target, source)? {
        Some(data) => {
            // A RE-LINKED record predates this door's marker when it was written by an older
            // build; stamping it here is the same first-write-wins belt the mint takes — and the
            // guard above has already proven no standing marker disagrees with `kind`.
            if let Some(Ok(sid)) = data.skill_id.as_deref().map(crate::id::SkillId::parse) {
                crate::bundle_kind::write_kind_marker(sctx, &sid, kind);
            }
            Ok(data)
        }
        None => add_with_name(sctx, source, None, true, kind),
    }
}

/// A record's KIND is decided once, when it is adopted, and everything downstream trusts the
/// marker that records it — including the publish gate, which reads it to know a bundle IS a
/// server and must have its document scanned for credentials. So a folder whose record already
/// stands under one kind can never be re-linked under another: the marker is neither overwritten
/// (it is the durable fact) nor left to disagree with the row (that is how a server document would
/// reach the wire as a skill, gate unrun). The way through is to drop the record and add the
/// folder again.
///
/// Silent when nothing stands for the folder, when it stands with no marker at all (the mint's
/// first write then records `kind`), or when the standing marker already agrees. A marker naming
/// a kind THIS BUILD does not own disagrees with every request, and refuses.
///
/// # Errors
/// [`ClientError::InvalidArgument`] naming both kinds and the command that frees the folder.
fn refuse_kind_change(
    ctx: &Ctx<'_>,
    source: &Path,
    requested: BundleKind,
) -> Result<(), ClientError> {
    let Ok(dir) = source.canonicalize() else {
        return Ok(());
    };
    let Some(id) = tracked_skill_at(ctx, &dir)? else {
        return Ok(());
    };
    let Ok(sid) = crate::id::SkillId::parse(&id) else {
        return Ok(());
    };
    let Some(recorded) = crate::bundle_kind::kind_marker(ctx.fs, &ctx.layout, &sid) else {
        return Ok(());
    };
    if crate::bundle_kind::BundleKind::parse(&recorded) == Some(requested) {
        return Ok(());
    }
    let name = tracked_name(ctx, &id)
        .or_else(|| dir_basename(&dir))
        .unwrap_or_else(|| dir.display().to_string());
    Err(ClientError::InvalidArgument(format!(
        "'{name}' is already tracked here as a {recorded} bundle — a record's kind is fixed when \
         it is adopted; `topos remove {name}` drops it, and the folder can then be added as a {} \
         bundle",
        requested.as_str()
    )))
}

/// The add receipt for a retained record no row in `target` claims — `None` when there is nothing
/// to re-link (no record for this folder, a record with no lock, or a row still claiming it),
/// which leaves every refusal and every fresh adopt exactly where [`add`] puts it.
fn unclaimed_record(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    source: &Path,
) -> Result<Option<AddData>, ClientError> {
    // A source that does not resolve is the adopt's error to raise — one refusal, in one place.
    let Ok(source_abs) = source.canonicalize() else {
        return Ok(None);
    };
    let Some(id) = tracked_skill_at(ctx, &source_abs)? else {
        return Ok(None);
    };
    if super::manifest_edit::path_row_claims(ctx, target, &source_abs)? {
        return Ok(None);
    }
    let skill_id = crate::id::SkillId::parse(&id)?;
    let sp = ctx.layout.published(&skill_id);
    // The lock is the record's commit marker AND everything the receipt reports: without it there
    // is nothing to re-link to, and the ordinary adopt owns the answer.
    let Some(lock) = doc::read_doc::<Lock>(ctx.fs, &sp.lock)? else {
        return Ok(None);
    };
    // Naming the folder to `add` is the explicit re-demand: a RETIRED record (released by the
    // one-time orphan resolution) returns to every surface, history and drafts intact.
    crate::sidecar::revive_record(ctx.fs, &ctx.layout, &skill_id);
    // Re-stamp the never-deletable source marker on the dir's slot. A record written before the
    // marker existed lacks it, and this is the one moment the dir is PROVEN to be the user's own
    // adopted source — they just named it to `add`. Under the skill lock, read-modify-write.
    {
        let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, &skill_id)?;
        let mut map = doc::read_map(ctx.fs, &sp.map)?;
        if let Some(m) = map.as_mut() {
            let mut stamped = false;
            for (dir, st) in m.placements.iter().zip(m.placement_state.iter_mut()) {
                if Path::new(dir) == source_abs && !st.adopted_source {
                    st.adopted_source = true;
                    stamped = true;
                }
            }
            if stamped {
                doc::write_map(ctx.fs, &sp.map, m)?;
            }
        }
    }

    logfile::append_event(
        ctx.fs,
        &ctx.layout.log_path(),
        &serde_json::json!({
            "action": "add",
            "skill_id": skill_id.as_str(),
            "name": lock.name,
            "version_id": lock.base_commit,
            "at": ctx.clock.now_unix_millis(),
        }),
    )?;

    Ok(Some(AddData {
        skill_id: Some(skill_id.into_string()),
        name: lock.name.clone(),
        version_id: Some(lock.base_commit),
        bundle_digest: Some(lock.bundle_digest),
        tracked: true,
        // Re-armed exactly as an adopt arms it: the folder is demanded again, and the harness
        // config may have been rewritten while it was not. Idempotent, and the signal the
        // composition root's breadth sweep + built-in placement ride.
        currency: recognize(ctx, &source_abs).map(|_| ctx.triggers.active().install()),
        triggers: Vec::new(),
        origin: None,
        // All three set by the manifest-edit step at the composition root, exactly as on a
        // fresh adopt.
        source: None,
        manifest: None,
        scope: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        published_match: None,
        mcp: None,
        dest: Vec::new(),
        dest_resolved: Vec::new(),
        dest_change: None,
        claim: None,
        unchanged: false,
        machine_copy: None,
        set_delivery: None,
        display: None,
        // The receipt would otherwise read as a fresh adopt while carrying a version older than
        // this run: say what actually happened.
        note: Some(format!(
            "'{}' still had a record here from an earlier add — the row points back at it, at the \
             version it left, with its history and any local edits intact",
            lock.name
        )),
    }))
}

/// Adopt the skill rooted at `source`. `name_override` (set by a name-resolved `add <skill>`) forces the
/// tracked name to the discovered name the user typed, so it stays consistent with `list`/`publish`/`diff`
/// even for a registry harness the active adapter does not recognize (whose bytes would otherwise be named
/// from `SKILL.md` frontmatter). `None` keeps the source-derived name (a direct path-shaped adopt).
///
/// # Errors
/// [`ClientError::SourceOverlap`] if `source` overlaps `~/.topos/`; [`ClientError::EmptyBundle`] /
/// [`ClientError::Scan`] from the scan; [`ClientError::SkillExists`] on an id collision; otherwise a
/// store/io failure.
pub(crate) fn add_with_name(
    ctx: &Ctx<'_>,
    source: &Path,
    name_override: Option<&str>,
    adopted_in_place: bool,
    kind: BundleKind,
) -> Result<AddData, ClientError> {
    // Establish the home, then refuse a source that overlaps it (canonicalized — catches symlinks), so
    // uninstall can never delete user bytes and the footprint oracle never collapses.
    ctx.fs.create_dir_all(ctx.layout.home())?;
    // The ONE adoption core, so the record always keys the folder the bytes really live in —
    // whichever door called it.
    let source = &origin_dir(source)?;
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
    // auto-update applies to it. A plain/unrecognized dir is tracked in place with no harness association.
    let recognized = recognize(ctx, &source_abs);

    // Mint identity. A recognized harness skill is keyed by its DIRECTORY name (the command name the
    // harness invokes); a plain dir keeps the frontmatter-first-then-basename order. The minted id is
    // parsed through the validated newtype like any other (the id source mints `topos_<hex>`, which
    // always fits — the parse is the type-level proof the path joins below demand).
    let skill_id = crate::id::SkillId::parse(&ctx.ids.new_skill_id())?;
    // A name-resolved `add <skill>` forces the discovered name (what `list` showed, what `publish`/`diff`
    // will resolve) — so a folder the active adapter does not recognize never tracks the bytes under a
    // divergent frontmatter name. Absent an override: a recognized harness skill is keyed by its DIRECTORY name (the
    // command name the harness invokes); a plain dir keeps the frontmatter-first-then-basename order.
    let name = match name_override {
        Some(n) => n.to_owned(),
        None => match &recognized {
            Some(placement) => {
                dir_basename(&placement.path).unwrap_or_else(|| skill_id.to_string())
            }
            None => bundle
                .name_hint
                .clone()
                .or_else(|| dir_basename(&source_abs))
                .unwrap_or_else(|| skill_id.to_string()),
        },
    };

    // The dirs topos keeps for itself are reserved end-to-end — an adopted skill can never take
    // one (the sidecar, `list`, `publish`, and the placement dirs would all collide).
    if let Some(refusal) = reserved_name_refusal(&name, skill_id.as_str()) {
        return Err(ClientError::InvalidArgument(refusal));
    }

    // version_id depends ONLY on the bytes + device id + the fixed message — never the id/time/RNG — so a
    // fixed fixture pins it while ids stay free.
    let version_id = identity::commit_id(&Commit {
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
            draft_observed: None,
        },
    )?;
    // Which agent this placement SERVES. Either the active adapter recognized the dir as its own
    // (adopt-in-place; auto-update armed below), or the folder is read by exactly ONE installed
    // agent, which makes it that agent's dir. A folder several agents read has no single owner and a
    // folder none reads has none at all — both stay `None`, the agent-less shape the placement engine
    // already treats as the user's own chosen location.
    let harness_slug = match &recognized {
        Some(_) => Some(ctx.harness.id().slug().to_owned()),
        None => match folder_readers(&source_abs).as_slice() {
            [sole] => Some(sole.clone()),
            _ => None,
        },
    };

    // Record the placement: the harness skill dir for a recognized skill (the path the harness reads),
    // else the canonical source. Topos writes NOTHING into this dir — it stays byte-identical.
    let (placement, harness) = match &recognized {
        Some(p) => (
            p.path.to_string_lossy().into_owned(),
            Some(ctx.harness.id()),
        ),
        None => (source_abs.to_string_lossy().into_owned(), None),
    };
    doc::write_map(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: vec![placement],
            applied_commit: version_hex.clone(),
            materialized_sha: digest_hex.clone(),
            // The adopted source dir is the ONE native placement (the adopted bytes ARE what topos
            // recorded, so the per-placement sha starts at the adopted digest — that baseline is
            // for DRIFT detection only). `adopted_source` marks a dir the USER owns — an
            // in-place adopt of a directory topos never created; the undemanded cleans retire
            // every placement topos wrote, but never that one. A REMOTE import materializes its
            // dir itself and passes `false`: those bytes are topos's to retire like any other.
            placement_state: vec![topos_types::persisted::PlacementState {
                kind: topos_types::persisted::PlacementKind::Native,
                agent: harness_slug.clone(),
                materialized_sha: Some(digest_hex.clone()),
                pre_existing_sha: None,
                swap_capability: SwapCapability::Unsupported,
                adopted_source: adopted_in_place,
                claim: None,
            }],
            harness,
            harness_slug,
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

    // The durable kind marker — an adopt IS a bundle's first sync, and every later verb reads this
    // rather than re-deriving the kind from state a sweep may drop.
    crate::bundle_kind::write_kind_marker(ctx, &skill_id, kind);

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

    // Arm auto-update for a recognized harness — a best-effort, idempotent edit of the harness CONFIG
    // (never the skill dir), AFTER the all-or-nothing adoption above, so a settings.json hiccup never
    // rolls back a good adoption. Disclosed in the result (the only write `add` makes outside ~/.topos/).
    let currency = recognized.as_ref().map(|_| ctx.triggers.active().install());

    Ok(AddData {
        skill_id: Some(skill_id.into_string()),
        name,
        version_id: Some(version_hex),
        bundle_digest: Some(digest_hex),
        tracked: true,
        currency,
        triggers: Vec::new(),
        // Set by the remote-import wrapper ([`add_remote`]); a local adopt has no upstream.
        origin: None,
        // All three set by the manifest-edit step at the composition root (the verb records the
        // demand line, and the row's key decides how the receipt spells the source).
        source: None,
        manifest: None,
        scope: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        // Set by the bare-NAME arm at the composition root when a connected workspace publishes
        // the same name (the team-managed spelling, disclosed beside the local copy).
        published_match: None,
        // Set by the manifest half when the edit was not the plain row write (a file born here, a
        // redundant row withheld, an `off` switch deleted, a standing web decline).
        note: None,
        // Set by `add --kind mcp`'s receipt fold; a plain adopt is not a server.
        mcp: None,
        // Set by the `-a`/`--dest` arms at the composition root (the row's frozen destinations).
        dest: Vec::new(),
        dest_resolved: Vec::new(),
        dest_change: None,
        claim: None,
        unchanged: false,
        machine_copy: None,
        set_delivery: None,
        display: None,
    })
}

/// The options a remote `add <owner/repo>` carries beyond the source spec.
#[derive(Debug, Clone, Default)]
pub(crate) struct AddRemoteOpts {
    /// Pick one skill from a multi-skill repo (else: a lone skill is taken; several is a typed error).
    pub skill: Option<String>,
    /// Land into this harness's skills dir (a registry slug); `None` = the active harness.
    pub harness: Option<String>,
    /// A LITERAL destination root the copy lands under (a manifest row's resolved `dest` entry)
    /// — wins over `harness`; the project containment proof still runs on it.
    pub dest_root: Option<std::path::PathBuf>,
    /// Land in the harness's user/global skills dir instead of the project (cwd) dir.
    pub global: bool,
}

/// The persisted remote-import provenance (`skills/<id>/origin.json`) — a best-effort adjunct written
/// AFTER adoption, so its absence just means "no recorded upstream." Carries a `schema_version` for the
/// fail-closed read dispatch a later re-sync will use.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginDoc {
    pub schema_version: u32,
    #[serde(flatten)]
    pub origin: SkillOrigin,
    /// When the import happened (epoch millis).
    pub imported_at: u64,
    /// Every skill the archive held AT `origin.commit` — the member set this import saw. Additive
    /// (an older document simply has none), and the whole point of recording it is that a PINNED
    /// repo-set row can tell "all of them landed" from "some of them landed": without it, a batch
    /// that installed two of five members reads as settled forever, because every member it DID
    /// land sits at the pin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// Adopt a skill fetched from a REMOTE source (`owner/repo`, a github.com URL). Resolves the destination
/// harness dir, refuses to clobber it, materializes the byte-exact skill there, then adopts it through the
/// unchanged [`add_with_name`] core and records the provenance.
///
/// This is the byte-placing PRIMITIVE, not a verb: it asks nothing. The describe → `--yes`
/// ceremony lives one layer up, in [`super::reference`], which every user-facing forge arm — bare
/// reference and `-s`/`-a` selector alike — goes through before reaching here.
///
/// # Errors
/// The remote-import family ([`ClientError::RemoteFetch`] / [`ClientError::NoSkillInSource`] /
/// [`ClientError::SkillNotInRepo`] / [`ClientError::AmbiguousSkillInRepo`] /
/// [`ClientError::PlacementOccupied`] / [`ClientError::HarnessNotFound`]), plus any adoption error.
pub(crate) fn add_remote(
    ctx: &Ctx<'_>,
    source: &dyn GitTarballSource,
    spec: &RemoteSpec,
    roots: &super::DiscoveryRoots,
    opts: &AddRemoteOpts,
) -> Result<AddData, ClientError> {
    let targz = source.fetch(spec)?;
    add_remote_fetched(ctx, &targz, spec, roots, opts)
}

/// [`add_remote`] over an ALREADY-FETCHED tarball — the seam the pin-refresh path uses so the
/// network round-trip (and its failure modes) happen BEFORE any old bytes are deleted.
pub(crate) fn add_remote_fetched(
    ctx: &Ctx<'_>,
    targz: &[u8],
    spec: &RemoteSpec,
    roots: &super::DiscoveryRoots,
    opts: &AddRemoteOpts,
) -> Result<AddData, ClientError> {
    ctx.fs.create_dir_all(ctx.layout.home())?;

    // 1. Destination root + scope. A LITERAL `dest_root` (a manifest row's resolved dest entry)
    //    wins whole; else the named harness's skills dir (default: the active harness — the one
    //    topos drives + can arm auto-updates for). An explicit `--harness` must name a known
    //    registry slug.
    let scope = if opts.global {
        SkillScope::User
    } else {
        SkillScope::Project
    };
    let dest_root = match &opts.dest_root {
        Some(root) => root.clone(),
        None => {
            let slug = opts
                .harness
                .clone()
                .unwrap_or_else(|| ctx.harness.id().slug().to_owned());
            if !topos_harness::registry::known_harnesses()
                .iter()
                .any(|h| h.slug == slug)
            {
                return Err(ClientError::HarnessNotFound(format!(
                    "unknown harness '{slug}' — omit `--harness` to use the default, or run `topos list` to see \
                     the harness slugs"
                )));
            }
            topos_harness::registry::skills_root(&slug, scope, &roots.home, roots.cwd.as_deref())
                .ok_or_else(|| ClientError::InvalidArgument(destination_hint(&slug, opts.global)))?
        }
    };
    // PROJECT containment, proven at the WRITE boundary (and re-proven immediately before the
    // landing rename below): the registry hands back a path, but a pre-existing `.claude/skills`
    // symlink aims that path exactly like a committed `path = "../.."` override — the same rail
    // every project placement passes runs here, before the first byte lands, and the import is
    // REFUSED, never redirected.
    prove_import_containment(scope, roots, &dest_root)?;

    // 2. Extract + select the skill (all typed; a multi-skill repo self-corrects via `--skill`).
    let source_label = spec.label();
    let repo = extract_tree(targz)?;
    let selected = repo.select(
        spec.subdir.as_deref(),
        opts.skill.as_deref(),
        &spec.repo,
        &source_label,
    )?;

    // 3. Destination dir — refuse to clobber a foreign non-empty dir (no silent overwrites).
    let dest_dir = dest_root.join(&selected.name);
    check_destination(ctx, &dest_dir)?;

    // 4. Materialize the byte-exact skill (the one place remote bytes land outside ~/.topos). Write into a
    //    `.`-prefixed staging sibling (discovery ignores it) and rename it into place, so a crash or a
    //    mid-write I/O error never leaves a PARTIAL skill at the real path — only a stray staging dir the
    //    next run clears. On a failed adopt we remove the (now-complete) dest so the tree is left clean.
    //    A PROJECT-scope root is created with the NO-FOLLOW walk under the proven checkout: `mkdir -p`
    //    through an ancestor swapped since the proof is refused, never followed.
    match (scope, roots.cwd.as_deref()) {
        (SkillScope::Project, Some(cwd)) => {
            crate::materialize::create_dir_contained(ctx.fs, cwd, &dest_root)
                .map_err(|e| ClientError::Io(format!("create import root: {e}")))?;
        }
        _ => ctx.fs.create_dir_all(&dest_root)?,
    }
    // The PROOF-TO-WRITE anchor (see `materialize`'s module doc): the proven destination root is
    // pinned OPEN here, and the predictable-stage deletion + the landing rename below run at this
    // handle — `(dev, ino)`-verified against the path immediately before each op — so an ancestor
    // swapped for a symlink after the proof is met as itself and refused. A PERSON-scope root that
    // IS a symlink (a dotfiles setup — there is no containment contract to defend in home dirs)
    // resolves once and pins the real directory, matching the materializer's symlink-placement
    // support; a project root gets no such rung (its proof already refused symlink components).
    let dest_handle = match ctx.fs.open_dir_handle(&dest_root) {
        Ok(h) => h,
        Err(e) if scope != SkillScope::Project => {
            let real = std::fs::canonicalize(&dest_root)
                .map_err(|_| ClientError::Io(format!("open import root: {e}")))?;
            ctx.fs
                .open_dir_handle(&real)
                .map_err(|e| ClientError::Io(format!("open import root: {e}")))?
        }
        Err(e) => return Err(ClientError::Io(format!("open import root: {e}"))),
    };
    let stage_leaf = format!(".topos-import-{}", selected.name);
    // The staging path is spelled from the HANDLE's own path, not `dest_root`: a symlinked
    // person-scope root resolved to its canonical spelling above, and the fd-walk below anchors
    // its lexical containment check on the handle's path — the symlink spelling would fail it.
    let stage_dir = dest_handle.path().join(&stage_leaf);
    if ctx.fs.exists(&stage_dir) {
        // The clear runs AT the held root handle (identity-verified inside the op) — a swapped
        // ancestor cannot re-aim the recursive removal.
        ctx.fs
            .remove_dir_all_at(&dest_handle, &stage_leaf)
            .map_err(|e| ClientError::Io(format!("clear import staging: {e}")))?;
    }
    // The staging dir itself: one fd-walked (`openat`/`mkdirat`) level below the HELD root
    // handle — the root's pathname is not re-resolved — and its HELD handle carries the file
    // walk and the self-ignore write below, plus the landing's source-identity proof.
    let stage_handle = ctx
        .fs
        .create_dir_nofollow_at(&dest_handle, &stage_dir)
        .map_err(|e| ClientError::Io(format!("create import staging: {e}")))?;
    if let Err(e) = write_skill_dir(ctx, &stage_handle, &selected.files) {
        let _ = ctx.fs.remove_dir_all_at(&dest_handle, &stage_leaf);
        return Err(e);
    }
    // A PROJECT-scope import self-ignores exactly like the materializer's staged trees: the
    // exact sentinel unless the bundle ships its own root ignore file (content, never
    // overlaid). A shipped root ignore that does NOT self-ignore leaves the placement visible
    // to git — disclosed on the receipt below, never edited.
    let shipped_root_ignore = selected
        .files
        .iter()
        .find(|f| f.path == crate::scan::IGNORE_FILE);
    let mut git_visible = false;
    if scope == SkillScope::Project {
        match shipped_root_ignore {
            None => {
                // AT the staging dir's held handle — no path-based write below the proven root.
                if let Err(e) = ctx.fs.write_staged_at(
                    &stage_handle,
                    crate::scan::IGNORE_FILE,
                    crate::scan::IGNORE_SENTINEL,
                    false,
                ) {
                    let _ = ctx.fs.remove_dir_all_at(&dest_handle, &stage_leaf);
                    return Err(ClientError::Io(format!("stage self-ignore: {e}")));
                }
            }
            Some(f) => git_visible = !crate::materialize::ignores_all(&f.bytes),
        }
    }
    // PARK, THEN VERIFY. Step 3's check ran BEFORE the staging window, and building the staged
    // tree takes real time — anything that appeared at `dest_dir` since then was validated by
    // nobody. Re-checking and then deleting only narrows the window; moving the directory aside
    // first CLOSES it: after the rename nothing can land in the tree we are about to judge, and
    // the judgment therefore covers exactly the bytes at stake. A disposable park is NOT dropped
    // here — it survives, journaled, until the WHOLE adopt succeeds (settled below), so any later
    // failure still has the prior occupant to put back.
    let dest_park = match park_and_verify_destination(ctx, &dest_dir, &stage_dir) {
        Ok(p) => p,
        Err(e) => {
            let _ = ctx.fs.remove_dir_all_at(&dest_handle, &stage_leaf);
            return Err(e);
        }
    };
    // What THIS run staged, by digest — the only content the failed-adopt cleanup below may ever
    // account a destination tree against.
    let staged_digest = scan::scan(&stage_dir).ok().map(|s| s.bundle_digest);
    // The containment proof AGAIN, immediately before the rename that lands the bytes — the
    // extract + staging took real time, and an ancestor turned symlink inside that window would
    // route the rename through it. (The rename itself then runs at the held root handle, so even
    // a swap in the beat after this proof cannot re-aim it.)
    if let Err(e) = prove_import_containment(scope, roots, &dest_dir) {
        let _ = ctx.fs.remove_dir_all_at(&dest_handle, &stage_leaf);
        if let Some(park) = &dest_park {
            restore_import_park(ctx, park, &dest_dir);
        }
        return Err(e);
    }
    let dest_leaf = match dest_dir.file_name().and_then(|n| n.to_str()) {
        Some(d) => d,
        None => {
            let _ = ctx.fs.remove_dir_all_at(&dest_handle, &stage_leaf);
            if let Some(park) = &dest_park {
                restore_import_park(ctx, park, &dest_dir);
            }
            return Err(ClientError::Io(format!(
                "place import at {}: no usable final component",
                dest_dir.display()
            )));
        }
    };
    // `_src`: immediately before the rename, the stage leaf is re-opened from the held root fd
    // and proven identical to the staging handle this run built under — a completed stage moved
    // aside and substituted at its predictable name in the final beat refuses, never lands.
    if let Err(e) =
        ctx.fs
            .rename_at_noreplace_src(&dest_handle, &stage_leaf, dest_leaf, &stage_handle)
    {
        // The staging removal runs AT the held handle (identity-verified inside the op — a
        // failing verify preserves the tree, exactly as the old explicit guard did).
        let _ = ctx.fs.remove_dir_all_at(&dest_handle, &stage_leaf);
        if let Some(park) = &dest_park {
            restore_import_park(ctx, park, &dest_dir);
        }
        return Err(ClientError::Io(format!(
            "place import at {}: {e}",
            dest_dir.display()
        )));
    }
    let mut data = match add_with_name(
        ctx,
        &dest_dir,
        Some(&selected.name),
        false,
        BundleKind::Skill,
    ) {
        Ok(d) => d,
        Err(e) => {
            // The staged tree is LIVE at `dest_dir` now, and an edit can land there the instant
            // the rename completes — so the error path never deletes the destination blind: it
            // parks the dir (journaled), drops it only when it still holds exactly what this run
            // staged, and preserves anything else for recovery to restore. Then the PRE-LANDING
            // park goes back where it was: in the concurrent-identical-import case this is the
            // winner's landed copy, and restoring it is what keeps the winner's map pointing at
            // a present directory.
            preserve_failed_adopt(ctx, &dest_dir, staged_digest);
            if let Some(park) = &dest_park {
                restore_import_park(ctx, park, &dest_dir);
            }
            return Err(e);
        }
    };

    // 5. Record provenance — a best-effort adjunct, never allowed to fail a good adoption (mirrors the
    //    auto-update hook being armed after the atomic adopt).
    let origin = SkillOrigin {
        source: spec.origin(),
        git_ref: spec.git_ref.clone(),
        commit: repo.commit.clone(),
        subdir: selected.subdir.clone(),
        license: selected.license.clone(),
    };
    if let Some(Ok(id)) = data.skill_id.as_deref().map(crate::id::SkillId::parse) {
        let doc = OriginDoc {
            schema_version: PERSISTED_SCHEMA_VERSION,
            origin: origin.clone(),
            imported_at: ctx.clock.now_unix_millis(),
            // The whole archive's member set at this commit (the same enumeration the reconcile's
            // repo-set arm compares against), not just the member this call selected.
            members: repo.skill_names(None, &spec.repo),
        };
        if let Err(e) = doc::write_doc(ctx.fs, &ctx.layout.published(&id).origin, &doc) {
            let _ = logfile::append_event(
                ctx.fs,
                &ctx.layout.log_path(),
                &serde_json::json!({
                    "action": "add_origin_warning",
                    "skill_id": id.as_str(),
                    "warning": e.detail(),
                }),
            );
        }
    }
    data.origin = Some(origin);
    if git_visible {
        data.note = Some(format!(
            "the bundle ships its own root .gitignore, which does not ignore the placement — \
             {} is visible to git; commit or ignore it deliberately",
            dest_dir.display()
        ));
    }
    // The WHOLE adopt succeeded — only now is the pre-landing park concluded (re-judged at the
    // drop; preserved instead if it no longer reads disposable).
    if let Some(park) = &dest_park {
        settle_import_park(ctx, park, staged_digest);
    }
    Ok(data)
}

/// The two-phase outcome of an `add <name>` that RE-FORKS a retained withdrawn/detached copy.
#[derive(Debug)]
pub(crate) enum KeepAsYoursOutcome {
    /// Bare `add <name>` — the local-fork preview (nothing changed) + the `--yes` argv.
    Described {
        data: KeepAsYoursData,
        yes_argv: Vec<String>,
    },
    /// `add <name> --yes` — the re-forked new local skill (boxed: `AddData` dwarfs the describe variant).
    Forked(Box<AddData>),
}

/// If `<name>` resolves to a RETAINED (withdrawn-upstream / detached / removed-here) tracked skill — the
/// state `withdraw_upstream` / `freeze_detached` / a `remove` exclusion leave, where the sidecar keeps the
/// bytes (+ any draft snapshot) but the follow entry no longer delivers here — re-adopt it as a **new
/// local skill** with no upstream ("keep it as yours"): the team copy stays archived/detached, the local
/// draft rides along, and the old (ghost) follow entry + sidecar are retired so `list` stops showing it.
///
/// Returns `Ok(None)` for a name that is NOT a retained tracked copy (a LIVE tracked skill, a purely-local
/// skill, or an untracked name) — the caller falls through to the ordinary `add <name>` adopt, which keeps
/// the `ALREADY_TRACKED` / discovery behavior.
///
/// # Errors
/// A store / io failure; a resolve error other than not-found/ambiguous (which fall through as `Ok(None)`).
pub(crate) fn keep_as_yours(
    ctx: &Ctx<'_>,
    name: &str,
    yes: bool,
) -> Result<Option<KeepAsYoursOutcome>, ClientError> {
    // Resolve to a tracked skill; a miss / ambiguity is not a retained copy — let the normal path answer.
    let (sid, lock) = match super::resolve_skill(ctx, name) {
        Ok(v) => v,
        Err(ClientError::NoSuchSkill { .. } | ClientError::AmbiguousName { .. }) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    // Only a DELIVERED skill can be withdrawn/detached — the offline delivery cache is the
    // session-model record; a purely-local (genesis) skill is not a fork case.
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    let Some((entry_ws, entry)) = cache.workspaces.iter().find_map(|(ws, e)| {
        e.delivered
            .get(sid.as_str())
            .map(|d| (ws.clone(), d.clone()))
    }) else {
        return Ok(None);
    };
    // Placement state: a withdrawal / exclusion CLEANED the agent dirs; a detach (unfollow) LEFT them.
    let sp = ctx.layout.published(&sid);
    let placements: Vec<String> = doc::read_map(ctx.fs, &sp.map)?
        .map(|m| m.placements)
        .unwrap_or_default();
    let present = placements.iter().any(|p| ctx.fs.exists(Path::new(p)));

    // The retained reason — or NOT a fork case (a live delivered skill stays the ordinary
    // already-tracked answer; a WITHDRAWN cache row or a cleaned placement is the retained state).
    let reason = if entry.withdrawn || !present {
        KeepReason::WithdrawnUpstream
    } else {
        return Ok(None);
    };

    let Some(dest) = placements.first().cloned() else {
        return Err(ClientError::InvalidArgument(format!(
            "'{name}' has no recorded placement to re-fork — adopt it by path instead"
        )));
    };
    let dest = PathBuf::from(dest);
    let has_draft = fork_has_draft(ctx, &sp, &lock, present)?;

    if !yes {
        return Ok(Some(KeepAsYoursOutcome::Described {
            data: KeepAsYoursData {
                name: name.to_owned(),
                workspace_id: Some(entry_ws.clone()),
                reason,
                has_draft,
            },
            yes_argv: vec![
                "topos".to_owned(),
                "add".to_owned(),
                name.to_owned(),
                "--yes".to_owned(),
            ],
        }));
    }

    // ---- APPLY (`--yes`) ----
    // 1. When the dirs were cleaned (withdrawn / removed here), re-render the retained tree (the draft
    //    snapshot if one rides along, else the base) back into the former placement path. A DETACHED copy
    //    already has its bytes on disk, so it is adopted in place.
    if !present {
        render_retained_into(ctx, &sp, &lock, &dest)?;
    }
    // 2. Retire the old sidecar record + follow entry BEFORE re-adopting `dest` — else `add`'s
    //    already-tracked guard would refuse the still-tracked path.
    retire_tracked(ctx, &sid)?;
    // 3. Adopt `dest` fresh: a new local skill id, named `<name>`, with NO upstream.
    let data = add_with_name(ctx, &dest, Some(name), false, BundleKind::Skill)?;
    Ok(Some(KeepAsYoursOutcome::Forked(Box::new(data))))
}

/// Whether a local draft rides along into the fork: for a present (detached) copy, the live bytes differ
/// from the base digest; for a cleaned copy, a draft snapshot was retained in the store at withdrawal.
fn fork_has_draft(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    present: bool,
) -> Result<bool, ClientError> {
    if present {
        let Some(placement) =
            doc::read_map(ctx.fs, &sp.map)?.and_then(|m| m.placements.into_iter().next())
        else {
            return Ok(false);
        };
        let src = Path::new(&placement);
        if !src.exists() {
            return Ok(false);
        }
        return Ok(match scan::scan(src) {
            Ok(b) => to_hex(&b.bundle_digest) != lock.bundle_digest,
            Err(_) => false,
        });
    }
    let store = Store::open(&sp.store)?;
    let base = super::parse_hex32(&lock.base_commit)?;
    // Base ⇒ no draft; Draft or Ambiguous ⇒ a draft rides along (Ambiguous is refused at apply, but the
    // describe must still say a draft exists so the user is not told "nothing rides along" and then
    // blocked).
    Ok(!matches!(retained_head(&store, base)?, RetainedTree::Base))
}

/// Render the retained tree (the draft snapshot if one rides along, else the base) back into `dest` — the
/// bytes a withdrawal/exclusion cleaned off disk but kept in the sidecar store. The dest is cleared then
/// recreated, so a stray remnant can never shadow the fork.
fn render_retained_into(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    dest: &Path,
) -> Result<(), ClientError> {
    let store = Store::open(&sp.store)?;
    let base = super::parse_hex32(&lock.base_commit)?;
    let head = match retained_head(&store, base)? {
        RetainedTree::Base => base,
        RetainedTree::Draft(d) => d,
        // FAIL CLOSED: more than one draft snapshot sits on the base and we cannot know which is the
        // one to keep. Rendering only the base and then retiring the sidecar (the next step) would
        // permanently destroy the other drafts — a silent loss the "nothing is ever lost" contract
        // forbids. Refuse instead, touching nothing: the bytes stay retained for the user to inspect.
        RetainedTree::Ambiguous => {
            return Err(ClientError::Corrupt(
                "this retained copy has more than one saved draft and cannot be re-forked \
                 automatically — nothing was changed; inspect the copies under ~/.topos and adopt \
                 the one you want by path (topos add <path>)"
                    .to_owned(),
            ));
        }
    };
    let leaves = store
        .read_tree_structure(head)
        .map_err(|e| ClientError::Corrupt(format!("read retained tree: {e:?}")))?;
    if ctx.fs.exists(dest) {
        ctx.fs.remove_dir_all(dest)?;
    }
    ctx.fs.create_dir_all(dest)?;
    for leaf in &leaves {
        let (bytes, _sha) = store
            .read_git_blob_verified(leaf.git_oid)
            .map_err(|e| ClientError::Corrupt(format!("read retained blob: {e:?}")))?;
        let mut p = dest.to_path_buf();
        for comp in leaf.path.split('/') {
            p.push(comp);
        }
        if let Some(parent) = p.parent() {
            ctx.fs
                .create_dir_all(parent)
                .map_err(|e| ClientError::Io(format!("create {}: {e}", parent.display())))?;
        }
        let executable = matches!(leaf.mode, FileMode::Executable);
        ctx.fs.write_staged(&p, &bytes, executable)?;
    }
    Ok(())
}

/// What the sidecar retained for a withdrawn/excluded copy: just the base tree, exactly one draft
/// snapshot on it (what a withdrawal saves), or several (which one is current is unknowable — a crash
/// mid-snapshot or repeated withdraw/refollow cycles can leave more than one).
enum RetainedTree {
    /// No draft snapshot — the base bytes are the retained tree.
    Base,
    /// The one draft snapshot parented on the base.
    Draft([u8; 32]),
    /// More than one draft snapshot on the base — genuinely ambiguous; the fork must fail closed
    /// rather than pick one and delete the rest.
    Ambiguous,
}

/// Classify the retained tree at `base`. A single draft snapshot is the withdrawal's saved delta; more
/// than one is [`RetainedTree::Ambiguous`] (the caller refuses the fork so nothing is lost).
fn retained_head(store: &Store, base: [u8; 32]) -> Result<RetainedTree, ClientError> {
    let versions = store
        .list_versions()
        .map_err(|e| ClientError::Corrupt(format!("list retained versions: {e:?}")))?;
    let mut drafts: Vec<[u8; 32]> = Vec::new();
    for v in versions {
        if v == base {
            continue;
        }
        let Ok(node) = store.read_commit_meta(v) else {
            continue;
        };
        if node.parents.as_slice() == [base]
            && node.message == super::sync_engine::DRAFT_SNAPSHOT_MESSAGE
        {
            drafts.push(v);
        }
    }
    Ok(match drafts.as_slice() {
        [] => RetainedTree::Base,
        [one] => RetainedTree::Draft(*one),
        _ => RetainedTree::Ambiguous,
    })
}

/// Retire the retained skill's sidecar record + follow entry — the `keep-as-yours` fork re-adopts the
/// bytes as a new local skill, so the old (ghost) entry must go before the re-adopt (else the
/// already-tracked guard refuses the path) and so `list` stops showing a detached ghost afterward.
fn retire_tracked(ctx: &Ctx<'_>, sid: &SkillId) -> Result<(), ClientError> {
    let skill_dir = ctx.layout.skill_dir(sid);
    if ctx.fs.exists(&skill_dir) {
        ctx.fs.remove_dir_all(&skill_dir)?;
    }
    Ok(())
}

/// The PROJECT-scope containment proof a remote import's destination passes at the write
/// boundary — the same rail every project placement root passes
/// ([`crate::placement::within_project`]), run against the checkout the import lands in
/// (`roots.cwd`). Person-scope (`--global`) imports land in home dirs and carry no such rail.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] when the path does not provably resolve inside the
/// checkout (a symlink component, or an ancestor that canonicalizes elsewhere).
fn prove_import_containment(
    scope: SkillScope,
    roots: &super::DiscoveryRoots,
    path: &Path,
) -> Result<(), ClientError> {
    if scope != SkillScope::Project {
        return Ok(());
    }
    // A project-scope destination only resolves with a cwd (`skills_root` answered None without
    // one), so a missing cwd here cannot vouch for anything — fail closed.
    let contained = roots
        .cwd
        .as_deref()
        .is_some_and(|cwd| crate::materialize::contained_in(cwd, path));
    if contained {
        return Ok(());
    }
    Err(ClientError::PlacementUnsupported {
        reason: crate::placement::escape_line("the import destination", path),
    })
}

/// The failed-adopt cleanup that can no longer destroy a raced edit: the destination is PARKED
/// (journal first — a crash mid-cleanup must not strand it), read where nothing can still reach
/// it by path, and dropped ONLY when two consecutive reads agree it holds exactly what this run
/// staged. Anything else — an edit that landed after the rename, an unreadable tree, an unknown
/// staged digest — keeps the park: the journal entry stands, and the next run's recovery restores
/// the bytes to the destination (visible, untracked, never lost). Best-effort by contract: the
/// adopt's own error is what propagates.
fn preserve_failed_adopt(ctx: &Ctx<'_>, dest: &Path, staged_digest: Option<[u8; 32]>) {
    if !ctx.fs.exists(dest) {
        return;
    }
    let Ok(parked) = crate::materialize::park_aside_journaled(
        ctx.fs,
        &ctx.layout,
        dest,
        "import-failed",
        true,
        None,
    ) else {
        return; // could not park: the dir stays whole at the destination — preserved in place
    };
    let mut prev: Option<[u8; 32]> = None;
    let mut benign = false;
    for _ in 0..4 {
        let Ok(scanned) = scan::scan(&parked) else {
            break; // unreadable = not ours to delete
        };
        if staged_digest != Some(scanned.bundle_digest) {
            break; // novel bytes — keep the park whole
        }
        if prev == Some(scanned.bundle_digest) {
            benign = true; // two consecutive agreeing reads of exactly the staged bytes
            break;
        }
        prev = Some(scanned.bundle_digest);
    }
    if benign && ctx.fs.remove_dir_all(&parked).is_ok() {
        crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, &parked);
    } else {
        let _ = logfile::append_event(
            ctx.fs,
            &ctx.layout.log_path(),
            &serde_json::json!({
                "action": "import_failed_park",
                "destination": dest.to_string_lossy(),
                "park": parked.to_string_lossy(),
                "at": ctx.clock.now_unix_millis(),
            }),
        );
    }
}

/// Verbatim guidance when a destination harness has no dir for the chosen scope.
fn destination_hint(slug: &str, global: bool) -> String {
    if global {
        format!(
            "harness '{slug}' has no global skills directory — drop `--global` for project scope, or pick \
             a different `--harness`"
        )
    } else {
        format!(
            "no project directory to import into — run inside a project, or use `--global` to land in \
             '{slug}'s global skills dir"
        )
    }
}

/// Refuse to clobber the destination: an absent or EMPTY dir is fine to fill; a non-empty dir is either
/// already tracked (`ALREADY_TRACKED`, edit it in place) or a foreign dir (`PLACEMENT_OCCUPIED`).
fn check_destination(ctx: &Ctx<'_>, dest: &Path) -> Result<(), ClientError> {
    if !ctx.fs.exists(dest) {
        return Ok(());
    }
    let empty = ctx.fs.read_dir(dest).map(|v| v.is_empty()).unwrap_or(false);
    if empty {
        return Ok(());
    }
    if let Ok(canon) = dest.canonicalize()
        && let Some(skill_id) = tracked_skill_at(ctx, &canon)?
    {
        return Err(already_tracked(ctx, &skill_id, &canon));
    }
    Err(ClientError::PlacementOccupied {
        path: dest.display().to_string(),
    })
}

/// PARK the destination aside, then prove it was ours to replace — the second half of
/// [`check_destination`], run after the staging window rather than before it, and run on a tree
/// nothing can still be writing to.
///
/// TIME PASSES between the two checks: extracting and writing the staged tree is real work, and a
/// directory that appeared at `dest` in that window carries bytes nobody validated. Verifying and
/// then deleting leaves a window between the two; the rename here has none — after it, `dest` is
/// free for the caller's no-replace rename and the parked tree is a still photograph.
///
/// Two benign cases mark the park DROPPABLE: an EMPTY directory (what [`check_destination`]
/// already allowed), and content BYTE-IDENTICAL to what this call is about to place (a concurrent
/// run of the same import that landed first). Marking is ALL this judgment does — the park
/// (journaled) survives until the WHOLE adopt has succeeded, because a park dropped here has
/// nothing left to restore when a LATER step fails: the landing rename can refuse, and the adopt
/// itself can conclude `ALREADY_TRACKED` (exactly the concurrent-identical-import case, whose
/// winner's map must keep pointing at a present directory). The caller settles it on success
/// ([`settle_import_park`]) and restores it on any later failure ([`restore_import_park`]).
/// Everything else — a foreign occupant, a now-tracked dir, an unreadable one — is PUT BACK here
/// and the typed refusal stands: never delete bytes this run cannot account for. A destination
/// that has been re-created while the park was out keeps the parked bytes under the park's own
/// name, and the refusal says where.
///
/// The park is JOURNALED before the rename (one critical section — see
/// [`crate::materialize::park_aside_journaled`]), and the entry is settled only when the park is
/// finally dropped or restored: a crash anywhere in between leaves the journal to make the next
/// run's recovery restore (or preserve + disclose) the prior occupant.
///
/// Returns the park's path when a directory was parked (still on disk, journaled), `None` when
/// the destination was absent.
fn park_and_verify_destination(
    ctx: &Ctx<'_>,
    dest: &Path,
    staged: &Path,
) -> Result<Option<PathBuf>, ClientError> {
    if !ctx.fs.exists(dest) {
        return Ok(None);
    }
    let parked = crate::materialize::park_aside_journaled(
        ctx.fs,
        &ctx.layout,
        dest,
        "import-old",
        true,
        None,
    )?;
    // Judged with the settle rail: a rename takes the tree out of every path but not away from an
    // already-open fd, so the disposable verdict needs TWO CONSECUTIVE AGREEING READS of a
    // disposable state — empty, or byte-identical to what this call is about to place. Anything
    // else (including a tree that keeps moving) is put back whole.
    let staged_digest = scan::scan(staged).ok().map(|s| s.bundle_digest);
    if import_park_disposable(ctx, &parked, staged_digest) {
        return Ok(Some(parked));
    }
    let restored = crate::materialize::restore_parked(ctx.fs, &parked, dest);
    if restored {
        crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, &parked);
    }
    Err(ClientError::PlacementOccupied {
        path: if restored {
            dest.display().to_string()
        } else {
            format!(
                "{} (its bytes are parked at {})",
                dest.display(),
                parked.display()
            )
        },
    })
}

/// The settle-rail loop behind the import park's DISPOSABLE verdict: two consecutive agreeing
/// reads of an empty tree, or of bytes identical to `staged_digest`. Run once to mark the park
/// droppable and AGAIN immediately before the final drop (the park sat on disk across the whole
/// adopt, and a pre-park fd can have written into it in between).
fn import_park_disposable(ctx: &Ctx<'_>, parked: &Path, staged_digest: Option<[u8; 32]>) -> bool {
    let mut prev: Option<Option<[u8; 32]>> = None; // inner None = an empty directory
    for _ in 0..4 {
        let state: Option<[u8; 32]> = if ctx
            .fs
            .read_dir(parked)
            .map(|v| v.is_empty())
            .unwrap_or(false)
        {
            None
        } else {
            match scan::scan(parked) {
                Ok(s) => Some(s.bundle_digest),
                Err(_) => return false, // unreadable = not ours to delete
            }
        };
        let benign = match state {
            None => true,
            Some(d) => staged_digest == Some(d),
        };
        if !benign {
            return false;
        }
        if prev == Some(state) {
            return true;
        }
        prev = Some(state);
    }
    false
}

/// Conclude a droppable pre-landing park after the WHOLE adopt succeeded: re-judge it with the
/// same settle rail (time passed — a pre-park fd can have written into it during the adopt) and
/// drop + settle, or preserve it as a `.topos-kept-*` sibling (+ settle + log) when it no longer
/// judges disposable. Never a blind delete, never a lost entry.
fn settle_import_park(ctx: &Ctx<'_>, parked: &Path, staged_digest: Option<[u8; 32]>) {
    if !ctx.fs.exists(parked) {
        // Concluded by a concurrent recovery (restored or preserved elsewhere) — the journal
        // entry, if any survives, resolves harmlessly on the next sweep.
        crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, parked);
        return;
    }
    if import_park_disposable(ctx, parked, staged_digest) && ctx.fs.remove_dir_all(parked).is_ok() {
        crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, parked);
        return;
    }
    match crate::materialize::preserve_park(ctx.fs, parked, None) {
        Some(kept) => {
            crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, parked);
            let _ = logfile::append_event(
                ctx.fs,
                &ctx.layout.log_path(),
                &serde_json::json!({
                    "action": "park_preserved",
                    "park": parked.to_string_lossy(),
                    "kept_at": kept.to_string_lossy(),
                    "at": ctx.clock.now_unix_millis(),
                }),
            );
        }
        None => {
            // Stuck under its own name: leave the journal entry — recovery preserves + discloses.
        }
    }
}

/// Restore the pre-landing park after a LATER failure of the adopt (the landing refused, or the
/// adopt itself failed after landing and [`preserve_failed_adopt`] freed the destination): the
/// prior occupant goes back where it was. A destination that is still occupied (or a failing
/// rename) leaves the park journaled — the next run's recovery restores or preserves + discloses
/// it — and logs where the bytes sit.
fn restore_import_park(ctx: &Ctx<'_>, parked: &Path, dest: &Path) {
    if !ctx.fs.exists(parked) {
        crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, parked);
        return;
    }
    if crate::materialize::restore_parked(ctx.fs, parked, dest) {
        crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, parked);
    } else {
        let _ = logfile::append_event(
            ctx.fs,
            &ctx.layout.log_path(),
            &serde_json::json!({
                "action": "import_old_park",
                "destination": dest.to_string_lossy(),
                "park": parked.to_string_lossy(),
                "at": ctx.clock.now_unix_millis(),
            }),
        );
    }
}

/// Write a selected skill's byte-exact files into the staging tree under its HELD handle
/// `dest_h`, preserving the executable bit (part of the digest). Paths are archive-relative
/// forward-slash and already `..`/absolute-safe (extraction rejected hazards), and every nested
/// path is spelled from THE HANDLE'S OWN path (the fd-walk's lexical anchor — a caller-supplied
/// spelling could diverge from it through a symlinked root), so the component-wise join stays
/// inside the stage — the creates DESCEND FROM THE HELD STAGING HANDLE'S fd
/// ([`crate::fs_seam::FsOps::create_dir_nofollow_at`]: the stage's mutable pathname is never
/// re-resolved, so a stage swapped for an outward symlink after its creation cannot re-aim the
/// walk), and each file write runs AT the held handle its walk returned
/// ([`crate::fs_seam::FsOps::write_staged_at`]: openat-exclusive, fd-fsyncs), so a symlink
/// swapped into the staged tree mid-write is met as itself and refused, never traversed — no
/// path-based write below the proven staging root.
fn write_skill_dir(
    ctx: &Ctx<'_>,
    dest_h: &crate::fs_seam::DirHandle,
    files: &[RepoFile],
) -> Result<(), ClientError> {
    for f in files {
        let mut path = dest_h.path().to_path_buf();
        for comp in f.path.split('/') {
            path.push(comp);
        }
        let parent = path
            .parent()
            .ok_or_else(|| ClientError::Io(format!("{}: no parent directory", path.display())))?;
        let parent_handle = ctx
            .fs
            .create_dir_nofollow_at(dest_h, parent)
            .map_err(|e| ClientError::Io(format!("create {}: {e}", parent.display())))?;
        let leaf = f.path.rsplit('/').next().unwrap_or(&f.path);
        let executable = f.mode & 0o111 != 0;
        ctx.fs
            .write_staged_at(&parent_handle, leaf, &f.bytes, executable)?;
    }
    Ok(())
}

/// Resolve an `add <target>` positional to the concrete skill directory to adopt.
///
/// `target` is a skill NAME (`deploy`) or a harness-disambiguated name (`deploy@claude-code`). It resolves
/// against the SAME untracked inventory `topos list` discovers — the concrete, listable set of skills
/// sitting in known harness dirs. This is name *resolution*, never a fuzzy guess: a name matching more
/// than one placement is a hard typed error demanding `<skill>@<harness>`; a name that is already tracked
/// or looks like a path gets its own actionable error rather than a bare not-found.
///
/// Returns the resolved skill directory AND its resolved NAME (the discovered basename the user typed) —
/// the caller adopts the dir *under that name* so `list`/`add`/`publish`/`diff` all agree, even for a
/// harness the active adapter does not recognize (whose bytes would otherwise be named from frontmatter).
///
/// # Errors
/// The name-resolution family — [`ClientError::AmbiguousSource`] / [`ClientError::HarnessNotFound`] /
/// [`ClientError::NoUntrackedSkill`] / [`ClientError::AlreadyTrackedName`] / [`ClientError::PathNotName`]
/// — or a discovery read failure.
pub(crate) fn resolve_add_target(
    ctx: &Ctx<'_>,
    roots: &super::DiscoveryRoots,
    target: &str,
    verb: &str,
) -> Result<(std::path::PathBuf, String), ClientError> {
    let (name, harness) = split_target(target);
    // A residual guard: `crate::source` already routes path shapes to a direct adopt, but a `~`-prefixed
    // bare token can still arrive here — it is never a discovered skill NAME (those are bare basenames),
    // so refuse it with actionable guidance rather than a confusing not-found.
    if is_path_shaped(name) {
        return Err(ClientError::PathNotName {
            arg: target.to_owned(),
            verb: verb.to_owned(),
        });
    }
    let untracked = super::list::discover_untracked(ctx, roots)?;
    match resolve_name(name, harness, &untracked) {
        NameResolution::Resolved(path) => Ok((std::path::PathBuf::from(path), name.to_owned())),
        // No workspace candidates here: this entry point serves the verbs that resolve a LOCAL
        // directory (`publish`'s auto-add, `remove`'s path arm), where a team's copy of the name
        // is not one of the ways out. The bare-`add` ladder ([`plan_bare_add`]) adds them.
        // The chooser is spelled for the verb that refused — a `publish` never sends its reader
        // off to a different command than the one they were running.
        NameResolution::Ambiguous(paths) => {
            Err(chooser(ctx, name, target, verb, false, Vec::new(), paths))
        }
        // `@harness` matched no untracked placement. If the name is nowhere untracked but IS already
        // tracked, this is a re-add — report `ALREADY_TRACKED` the same as the bare form (so an agent
        // branches identically whether or not it typed `@harness`). Otherwise it's a genuine miss.
        NameResolution::HarnessNotFound { harness, available } => {
            if available.is_empty() && tracked_by_name(ctx, name)? {
                Err(ClientError::AlreadyTrackedName {
                    name: name.to_owned(),
                })
            } else {
                Err(ClientError::HarnessNotFound(harness_not_found_message(
                    name, &harness, &available,
                )))
            }
        }
        // A bare name discovery does not surface: distinguish "it's already tracked", "you meant a local
        // dir", and "it truly isn't there" so the agent knows exactly what's wrong.
        NameResolution::NoMatch => {
            if tracked_by_name(ctx, name)? {
                Err(ClientError::AlreadyTrackedName {
                    name: name.to_owned(),
                })
            } else if Path::new(name).exists() {
                // A bare word that is a real cwd entry but no skill — the user likely meant a path.
                Err(ClientError::PathNotName {
                    arg: target.to_owned(),
                    verb: verb.to_owned(),
                })
            } else {
                Err(ClientError::NoUntrackedSkill {
                    name: name.to_owned(),
                })
            }
        }
    }
}

/// One connected workspace that publishes a bare NAME — the workspace half of the bare-name
/// namespace (see [`published_matches`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedName {
    pub host: String,
    pub workspace: String,
    /// The bundle's catalog name (equal to the name that was probed for).
    pub name: String,
    /// The CANONICAL host-qualified spelling — a bare `@ws/name` is ambiguous when sessions on
    /// different servers share a workspace slug.
    pub reference: String,
    /// The digest of the version that workspace serves right now, when a LIVE catalog read
    /// answered. `None` for a cache-only match: the offline record keeps a served VERSION id,
    /// which is a different hash from a bundle digest and must never be compared to one.
    pub bundle_digest: Option<String>,
}

impl PublishedName {
    /// Build a match ONLY when its canonical spelling reads back through the manifest grammar as
    /// exactly this bundle. The grammar reserves spellings — `channels` in the bundle position is
    /// an incomplete channel reference, and a host shape it cannot read has no key at all — and a
    /// match that cannot be SPELLED cannot be subscribed, hinted, or acted on: offering it would
    /// hand the user a command that refuses. Unspellable stays unlisted, which is exactly the
    /// answer the ladder gave before the workspace half of the namespace existed.
    pub(crate) fn spelled(
        host: &str,
        workspace: &str,
        name: &str,
        bundle_digest: Option<String>,
    ) -> Option<Self> {
        let reference = format!("{host}/{workspace}/{name}");
        match crate::manifest::keys::classify_key(&reference) {
            Ok(crate::manifest::keys::KeyShape::WorkspaceBundle { bundle, .. })
                if bundle == name =>
            {
                Some(PublishedName {
                    host: host.to_owned(),
                    workspace: workspace.to_owned(),
                    name: name.to_owned(),
                    reference,
                    bundle_digest,
                })
            }
            _ => None,
        }
    }

    /// The receipt disclosure for an adopt that landed `adopted_digest` bytes: the same workspace
    /// spelling, plus whether those bytes are provably what the workspace serves today (an
    /// unknown digest reads `false` — the disclosure never claims agreement it did not check).
    pub(crate) fn suggestion(&self, adopted_digest: &str) -> topos_types::results::PublishedMatch {
        topos_types::results::PublishedMatch {
            workspace: self.workspace.clone(),
            name: self.name.clone(),
            reference: self.reference.clone(),
            identical: self.bundle_digest.as_deref() == Some(adopted_digest),
        }
    }
}

/// Every connected workspace that publishes `name`, offline-first and best-effort — the workspace
/// half of the namespace a bare `add <name>` resolves against.
///
/// Two sources, unioned by `(host, workspace)` — and a live catalog that ANSWERS replaces its
/// workspace's cache candidate wholesale, so a name the workspace no longer serves (deleted or
/// archived since the last delivery) stops matching the moment the index is read. The offline delivery
/// cache costs nothing and keeps the answer useful on a plane nobody can reach right now, but it
/// only knows what was delivered to THIS machine — and it carries no bundle digest, so a
/// cache-only match can never support an identical-bytes claim. Each ACTIVE session's catalog is
/// the authority on what its workspace actually publishes, and carries the digest.
///
/// The ACTIVE session roster gates BOTH halves: `logout` ends the session but leaves
/// `sync_status.json` behind, so a leftover cache row must not resolve a name toward a workspace
/// this machine can no longer subscribe to (a dead `SESSION_REQUIRED` instead of an honest
/// not-found, or a false two-workspace ambiguity). A cache entry the workspace since WITHDREW is
/// skipped the same way — withdrawn is not published, whatever the cache still holds.
///
/// A session that does not answer is SKIPPED, silently: this is a hint source, and folding a
/// transport fault into "no workspace has it" would turn an unreachable server into a
/// not-found. The authoritative read on the subscribe path happens inside `add_reference`, which
/// refuses honestly when the catalog cannot be read.
pub(crate) fn published_matches(
    ctx: &Ctx<'_>,
    connect: &super::reconcile::SessionConnect<'_>,
    name: &str,
    workspace: Option<&str>,
) -> Vec<PublishedName> {
    let active = active_sessions(ctx, workspace);
    let mut found = cached_matches(ctx, &active, name);
    for s in &active {
        let transports = connect(s);
        let Ok(index) = transports.directory.skills_index(&s.workspace_id) else {
            continue;
        };
        // A catalog that ANSWERED is authoritative for its workspace: a cache candidate the index
        // no longer carries (deleted or archived since the last delivery) is cleared here, never
        // left to fabricate a match. A failed read above keeps the cache's answer — offline
        // degradation, not truth decay.
        found.remove(&(s.host.clone(), s.workspace_name.clone()));
        for e in index.skills {
            if e.name != name || e.status != "active" {
                continue;
            }
            if let Some(published) = PublishedName::spelled(
                &s.host,
                &s.workspace_name,
                &e.name,
                Some(e.bundle_digest.clone()),
            ) {
                found.insert((s.host.clone(), s.workspace_name.clone()), published);
            }
        }
    }
    let mut out: Vec<PublishedName> = found.into_values().collect();
    // Sorted by the spelling the messages print, so a refusal reads the same on every machine.
    out.sort_by(|a, b| a.reference.cmp(&b.reference));
    out
}

/// The ACTIVE session roster — the gate both probe halves stand behind. An unreadable roster reads
/// as empty: nothing subscribable, local-only resolution. `workspace` is the invocation's global
/// `--workspace` selector (already canonicalized to an id): the flag exists to pick WHICH
/// workspace an ambient verb means, so a bare name probes only that one — never silently another.
pub(crate) fn active_sessions(
    ctx: &Ctx<'_>,
    workspace: Option<&str>,
) -> Vec<crate::sessions::Session> {
    let Ok(sessions) = crate::sessions::read_sessions(ctx.fs, &ctx.layout) else {
        return Vec::new();
    };
    sessions
        .sessions
        .into_iter()
        // Only an ACTIVE session's catalog is this person's universe (pending delivers nothing).
        .filter(|s| s.status == crate::sessions::SESSION_ACTIVE)
        .filter(|s| workspace.is_none_or(|id| s.workspace_id == id))
        .collect()
}

/// The CACHE half of [`published_matches`]: what this machine's own delivery history says the
/// still-connected workspaces publish. Costs no network at all.
fn cached_matches(
    ctx: &Ctx<'_>,
    active: &[crate::sessions::Session],
    name: &str,
) -> BTreeMap<(String, String), PublishedName> {
    let mut found: BTreeMap<(String, String), PublishedName> = BTreeMap::new();
    if active.is_empty() {
        return found;
    }
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    for entry in cache.workspaces.values() {
        let (Some(host), Some(workspace)) =
            (entry.host.as_deref(), entry.workspace_name.as_deref())
        else {
            continue; // a pre-session record cannot be spelled as a reference
        };
        if !active
            .iter()
            .any(|s| s.host == host && s.workspace_name == workspace)
        {
            continue; // a session that ended keeps no say in the namespace
        }
        if !entry
            .delivered
            .values()
            .any(|d| d.name == name && !d.withdrawn)
        {
            continue;
        }
        if let Some(published) = PublishedName::spelled(host, workspace, name, None) {
            found.insert((host.to_owned(), workspace.to_owned()), published);
        }
    }
    found
}

/// The receipt disclosure a CLEAN local resolve carries — bounded to at most ONE catalog read,
/// because a local adopt must stay a local act: only this machine's own delivery history nominates
/// a workspace (no fan-out probe across every session just for a courtesy line), and only that one
/// workspace's catalog is then asked — for the digest the identical judgment wants, and to drop a
/// row the workspace no longer serves. The confirming read is best-effort exactly like the full
/// probe: unanswered keeps the cache's (digest-less) disclosure; answered-and-absent withdraws it.
fn confirmed_cached_match(
    ctx: &Ctx<'_>,
    connect: &super::reconcile::SessionConnect<'_>,
    name: &str,
    workspace: Option<&str>,
) -> Option<PublishedName> {
    let active = active_sessions(ctx, workspace);
    let cached = cached_matches(ctx, &active, name);
    // ONE workspace is a spelling worth naming; several is a choice no receipt should make.
    let mut values = cached.into_values();
    let (candidate, None) = (values.next()?, values.next()) else {
        return None;
    };
    let session = active
        .iter()
        .find(|s| s.host == candidate.host && s.workspace_name == candidate.workspace)?;
    let Ok(index) = connect(session)
        .directory
        .skills_index(&session.workspace_id)
    else {
        return Some(candidate); // unanswered: the cache's word stands, claiming no digest
    };
    index
        .skills
        .into_iter()
        .find(|e| e.name == name && e.status == "active")
        .map(|e| PublishedName {
            bundle_digest: Some(e.bundle_digest),
            ..candidate
        })
}

/// What a bare `add <name>` resolved to (see [`plan_bare_add`]).
#[derive(Debug)]
pub(crate) enum BareAddPlan {
    /// A local untracked directory to adopt in place — with the workspace spelling to disclose on
    /// the receipt when exactly one workspace publishes the same name.
    Adopt {
        path: PathBuf,
        name: String,
        published: Option<PublishedName>,
    },
    /// A REFERENCE to record through the ordinary reference arm: the bundle already stands in the
    /// other scope under this name, or nothing local carries it and exactly one connected
    /// workspace publishes it. `note` is the disclosure that add answer carries, when the way the
    /// name resolved is worth stating.
    Reference {
        reference: String,
        note: Option<String>,
    },
    /// The bundle already stands in THIS scope, adopted from a folder, and the invocation named
    /// destinations: nothing is adopted again and no version is minted — the row this file already
    /// spells gains what was named. (A standing bundle with a REFERENCE origin needs no arm of its
    /// own: its reference goes through [`BareAddPlan::Reference`], where the ordinary row write
    /// extends it.)
    ExtendFolderDest { dir: PathBuf },
}

/// What the invocation SAID, beside the name it said it about.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BareAdd<'a> {
    /// The fully-bare gate: an `-s` member pick is about a repo, so a form carrying one never
    /// resolves a name toward a workspace.
    pub subscribe: bool,
    /// The invocation named DESTINATIONS (`-a`/`--dest`). The act is then about where this
    /// bundle's copies live, so a name that already stands here EXTENDS its row instead of
    /// answering that it is already added — which would be true, and useless.
    pub dest_selected: bool,
    /// The invocation's `-g`: it picks which scope is "this" one, and rides every spelled
    /// follow-up so a chosen line lands in the file that was asked for.
    pub global: bool,
    /// The global `--workspace` selector (an id): set, the catalog half probes only that session.
    pub workspace: Option<&'a str>,
}

/// Resolve a bare `add <name>` — STANDING first, discovery second.
///
/// A bare name is what someone calls a bundle they already know, so what they already have is the
/// first thing consulted, in this order:
///
/// 1. **Standing in the scope this invocation acts in** — the answer is [`ClientError::AlreadyAdded`],
///    which reads like the receipt it mirrors. Several records of one name there is the chooser.
///    An invocation naming DESTINATIONS asks a different question, so it gets a different answer:
///    the standing row EXTENDS (`opts.dest_selected`), because "already added" says nothing about
///    the folder the person just asked for.
/// 2. **Standing in the OTHER scope** — the name resolves to that record's own reference and is
///    recorded HERE, an ordinary add with nothing to ask. Reading the other scope is how a bare
///    name resolves at all; the WRITE stays scope-strict, in the file this invocation named. Any
///    `-a`/`--dest` rides that write exactly as it would on the spelled-out reference.
/// 3. **Neither** — the two discovery namespaces: untracked directories on this machine and the
///    connected workspaces' catalogs. Exactly ONE candidate across both acts; two or more is the
///    chooser. Nothing at all is the ordinary not-found.
///
/// A `<name>@<harness>` suffix names a local folder by the agents that read it, so it skips the
/// standing rungs entirely — it is a question about the inventory.
///
/// # Errors
/// [`ClientError::AlreadyAdded`] for a name this scope already records, [`ClientError::AmbiguousSource`]
/// for every ambiguity, plus the name-resolution family of [`resolve_add_target`].
pub(crate) fn plan_bare_add(
    ctx: &Ctx<'_>,
    connect: &super::reconcile::SessionConnect<'_>,
    roots: &super::DiscoveryRoots,
    target: &str,
    opts: BareAdd<'_>,
) -> Result<BareAddPlan, ClientError> {
    let BareAdd {
        subscribe,
        global,
        workspace,
        ..
    } = opts;
    let (name, harness) = split_target(target);
    // The same residual guard [`resolve_add_target`] carries: a `~`-prefixed bare token is never a
    // discovered skill NAME, so it gets path guidance rather than a confusing not-found — and the
    // probe below never runs for it.
    if is_path_shaped(name) {
        return Err(ClientError::PathNotName {
            arg: target.to_owned(),
            verb: "add".to_owned(),
        });
    }
    // THE STANDING RUNGS ANSWER A BARE NAME. Two forms say the question is about something else
    // and skip them: `<name>@<harness>` asks the local INVENTORY which folder an agent reads, and
    // `-s <member>` says the source is a REPO holding several skills. A standing record is neither
    // — letting one answer silently dropped the selector the person typed.
    if harness.is_none()
        && subscribe
        && let Some(plan) = plan_from_standing(ctx, roots, name, opts)?
    {
        return Ok(plan);
    }
    let untracked = super::list::discover_untracked(ctx, roots)?;
    // COUNTING candidates means knowing them all, so the arms that count run the full fan-out
    // probe (one catalog read per active session). The narrowed forms that cannot reach a
    // workspace at all still pay only the bounded courtesy read in [`confirmed_cached_match`].
    match resolve_name(name, harness, &untracked) {
        // A form that cannot resolve toward a workspace: the one local directory is the answer,
        // with the bounded courtesy disclosure.
        NameResolution::Resolved(path) if !subscribe => Ok(BareAddPlan::Adopt {
            path: PathBuf::from(path),
            name: name.to_owned(),
            published: confirmed_cached_match(ctx, connect, name, workspace),
        }),
        NameResolution::Resolved(path) => {
            let published = published_matches(ctx, connect, name, workspace);
            if published.is_empty() {
                return Ok(BareAddPlan::Adopt {
                    path: PathBuf::from(path),
                    name: name.to_owned(),
                    // Nothing publishes it, so there is nothing to disclose.
                    published: None,
                });
            }
            // A folder in front of you and a team's copy of the same name are two different
            // bundles, and picking the folder for you forks the team's process silently.
            Err(chooser(
                ctx,
                name,
                target,
                "add",
                global,
                published.iter().map(|p| p.reference.clone()).collect(),
                vec![path],
            ))
        }
        NameResolution::Ambiguous(paths) => {
            let published = published_matches(ctx, connect, name, workspace);
            Err(chooser(
                ctx,
                name,
                target,
                "add",
                global,
                published.iter().map(|p| p.reference.clone()).collect(),
                paths,
            ))
        }
        NameResolution::HarnessNotFound { harness, available } => {
            if available.is_empty() && tracked_by_name(ctx, name)? {
                Err(ClientError::AlreadyTrackedName {
                    name: name.to_owned(),
                })
            } else {
                Err(ClientError::HarnessNotFound(harness_not_found_message(
                    name, &harness, &available,
                )))
            }
        }
        NameResolution::NoMatch => {
            if Path::new(name).exists() {
                // A bare word that is a real cwd entry but no skill — the user likely meant a path.
                return Err(ClientError::PathNotName {
                    arg: target.to_owned(),
                    verb: "add".to_owned(),
                });
            }
            let published = if subscribe {
                published_matches(ctx, connect, name, workspace)
            } else {
                Vec::new()
            };
            match published.as_slice() {
                [one] => Ok(BareAddPlan::Reference {
                    reference: one.reference.clone(),
                    note: Some(format!(
                        "resolved '{name}' to {} — no untracked skill of that name is on this \
                         machine, and {} publishes it",
                        one.reference, one.workspace
                    )),
                }),
                // Several teams publish the name: naming one for the user would be a guess about
                // whose process they meant. Every spelling, and the user picks.
                [_, _, ..] => Err(chooser(
                    ctx,
                    name,
                    target,
                    "add",
                    global,
                    published.iter().map(|p| p.reference.clone()).collect(),
                    Vec::new(),
                )),
                // Nothing discovery can offer. A form that SKIPPED the standing rungs (`-s`) can
                // still be about a name this machine already tracks, and "no untracked skill of
                // that name" would be a true sentence answering the wrong question.
                [] if tracked_by_name(ctx, name)? => Err(ClientError::AlreadyTrackedName {
                    name: name.to_owned(),
                }),
                // A folder DISCOVERY CANNOT SEE, standing right where the person is looking: a
                // shell whose `SKILL.md` link is broken has no regular marker file, so no probe
                // confirms it as a skill. Answering "no untracked skill of that name" about a
                // folder of exactly that name is the one thing worse than refusing. Probed only
                // here, on the path where there is nothing else to say.
                [] => Err(broken_shell_refusal(ctx, roots, name).unwrap_or(
                    ClientError::NoUntrackedSkill {
                        name: name.to_owned(),
                    },
                )),
            }
        }
    }
}

/// A folder of this NAME that discovery skipped because it is a link shell it cannot resolve — the
/// refusal that shell's own folder earns, or `None` when no skills root holds one.
///
/// Both shapes [`origin_dir`] refuses answer here, and each is the whole truth about that folder: a
/// broken `SKILL.md` link (invisible to every probe — `is_file()` follows the link) and a shell
/// holding its own files beside the link (delisted by [`super::list::discover_untracked`], because
/// as it stands there is no single folder to manage). ONE classifier decides both; this only asks
/// it about the folders a bare name could mean.
///
/// The refusal names the FOLDER, not the bare word: several roots can hold that name, and the
/// person needs to know which directory the answer is about.
fn broken_shell_refusal(
    ctx: &Ctx<'_>,
    roots: &super::DiscoveryRoots,
    name: &str,
) -> Option<ClientError> {
    for root in topos_harness::registry::skills_roots(&roots.home, roots.cwd.as_deref()) {
        let dir = root.join(name);
        if !dir.is_dir() {
            continue;
        }
        if let Err(ClientError::LinkedSource { reason, .. }) = origin_dir(&dir) {
            return Some(ClientError::LinkedSource {
                name: spell_home(ctx, &dir),
                reason,
            });
        }
    }
    None
}

/// The DESTINATION-ONLY add: the folder-adopted bundle already stands in the scope this invocation
/// acts in, and the invocation named where its copies should ALSO live. Nothing is adopted and no
/// version is minted — the row this file already spells for that folder gains the destinations
/// ([`super::manifest_edit::write_row`] extends them), and the caller's narrowed update lands the
/// copies, exactly as it does on every other destination-carrying arm.
///
/// The row is found the way the original add wrote it: from the SAME source folder, so the key
/// this write computes is the key that is already there. The RECORD is found the same way — by
/// the folder, never by a name two bundles could share.
///
/// `Ok(None)` when no record in this scope claims the folder: it is not a re-add at all, and the
/// caller adopts as it always would.
///
/// # Errors
/// The selection's own resolution errors; a store read or manifest write failure.
pub(crate) fn extend_folder_dest(
    ctx: &Ctx<'_>,
    scope: &super::manifest_edit::AddScope,
    dir: &Path,
    selection: &super::dest_select::Selection,
) -> Result<Option<AddData>, ClientError> {
    let sctx = super::pull::ctx_with_layout(ctx, &scope.layout);
    let dir = &origin_dir(dir)?;
    let Some(id) = tracked_skill_at(&sctx, dir)? else {
        return Ok(None);
    };
    let Ok(sid) = SkillId::parse(&id) else {
        return Ok(None);
    };
    let Some(lock) = doc::read_doc::<Lock>(sctx.fs, &sctx.layout.published(&sid).lock)? else {
        return Ok(None);
    };
    // Naming destinations for this folder is a re-demand exactly like naming the folder alone
    // ([`unclaimed_record`]): a RETIRED record (released by the one-time orphan resolution) returns
    // to every surface, so the row written below never stands over a record every store walker
    // skips — a row asking for copies nothing would place, update, or ever take away again.
    crate::sidecar::revive_record(sctx.fs, &sctx.layout, &sid);
    // WHAT the bundle is decides the vocabulary its destinations are spelled in. The row already
    // stands, so a folder that never placed anything is not a fail-closed case here: the same
    // classifier the row writers use, read as the ordinary skill when nothing answers.
    let kind =
        crate::bundle_kind::classify(&sctx, sid.as_str(), &placements_of(&sctx, &sid)).or_skill();
    let entries = if kind.is_mcp() {
        selection.mcp_entries(scope.target.scope)?
    } else {
        selection.skill_entries(scope.target.scope)?
    };
    let mut data = AddData {
        skill_id: Some(sid.into_string()),
        name: lock.name,
        version_id: Some(lock.base_commit),
        bundle_digest: Some(lock.bundle_digest),
        tracked: true,
        // Nothing was adopted, so no trigger was armed and no arming sweep rides this receipt.
        currency: None,
        triggers: Vec::new(),
        origin: None,
        source: Some(dir.display().to_string()),
        manifest: None,
        scope: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        published_match: None,
        mcp: None,
        dest: entries.clone(),
        dest_resolved: Vec::new(),
        dest_change: None,
        claim: None,
        unchanged: false,
        machine_copy: None,
        set_delivery: None,
        display: None,
        note: None,
    };
    super::manifest_edit::note_added_path_kind_dest_in(
        ctx,
        &mut data,
        &scope.target,
        dir,
        // The row already spells its own kind; naming it again writes the same field.
        kind.tag().and(Some(kind)),
        &entries,
    )?;
    Ok(Some(data))
}

/// A record's recorded placements — the second rung the kind classifier reads. An unreadable map
/// is an empty set, which is what an unplaced record honestly is.
fn placements_of(sctx: &Ctx<'_>, sid: &SkillId) -> Vec<String> {
    crate::doc::read_map(sctx.fs, &sctx.layout.published(sid).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default()
}

/// Rungs 1 and 2 of [`plan_bare_add`]: what the two manifest scopes' STORES already know this name
/// to be. `Ok(None)` means neither scope has heard of it and discovery decides.
///
/// # Errors
/// [`ClientError::AlreadyAdded`] (rung 1), [`ClientError::AmbiguousSource`] when either scope holds
/// several records of the name, or a store read failure.
fn plan_from_standing(
    ctx: &Ctx<'_>,
    roots: &super::DiscoveryRoots,
    name: &str,
    opts: BareAdd<'_>,
) -> Result<Option<BareAddPlan>, ClientError> {
    let global = opts.global;
    let here = standing_records(ctx, invoked_store(ctx, global).as_ref(), name)?;
    if !here.is_empty() {
        // ONE thing the answer can name, or none at all, is "already added"; two or more is the
        // pick. A record whose source nothing on this machine can spell still COUNTS as standing
        // — it is added, and saying so with no `source:` line beats saying it is not there.
        //
        // DEDUPED FIRST, then counted. Several records can share ONE source — a forge import
        // lands a record per destination — and counting before the dedup called that ambiguous
        // while the pick it then offered held a single line. The question is how many things the
        // name could MEAN, which is how many distinct sources answer to it.
        let named = spellable(&here);
        if named.len() > 1 {
            return Err(records_chooser(ctx, name, global, &named));
        }
        // DESTINATIONS were named: the question is where this bundle's copies live, not whether
        // it is here. The row this file already spells gains them.
        if opts.dest_selected
            && let Some(one) = named.first()
        {
            return Ok(Some(
                match one.origin.as_ref().expect("a spellable record") {
                    RecordOrigin::Reference(reference) => BareAddPlan::Reference {
                        reference: reference.clone(),
                        note: None,
                    },
                    RecordOrigin::Folder(dir) => BareAddPlan::ExtendFolderDest { dir: dir.clone() },
                },
            ));
        }
        let standing = named.first();
        return Err(ClientError::AlreadyAdded {
            name: name.to_owned(),
            scope: if global {
                ReceiptScope::Machine
            } else {
                ReceiptScope::Project
            },
            from: standing.map(|r| r.token(ctx)),
            candidates: standing
                .map(|r| claimable_copies(ctx, roots, r, name, global))
                .unwrap_or_default(),
        });
    }
    let mut other: Vec<StandingRecord> = Vec::new();
    for layout in other_stores(ctx, global) {
        other.extend(standing_records(ctx, Some(&layout), name)?);
    }
    let other = spellable(&other);
    match other.as_slice() {
        // Nothing over there this add could re-spell: discovery gets its turn.
        [] => Ok(None),
        // The name means what the other scope already means by it: record THAT source here — an
        // ordinary add through the door its shape belongs to.
        [one] => match &one.origin {
            // A REFERENCE is re-recordable: both scopes may subscribe to one workspace bundle,
            // and each lands its own copies in its own dirs from its own store.
            Some(RecordOrigin::Reference(reference)) => Ok(Some(BareAddPlan::Reference {
                reference: reference.clone(),
                note: None,
            })),
            // A FOLDER is not. There is exactly ONE of it, and adopting it here would put a
            // second store's engine on the same directory — the state the claim door refuses
            // outright, for the same reason: two engines converging one folder. So the answer is
            // the honest one, naming the file that already asks for it; the person can look
            // there, or claim a copy of their own.
            Some(RecordOrigin::Folder(_)) => Err(ClientError::AlreadyAdded {
                name: name.to_owned(),
                scope: if global {
                    ReceiptScope::Project
                } else {
                    ReceiptScope::Machine
                },
                from: Some(one.token(ctx)),
                // The claim options belong to a bundle standing HERE; this one does not.
                candidates: Vec::new(),
            }),
            None => unreachable!("spellable() keeps only records with an origin"),
        },
        several => Err(records_chooser(ctx, name, global, several)),
    }
}

/// The DISTINCT sources standing under one name: the records an offered command can actually name,
/// one per origin. Both halves matter — a record nothing can spell cannot appear on a runnable
/// line, and several records of ONE source (a forge import lands one per destination) are one
/// thing the name means, not an ambiguity between identical spellings.
fn spellable(records: &[StandingRecord]) -> Vec<&StandingRecord> {
    let mut out: Vec<&StandingRecord> = Vec::new();
    for record in records.iter().filter(|r| r.origin.is_some()) {
        if !out.iter().any(|kept| kept.origin == record.origin) {
            out.push(record);
        }
    }
    out
}

/// The STORE the invocation writes into, for reading purposes only — never created here (a scope
/// with no store yet has tracked nothing, which is the honest answer). `None` when no `topos.toml`
/// covers this folder at all: the add below will refuse on its own terms.
fn invoked_store(ctx: &Ctx<'_>, global: bool) -> Option<crate::sidecar::Layout> {
    if global {
        return Some(ctx.layout.clone());
    }
    let target = super::manifest_edit::project_target(ctx).ok().flatten()?;
    crate::sidecar::existing_project_store(ctx.fs, &target.dir)
}

/// The stores the invocation does NOT write, nearest first — the machine store for a project
/// invocation, the cwd chain's project stores for `-g`. Reading these is how a bare name resolves;
/// nothing here is ever written.
fn other_stores(ctx: &Ctx<'_>, global: bool) -> Vec<crate::sidecar::Layout> {
    if !global {
        return vec![ctx.layout.clone()];
    }
    let Some(roots) = &ctx.roots else {
        return Vec::new();
    };
    let Some(cwd) = roots.cwd.as_deref() else {
        return Vec::new();
    };
    crate::manifest::scopes::manifest_dirs_up(ctx.fs, cwd, Some(&roots.home))
        .into_iter()
        .filter_map(|dir| crate::sidecar::existing_project_store(ctx.fs, &dir))
        .collect()
}

/// WHERE a tracked record's bytes come from — the three things a bundle can be, and the ONE fact
/// that decides both how it is spelled and which door re-adds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecordOrigin {
    /// A governed reference: a workspace bundle (`<host>/<ws>/<name>`) or a forge import
    /// (`github.com/<owner>/<repo>/<name>`). Re-added through the reference arm.
    Reference(String),
    /// A folder on this machine, adopted in place. Re-added through the path arm.
    Folder(PathBuf),
}

/// One record standing under a bare name. `origin` is `None` for a record nothing on this machine
/// can name — it still counts as standing, it just cannot appear on a runnable line.
struct StandingRecord {
    origin: Option<RecordOrigin>,
    /// The record itself, in the store that holds it — what a byte-proof offer is judged against.
    id: SkillId,
    lock: Lock,
    layout: crate::sidecar::Layout,
}

impl StandingRecord {
    /// The argv token an offered command carries, and the `source:` line of an answer about this
    /// record — a reference verbatim, a folder in the one spelling that stays portable.
    ///
    /// # Panics
    /// Only ever called on a record [`spellable`] kept.
    fn token(&self, ctx: &Ctx<'_>) -> String {
        match self.origin.as_ref().expect("a spellable record") {
            RecordOrigin::Reference(r) => r.clone(),
            RecordOrigin::Folder(dir) => spell_home(ctx, dir),
        }
    }
}

/// Every record in ONE store carrying `name`, each resolved to where its bytes come from.
///
/// # Errors
/// A store read failure.
fn standing_records(
    ctx: &Ctx<'_>,
    layout: Option<&crate::sidecar::Layout>,
    name: &str,
) -> Result<Vec<StandingRecord>, ClientError> {
    let Some(layout) = layout else {
        return Ok(Vec::new());
    };
    let sctx = super::pull::ctx_with_layout(ctx, layout);
    let mut out = Vec::new();
    for (id, lock) in super::skills_named(&sctx, name)? {
        out.push(StandingRecord {
            origin: record_origin(ctx, &sctx, &id, &lock),
            id,
            lock,
            layout: layout.clone(),
        });
    }
    Ok(out)
}

/// The UNMANAGED folders on this machine holding the NAME that already stands here — the listing
/// the answer prints, each folder carrying what its bytes are.
///
/// Byte proof decides the RELATION, not the listing. A same-named folder used to be dropped
/// outright unless its bytes were provably a version of this bundle, because the line offered was
/// a runnable claim and a claim over an unrelated folder invites merging two histories that never
/// met. The listing states the relation instead, so a person sees every folder the name is in AND
/// what adopting each would mean — and the folder whose bytes nothing here explains simply reads
/// `edited`, with the draft it would become spelled out beside it. A copy the record's own history
/// DOES explain (an older version — the next update brings it current) is neither current nor a
/// draft, so it carries no clause at all rather than a wrong one.
///
/// SCOPE, as everywhere: a project-invoked answer lists only folders inside the checkout, and `-g`
/// only the machine's own. Best-effort throughout — discovery or a store read that fails offers
/// nothing rather than half a list.
fn claimable_copies(
    ctx: &Ctx<'_>,
    roots: &super::DiscoveryRoots,
    record: &StandingRecord,
    name: &str,
    global: bool,
) -> Vec<TargetCandidate> {
    // A record nothing can spell has no `--as` value, so there is no line to print.
    let Some(origin) = &record.origin else {
        return Vec::new();
    };
    let (as_argv, as_display) = match origin {
        RecordOrigin::Reference(reference) => (reference.clone(), reference.clone()),
        RecordOrigin::Folder(dir) => (dir.to_string_lossy().into_owned(), spell_home(ctx, dir)),
    };
    let Ok(untracked) = super::list::discover_untracked(ctx, roots) else {
        return Vec::new();
    };
    let want_scope = if global { "user" } else { "project" };
    let sctx = super::pull::ctx_with_layout(ctx, &record.layout);
    let sp = sctx.layout.published(&record.id);
    let store = Store::open(&sp.store).ok();
    let mut found: Vec<(TargetCandidate, Option<String>)> = Vec::new();
    for entry in untracked.iter().filter(|u| u.name == name) {
        if entry.scope != want_scope {
            continue;
        }
        let dir = origin_dir_or_self(Path::new(&entry.path));
        let Ok(scanned) = scan::scan(&dir) else {
            continue;
        };
        let digest = to_hex(&scanned.bundle_digest);
        let candidate = TargetCandidate::claim(
            dir.to_string_lossy().into_owned(),
            spell_home(ctx, &dir),
            as_argv.clone(),
            as_display.clone(),
        );
        // The digest rides along until the whole set is known: `edited differently` is a fact
        // about a folder AND the ones listed before it, so it cannot be decided one at a time.
        let differing = (digest != record.lock.bundle_digest
            && !store
                .as_ref()
                .is_some_and(|s| super::claim::digest_in_history(s, &digest).unwrap_or(false)))
        .then_some(digest.clone());
        found.push((
            if digest == record.lock.bundle_digest {
                candidate.with_relation(Some(CopyRelation::Current))
            } else {
                candidate
            },
            differing,
        ));
    }
    found.sort();
    found.dedup();
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<TargetCandidate> = Vec::new();
    for (candidate, differing) in found {
        let relation = differing.map(|digest| {
            let differently = !seen.is_empty() && !seen.contains(&digest);
            seen.push(digest);
            if differently {
                CopyRelation::EditedDifferently
            } else {
                CopyRelation::Edited
            }
        });
        out.push(match relation {
            Some(r) => candidate.with_relation(Some(r)),
            None => candidate,
        });
    }
    out
}

/// Where a tracked record came from — the one join every surface that has to name a record's
/// origin makes: a DELIVERED record is its workspace's canonical spelling, a forge import is its
/// `<host>/<owner>/<repo>/<skill>` key, and a locally adopted one is the folder its bytes live in.
/// `None` where none of the three answers, which is a record nothing on this machine can name.
///
/// The workspace join comes from the delivery cache, which is machine-wide (in the HOME sidecar):
/// a project store's own layout is the checkout's, so that read goes through the outer ctx. A
/// WITHDRAWN row is not an answer — the workspace does not serve that name any more, so a command
/// spelling it would refuse, and the next rung gets its chance.
pub(super) fn record_origin(
    ctx: &Ctx<'_>,
    sctx: &Ctx<'_>,
    id: &SkillId,
    lock: &Lock,
) -> Option<RecordOrigin> {
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    for entry in cache.workspaces.values() {
        if let (Some(host), Some(workspace)) =
            (entry.host.as_deref(), entry.workspace_name.as_deref())
            && entry
                .delivered
                .get(id.as_str())
                .is_some_and(|d| !d.withdrawn)
        {
            return Some(RecordOrigin::Reference(format!(
                "{host}/{workspace}/{}",
                lock.name
            )));
        }
    }
    if let Ok(Some(origin)) = doc::read_doc::<OriginDoc>(sctx.fs, &sctx.layout.published(id).origin)
    {
        return Some(RecordOrigin::Reference(format!(
            "{}/{}",
            origin.origin.source, lock.name
        )));
    }
    let map = crate::doc::read_map(sctx.fs, &sctx.layout.published(id).map)
        .ok()
        .flatten()?;
    map.placements
        .iter()
        .zip(&map.placement_state)
        .find(|(_, st)| st.adopted_source)
        // A folder that is GONE names nothing: an offered `topos add <that folder>` would refuse
        // as a missing source, and a `source:` line pointing at it would send a reader somewhere
        // there is nothing to look at. The record still stands — it just cannot be spelled.
        .filter(|(dir, _)| ctx.fs.exists(Path::new(dir)))
        .map(|(dir, _)| RecordOrigin::Folder(PathBuf::from(dir)))
}

/// The chooser over STANDING records — the same shape every other add ambiguity ends in, with
/// each record's own re-add spelling as one line.
fn records_chooser(
    ctx: &Ctx<'_>,
    name: &str,
    global: bool,
    records: &[&StandingRecord],
) -> ClientError {
    let mut remote = Vec::new();
    let mut local = Vec::new();
    for record in records {
        match record.origin {
            Some(RecordOrigin::Reference(_)) => remote.push(record.token(ctx)),
            Some(RecordOrigin::Folder(_)) => local.push(record.token(ctx)),
            None => {}
        }
    }
    // The standing rungs are only reached by a BARE name (an `@harness` suffix skips them), so
    // the name IS the token the person typed.
    chooser(ctx, name, name, "add", global, remote, local)
}

/// THE ambiguity answer, built once for every surface that meets one: remote spellings first, then
/// local paths, each group sorted and deduped, every line one runnable command.
///
/// `local` arrives as ABSOLUTE directories and stays absolute in argv — a `~/` prefix is a SHELL's
/// to expand, and this binary resolves a source path itself, so a home-abbreviated token would
/// name nothing at all. The abbreviation is made here once, as a DISPLAY carried beside the argv
/// form, and reaches the printed line alone.
///
/// `token` is the positional as the person typed it, so a rebuilt command can swap exactly that
/// one and leave the rest of their invocation standing.
pub(super) fn chooser(
    ctx: &Ctx<'_>,
    name: &str,
    token: &str,
    verb: &str,
    global: bool,
    remote: Vec<String>,
    local: Vec<String>,
) -> ClientError {
    let mut remote = remote;
    remote.sort();
    remote.dedup();
    let mut local = local;
    local.sort();
    local.dedup();
    let mut candidates: Vec<crate::error::TargetCandidate> =
        remote.into_iter().map(TargetCandidate::plain).collect();
    candidates.extend(local.into_iter().map(|dir| {
        let display = spell_home(ctx, Path::new(&dir));
        TargetCandidate::folder(dir, display)
    }));
    ClientError::AmbiguousSource {
        name: name.to_owned(),
        token: token.to_owned(),
        verb: verb.to_owned(),
        candidates,
        global,
    }
}

/// A folder as a PRINTED line spells it: `~/`-abbreviated under this machine's home, absolute
/// otherwise. Display only — never argv (see [`chooser`]).
pub(super) fn spell_home(ctx: &Ctx<'_>, dir: &Path) -> String {
    let Some(roots) = &ctx.roots else {
        return dir.display().to_string();
    };
    match dir.strip_prefix(&roots.home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// The outcome of matching a name (+ optional harness slug) against the discovered untracked inventory —
/// the PURE core of [`resolve_add_target`], so the whole dispatch is unit-tested without a filesystem.
#[derive(Debug, PartialEq, Eq)]
enum NameResolution {
    /// Exactly one placement — its directory path.
    Resolved(String),
    /// A bare name (no `@harness`) that no placement carries.
    NoMatch,
    /// `@harness` was given but no folder that agent reads holds the skill; `available` are the slugs
    /// of every agent that DOES read a folder holding it (sorted, deduped).
    HarnessNotFound {
        harness: String,
        available: Vec<String>,
    },
    /// The name sits in more than one untracked DIRECTORY — the paths the caller picks between.
    /// One shape whether or not an `@harness` filter narrowed the set: the way out is the same
    /// either way, which is to name one of them.
    Ambiguous(Vec<String>),
}

/// Split `<skill>[@<harness>]` on the LAST `@` (a harness slug never contains one). A degenerate token
/// (empty name or empty harness — `foo@`, `@bar`) is treated as a bare name, so it fails as an ordinary
/// not-found rather than a confusing empty-harness lookup.
pub(crate) fn split_target(target: &str) -> (&str, Option<&str>) {
    match target.rsplit_once('@') {
        Some((name, harness)) if !name.is_empty() && !harness.is_empty() => (name, Some(harness)),
        _ => (target, None),
    }
}

/// The pure matcher over the discovered inventory. `harness` NARROWS the entries to the ones whose
/// folder that agent READS (the registry's one attribution answer, carried on every entry); the
/// counting that follows is identical with or without it, because the answer to "which of these
/// did you mean" is the same list of directories either way.
fn resolve_name(name: &str, harness: Option<&str>, untracked: &[UntrackedEntry]) -> NameResolution {
    let by_name: Vec<&UntrackedEntry> = untracked.iter().filter(|u| u.name == name).collect();
    let matched: Vec<&UntrackedEntry> = match harness {
        Some(h) => by_name
            .iter()
            .copied()
            .filter(|u| u.readers.iter().any(|slug| slug == h))
            .collect(),
        None => by_name.clone(),
    };
    match (matched.as_slice(), harness) {
        ([], Some(h)) => NameResolution::HarnessNotFound {
            harness: h.to_owned(),
            available: distinct_sorted_readers(&by_name),
        },
        ([], None) => NameResolution::NoMatch,
        ([one], _) => NameResolution::Resolved(one.path.clone()),
        (many, _) => NameResolution::Ambiguous(many.iter().map(|u| u.path.clone()).collect()),
    }
}

/// Every agent that reads a folder holding one of these entries, sorted + deduped (deterministic
/// error copy) — the `@<slug>` tokens that CAN select something.
fn distinct_sorted_readers(entries: &[&UntrackedEntry]) -> Vec<String> {
    let mut slugs: Vec<String> = entries.iter().flat_map(|u| u.readers.clone()).collect();
    slugs.sort();
    slugs.dedup();
    slugs
}

/// The verbatim guidance for `add <skill>@<harness>` when that harness has no such skill — naming where it
/// IS found, if anywhere.
fn harness_not_found_message(name: &str, harness: &str, available: &[String]) -> String {
    if available.is_empty() {
        format!(
            "no untracked skill named '{name}' in harness '{harness}' — run `topos list` to see what's adoptable"
        )
    } else {
        format!(
            "no untracked skill named '{name}' in harness '{harness}' — it is available in: {} (try `topos add {name}@<harness>`)",
            available.join(", ")
        )
    }
}

/// Whether a name token is SYNTACTICALLY a path (a separator, or a `.`/`~` prefix) — so it can never be a
/// discovered skill NAME (a bare basename) and gets actionable path guidance instead. The weaker "a bare
/// word that happens to be a cwd entry" heuristic is applied only AFTER discovery finds no name match (so
/// a skill named the same as a cwd dir still resolves).
fn is_path_shaped(arg: &str) -> bool {
    arg.contains('/') || arg.contains('\\') || arg.starts_with('.') || arg.starts_with('~')
}

/// Whether a TRACKED skill already carries this name (discovery would then exclude its dir, so a
/// zero-match is really "already adopted"). An ambiguous tracked name still counts as tracked.
fn tracked_by_name(ctx: &Ctx<'_>, name: &str) -> Result<bool, ClientError> {
    match super::resolve_skill(ctx, name) {
        Ok(_) | Err(ClientError::AmbiguousName { .. }) => Ok(true),
        Err(ClientError::NoSuchSkill { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(super) fn locked_files(bundle: &ScannedBundle) -> Vec<LockedFile> {
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

/// The refusal an adopted name earns when it would claim a placement dir topos keeps for itself —
/// the ADD-TIME half of the reservation [`topos_harness::is_reserved_skill_dir`] enforces in the
/// naming ladder. Keyed on the SANITIZED name, the same spelling the ladder reserves, so a name
/// that would land in a reserved dir is answered here instead of silently renamed.
///
/// Only an add refuses. A person naming a folder asked a question and gets an answer; a workspace
/// DELIVERY has nobody to ask — a silent update cannot refuse — so there the ladder's degrade to a
/// suffixed dir stands as the belt.
fn reserved_name_refusal(name: &str, skill_id: &str) -> Option<String> {
    let dir = topos_harness::sanitize_skill_dir(name)?;
    if !topos_harness::is_reserved_skill_dir(&dir, skill_id) {
        return None;
    }
    let held = if dir == topos_harness::RESERVED_MCP_PLUGIN_DIR {
        "topos's own MCP surface"
    } else {
        "the built-in topos skill"
    };
    Some(format!(
        "the name `{dir}` is reserved for {held} — adopt under another name (`--skill <name>`)"
    ))
}

/// Refuse to re-adopt a directory topos already tracks (same canonical path). Best-effort: the writer
/// lock is per fresh skill id, so a rare concurrent `add` of the same dir could still race through to
/// today's same-name `AmbiguousName`; the common re-run is caught here.
///
/// # Errors
/// [`ClientError::AlreadyTracked`] if a tracked skill already records this canonical path; otherwise an
/// [`FsOps`](crate::fs_seam::FsOps) read failure.
fn reject_already_tracked(ctx: &Ctx<'_>, canonical_source: &Path) -> Result<(), ClientError> {
    match tracked_skill_at(ctx, canonical_source)? {
        Some(skill_id) => Err(already_tracked(ctx, &skill_id, canonical_source)),
        None => Ok(()),
    }
}

/// The `ALREADY_TRACKED` refusal, spelled in things a person can act on. The store id that
/// [`tracked_skill_at`] answers with is the LOOKUP key, never the answer: it is resolved here to
/// the record's own name, the folder, and the manifest row that installs it.
///
/// Each part degrades on its own. An unreadable lock falls back to the folder's basename (still a
/// name someone typed); no reachable manifest spelling the folder leaves `claim` empty and the
/// sentence stops at the folder. Nothing in the chain reintroduces the id.
fn already_tracked(ctx: &Ctx<'_>, skill_id: &str, canonical_dir: &Path) -> ClientError {
    let name = tracked_name(ctx, skill_id)
        .or_else(|| dir_basename(canonical_dir))
        .unwrap_or_else(|| canonical_dir.display().to_string());
    ClientError::AlreadyTracked {
        name,
        dir: canonical_dir.display().to_string(),
        claim: claiming_row(ctx, canonical_dir),
    }
}

/// A tracked record's own NAME, read from its lock — the one document that carries it. `None`
/// when the id does not parse or the lock cannot be read.
fn tracked_name(ctx: &Ctx<'_>, skill_id: &str) -> Option<String> {
    let id = SkillId::parse(skill_id).ok()?;
    let lock = doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&id).lock).ok()??;
    Some(lock.name)
}

/// What a refusal CALLS the record holding a folder: its own name, and the store id only when
/// nothing else can be read — the one degradation every ownership answer takes.
pub(super) fn record_name(sctx: &Ctx<'_>, skill_id: &str) -> String {
    tracked_name(sctx, skill_id).unwrap_or_else(|| skill_id.to_owned())
}

/// ANOTHER SCOPE's store already manages this folder — the refusal both folder doors take.
///
/// The stores are [`super::manifest_edit::other_scope_stores`]; `claim` is the bundle a `--as`
/// named, or `None` for a plain path adopt, which is asking to manage the folder outright. Either
/// way the refusal names the file the owner answers to: that is where the folder is governed from,
/// and the only place a person can act on it.
///
/// `dir` is the ORIGIN — the folder the bytes really live in, resolved by [`origin_dir`] before
/// this is asked, exactly as the record write keys it.
///
/// TWO ENGINES CONVERGING ONE DIRECTORY is the whole hazard, so `kind` decides whether there is
/// one: a server bundle is delivered as config ENTRIES and places no directory at all, so one
/// `server.json` folder may legitimately stand in both scopes, each minting its own config key.
/// The refusal arms the moment either side would place the folder.
///
/// # Errors
/// [`ClientError::ClaimTaken`] naming the owning bundle and its manifest; a store read failure.
pub(super) fn refuse_other_scope(
    ctx: &Ctx<'_>,
    scope: &super::manifest_edit::AddScope,
    dir: &Path,
    claim: Option<&str>,
    kind: BundleKind,
) -> Result<(), ClientError> {
    for (layout, file) in super::manifest_edit::other_scope_stores(ctx, scope) {
        let octx = super::pull::ctx_with_layout(ctx, &layout);
        let Some(id) = tracked_skill_at(&octx, dir)? else {
            continue;
        };
        if kind.is_mcp() && record_is_mcp(&octx, &id) {
            continue; // neither side places this folder — nothing to converge, nothing to refuse
        }
        return Err(ClientError::ClaimTaken {
            dir: crate::ops::inventory::pretty(ctx, dir),
            claim: claim.map(str::to_owned),
            owner: record_name(&octx, &id),
            manifest: Some(file),
        });
    }
    Ok(())
}

/// Whether the record `id` in `sctx`'s store is a server bundle — the one classifier, read as the
/// ordinary skill when nothing answers (which is what an unplaceable record honestly is here).
fn record_is_mcp(sctx: &Ctx<'_>, id: &str) -> bool {
    let Ok(sid) = SkillId::parse(id) else {
        return false;
    };
    crate::bundle_kind::classify(sctx, sid.as_str(), &placements_of(sctx, &sid))
        .or_skill()
        .is_mcp()
}

/// The manifest file whose row already installs `canonical_dir`. The scope THIS store belongs to
/// is consulted first — the add was routed to it, so its file is the one that made the folder
/// tracked — and the other scope only as a fallback. A project store never claims to speak for
/// the machine file (its layout home is the checkout's own store, not `~/.topos`).
///
/// Best-effort by contract: this runs on a refusal path, so an unreadable or unparseable manifest
/// answers `None` and the refusal simply says less. It never turns a refusal into a different one.
fn claiming_row(ctx: &Ctx<'_>, canonical_dir: &Path) -> Option<TrackedBy> {
    let project = super::manifest_edit::project_target(ctx).ok().flatten();
    let machine =
        (!ctx.layout.is_project_scope()).then(|| super::manifest_edit::global_target(ctx));
    let ordered = if ctx.layout.is_project_scope() {
        [project, machine]
    } else {
        [machine, project]
    };
    ordered.into_iter().flatten().find_map(|target| {
        super::manifest_edit::path_row_claims(ctx, &target, canonical_dir)
            .ok()?
            .then(|| TrackedBy {
                manifest: target.path.display().to_string(),
                global: target.scope == crate::manifest::document::ManifestScope::Global,
            })
    })
}

/// The id of the tracked skill whose placement resolves to `canonical_source` (canonical `Path` compare,
/// resolving symlinks/firmlinks on both sides; a placement that no longer resolves on disk is stale, not a
/// match), or `None` if that directory is not tracked. The shared predicate behind
/// [`reject_already_tracked`] and the remote-import [`check_destination`].
pub(crate) fn tracked_skill_at(
    ctx: &Ctx<'_>,
    canonical_source: &Path,
) -> Result<Option<String>, ClientError> {
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
        let Some(map) = doc::read_map(ctx.fs, &ctx.layout.published(&id).map)? else {
            continue;
        };
        if map.placements.iter().any(|p| {
            Path::new(p)
                .canonicalize()
                .is_ok_and(|c| c == *canonical_source)
        }) {
            return Ok(Some(id.into_string()));
        }
    }
    Ok(None)
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

/// The installed agents that read the folder holding `source_abs` — the registry's ONE attribution
/// answer, resolved against the real env home + cwd. Best-effort provenance: no `$HOME` (or no
/// parent) ⇒ nobody.
pub(crate) fn folder_readers(source_abs: &Path) -> Vec<String> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    let Some(folder) = source_abs.parent() else {
        return Vec::new();
    };
    let cwd = std::env::current_dir().ok();
    topos_harness::registry::folder_reader_slugs(folder, &home, cwd.as_deref())
}

/// Consult each ACTIVE session's catalog for a GOVERNED copy of `spec`'s source — the dedup
/// suggestion a remote import's receipt carries ("acme already has this as `@acme/deploy`").
/// Matching is by upstream host + `owner/repo` over the catalog's additive upstream fields; a
/// path-exact match wins over a same-repo sibling. `imported_subdir` is the skill path the
/// import actually SELECTED inside the repo (the recorded origin — a `--skill` pick or a
/// multi-skill repo resolves deeper than the spec's own subdir), so path-exactness is judged
/// against what landed, not what was typed. Best-effort by design: no sessions, a transport
/// fault, or an upstream-less catalog all answer `None` — the suggestion is a courtesy, never
/// a gate on the import (npm shape: warn beside the act, never block it).
pub(crate) fn governed_copy_suggestion(
    ctx: &Ctx<'_>,
    connect: &super::reconcile::SessionConnect<'_>,
    spec: &RemoteSpec,
    imported_subdir: Option<&str>,
) -> Option<topos_types::results::GovernedCopy> {
    let sessions = crate::sessions::read_sessions(ctx.fs, &ctx.layout).ok()?;
    let want_host = spec.host.domain();
    let want_repo = format!("{}/{}", spec.owner, spec.repo);
    let want_path = imported_subdir
        .or(spec.subdir.as_deref())
        .unwrap_or_default();
    let mut sibling: Option<topos_types::results::GovernedCopy> = None;
    for s in &sessions.sessions {
        // Only an ACTIVE session's catalog is this person's universe (pending delivers nothing).
        if s.status != crate::sessions::SESSION_ACTIVE {
            continue;
        }
        let transports = connect(s);
        let Ok(index) = transports.directory.skills_index(&s.workspace_id) else {
            continue;
        };
        for e in index.skills {
            if e.status != "active" {
                continue;
            }
            let (Some(host), Some(repo)) = (e.upstream_host.as_deref(), e.upstream_repo.as_deref())
            else {
                continue;
            };
            if host != want_host || repo != want_repo {
                continue;
            }
            let same_path = e.upstream_path.as_deref().unwrap_or("") == want_path;
            let copy = topos_types::results::GovernedCopy {
                workspace: s.workspace_name.clone(),
                name: e.name.clone(),
                // The CANONICAL host-qualified spelling — a bare `@ws/name` is ambiguous when
                // sessions on different servers share a workspace slug.
                reference: format!("{}/{}/{}", s.host, s.workspace_name, e.name),
                same_path,
            };
            if same_path {
                return Some(copy);
            }
            sibling.get_or_insert(copy);
        }
    }
    sibling
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One discovered untracked row: a skill dir, its folder, and the agents that read the folder —
    /// only the fields name resolution reads matter.
    fn ue(name: &str, readers: &[&str], path: &str) -> UntrackedEntry {
        let p = std::path::Path::new(path);
        UntrackedEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            folder: p.parent().unwrap_or(p).to_string_lossy().into_owned(),
            readers: readers.iter().map(|s| (*s).to_owned()).collect(),
            scope: "user".to_owned(),
            original: None,
        }
    }

    #[test]
    fn split_target_splits_on_the_last_at_and_ignores_degenerate_forms() {
        assert_eq!(split_target("deploy"), ("deploy", None));
        assert_eq!(
            split_target("deploy@claude-code"),
            ("deploy", Some("claude-code"))
        );
        // Rsplit: a name may (pathologically) contain '@' — the harness is the final segment.
        assert_eq!(split_target("we@ird@cursor"), ("we@ird", Some("cursor")));
        // Degenerate tokens fold back to a bare name (they fail as an ordinary not-found).
        assert_eq!(split_target("deploy@"), ("deploy@", None));
        assert_eq!(split_target("@cursor"), ("@cursor", None));
    }

    #[test]
    fn a_single_discovered_placement_resolves_to_its_path() {
        let inv = vec![ue("deploy", &["claude-code"], "/h/.claude/skills/deploy")];
        assert_eq!(
            resolve_name("deploy", None, &inv),
            NameResolution::Resolved("/h/.claude/skills/deploy".to_owned())
        );
        // The correct `@harness` resolves the same single placement.
        assert_eq!(
            resolve_name("deploy", Some("claude-code"), &inv),
            NameResolution::Resolved("/h/.claude/skills/deploy".to_owned())
        );
    }

    #[test]
    fn a_name_in_two_folders_is_ambiguous_until_disambiguated() {
        let inv = vec![
            ue("deploy", &["claude-code"], "/h/.claude/skills/deploy"),
            ue("deploy", &["cursor"], "/h/.cursor/skills/deploy"),
        ];
        // Bare name → ambiguous across the two DIRECTORIES, which is what a printed line names.
        assert_eq!(
            resolve_name("deploy", None, &inv),
            NameResolution::Ambiguous(vec![
                "/h/.claude/skills/deploy".to_owned(),
                "/h/.cursor/skills/deploy".to_owned()
            ])
        );
        // `@harness` picks the folder that agent reads.
        assert_eq!(
            resolve_name("deploy", Some("cursor"), &inv),
            NameResolution::Resolved("/h/.cursor/skills/deploy".to_owned())
        );
    }

    #[test]
    fn a_slug_reading_two_folders_cannot_split_them() {
        // One agent reads both folders (its user dir and this project's dir) — `@harness` selects
        // both, so the caller picks by path. Narrowed or not, the answer is the same shape: the
        // directories that are still on the table.
        let inv = vec![
            ue("deploy", &["claude-code"], "/h/.claude/skills/deploy"),
            ue("deploy", &["claude-code"], "/proj/.claude/skills/deploy"),
        ];
        let both = NameResolution::Ambiguous(vec![
            "/h/.claude/skills/deploy".to_owned(),
            "/proj/.claude/skills/deploy".to_owned(),
        ]);
        assert_eq!(resolve_name("deploy", Some("claude-code"), &inv), both);
        assert_eq!(resolve_name("deploy", None, &inv), both);
    }

    #[test]
    fn a_shared_folder_answers_to_every_agent_that_reads_it() {
        // The shared dir: BOTH slugs select the same single entry, and neither is ambiguous.
        let inv = vec![ue("deploy", &["warp", "zed"], "/h/.agents/skills/deploy")];
        for slug in ["warp", "zed"] {
            assert_eq!(
                resolve_name("deploy", Some(slug), &inv),
                NameResolution::Resolved("/h/.agents/skills/deploy".to_owned())
            );
        }
        // An agent that does NOT read the folder is told who does.
        assert_eq!(
            resolve_name("deploy", Some("cursor"), &inv),
            NameResolution::HarnessNotFound {
                harness: "cursor".to_owned(),
                available: vec!["warp".to_owned(), "zed".to_owned()],
            }
        );
    }

    #[test]
    fn a_bare_name_with_no_placement_is_no_match() {
        let inv = vec![ue("deploy", &["claude-code"], "/h/.claude/skills/deploy")];
        assert_eq!(resolve_name("lint", None, &inv), NameResolution::NoMatch);
    }

    #[test]
    fn a_wrong_harness_reports_where_the_skill_actually_lives() {
        let inv = vec![ue("deploy", &["claude-code"], "/h/.claude/skills/deploy")];
        // Named for an agent that does not read the folder → HarnessNotFound, listing who does.
        assert_eq!(
            resolve_name("deploy", Some("cursor"), &inv),
            NameResolution::HarnessNotFound {
                harness: "cursor".to_owned(),
                available: vec!["claude-code".to_owned()],
            }
        );
        // Named nowhere at all, with a harness → HarnessNotFound with no alternatives.
        assert_eq!(
            resolve_name("ghost", Some("cursor"), &inv),
            NameResolution::HarnessNotFound {
                harness: "cursor".to_owned(),
                available: vec![],
            }
        );
    }

    #[test]
    fn is_path_shaped_flags_syntactic_path_forms_only() {
        assert!(is_path_shaped("./deploy"));
        assert!(is_path_shaped("skills/deploy"));
        assert!(is_path_shaped("~/x/deploy"));
        assert!(is_path_shaped("../deploy"));
        assert!(is_path_shaped("a\\b"));
        // A plain skill name is NOT path-shaped — even if a cwd entry of that name exists (that weaker
        // heuristic runs only AFTER discovery finds no match, so a skill named like a cwd dir still
        // resolves as a name).
        assert!(!is_path_shaped("deploy"));
    }

    #[test]
    fn standing_records_of_one_source_count_once_and_unnameable_ones_never_count() {
        let record = |origin: Option<RecordOrigin>| StandingRecord {
            origin,
            id: SkillId::parse("topos_00000000000000000000000000000000").expect("a valid id"),
            lock: Lock {
                schema_version: PERSISTED_SCHEMA_VERSION,
                skill_id: "topos_00000000000000000000000000000000".to_owned(),
                name: "deploy".to_owned(),
                base_commit: String::new(),
                bundle_digest: String::new(),
                files: Vec::new(),
            },
            layout: crate::sidecar::Layout::new(Path::new("/h/.topos")),
        };
        let reference = |r: &str| Some(RecordOrigin::Reference(r.to_owned()));
        // A forge import lands one record PER DESTINATION. They are one thing the name means, not
        // an ambiguity between two identical spellings — counting before the dedup called it
        // ambiguous and then offered a "pick one" list holding a single line.
        let same_source = vec![
            record(reference("github.com/acme/tools/deploy")),
            record(reference("github.com/acme/tools/deploy")),
        ];
        assert_eq!(spellable(&same_source).len(), 1);
        // Two GENUINELY different sources under one name stay two.
        let two_sources = vec![
            record(reference("github.com/acme/tools/deploy")),
            record(Some(RecordOrigin::Folder(PathBuf::from(
                "/h/skills/deploy",
            )))),
        ];
        assert_eq!(spellable(&two_sources).len(), 2);
        // A record nothing can name is never offered — a printed line has to be runnable — but it
        // is not counted away either: its caller still knows the name STANDS.
        let unnameable = vec![record(None), record(reference("acme.test/eng/deploy"))];
        assert_eq!(spellable(&unnameable).len(), 1);
        assert!(spellable(&[record(None)]).is_empty());
    }

    /// One probed workspace match; `digest` `None` is the cache-only shape (no live catalog read).
    fn pn(host: &str, workspace: &str, name: &str, digest: Option<&str>) -> PublishedName {
        PublishedName {
            host: host.to_owned(),
            workspace: workspace.to_owned(),
            name: name.to_owned(),
            reference: format!("{host}/{workspace}/{name}"),
            bundle_digest: digest.map(str::to_owned),
        }
    }

    #[test]
    fn a_match_exists_only_where_its_spelling_reads_back_as_the_bundle() {
        // A port-bearing self-hosted session spells fine — the grammar reads it back.
        let ported = PublishedName::spelled("localhost:3000", "acme", "deploy", None)
            .expect("a ported host is a legal reference");
        assert_eq!(ported.reference, "localhost:3000/acme/deploy");
        // `channels` is RESERVED in the bundle position (an incomplete channel reference) — a
        // bundle so named cannot be spelled, so it is never offered.
        assert!(PublishedName::spelled("topos.sh", "acme", "channels", None).is_none());
        // A host the grammar cannot read has no key at all.
        assert!(PublishedName::spelled("not a host", "acme", "deploy", None).is_none());
    }

    #[test]
    fn a_receipt_suggestion_calls_the_bytes_identical_only_when_they_are() {
        let live = pn("acme.test", "eng", "deploy", Some("beef"));
        let same = live.suggestion("beef");
        assert_eq!(same.workspace, "eng");
        assert_eq!(same.name, "deploy");
        assert_eq!(same.reference, "acme.test/eng/deploy");
        assert!(same.identical);
        assert!(!live.suggestion("cafe").identical);
        // An unknown digest reads `false` — never a claim of agreement nobody checked.
        assert!(
            !pn("acme.test", "eng", "deploy", None)
                .suggestion("beef")
                .identical
        );
    }

    #[test]
    fn harness_not_found_message_names_alternatives_when_they_exist() {
        let none = harness_not_found_message("deploy", "cursor", &[]);
        assert!(none.contains("'deploy'") && none.contains("'cursor'"));
        assert!(!none.contains("available in"));
        let some = harness_not_found_message("deploy", "cursor", &["claude-code".to_owned()]);
        assert!(some.contains("available in: claude-code"));
        assert!(some.contains("topos add deploy@<harness>"));
    }
}
