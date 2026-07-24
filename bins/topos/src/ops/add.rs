//! `add <source>` — adopt a skill. The positional is source-polymorphic (classified in
//! [`crate::source`]): a local PATH (`./ ../ ~/ /…`) adopts a directory in place; a bare skill NAME
//! resolves against the untracked inventory `list` discovers (see [`resolve_add_target`]); a remote source
//! (`owner/repo`, a github.com URL) fetches + imports it (see [`add_remote`]). Adoption itself
//! ([`add_with_name`]) mints an id + name, scans + imports to the embedded-git store, snapshots the genesis
//! version, and writes the sidecar docs — all staged and published with one directory rename, so it is
//! all-or-nothing and the source bytes are never touched.

use std::path::{Path, PathBuf};

use topos_core::digest::{FileMode, to_hex};
use topos_core::identity::{self, Commit};
use topos_gitstore::{ImportFile, Store};
use topos_harness::DiscoveredPlacement;
use topos_harness::registry::SkillScope;
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, LockedFile, PlacementMap, SwapCapability, SyncState};
use topos_types::results::{AddData, KeepAsYoursData, KeepReason, SkillOrigin, UntrackedEntry};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::git_source::{GitTarballSource, RepoFile, extract_tree};
use crate::id::SkillId;
use crate::scan::{self, ScannedBundle};
use crate::source::RemoteSpec;
use crate::{doc, logfile, sidecar};

/// The fixed, controlled-ASCII commit message for a genesis adopt — folded into the `version_id`
/// preimage, so it must stay constant for a deterministic id.
const ADD_MESSAGE: &str = "topos: add";

/// Adopt the skill rooted at `source`, naming it from the source itself (a recognized harness dir's name,
/// else frontmatter-then-basename) — the direct-path entry point (a path-shaped positional).
///
/// # Errors
/// [`ClientError::SourceOverlap`] if `source` overlaps `~/.topos/`; [`ClientError::EmptyBundle`] /
/// [`ClientError::Scan`] from the scan; [`ClientError::SkillExists`] on an id collision; otherwise a
/// store/io failure.
pub(crate) fn add(ctx: &Ctx<'_>, source: &Path) -> Result<AddData, ClientError> {
    add_with_name(ctx, source, None)
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
) -> Result<AddData, ClientError> {
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
    // auto-update applies to it. A plain/unrecognized dir is tracked in place with no harness association.
    let recognized = recognize(ctx, &source_abs);

    // Mint identity. A recognized harness skill is keyed by its DIRECTORY name (the command name the
    // harness invokes); a plain dir keeps the frontmatter-first-then-basename order. The minted id is
    // parsed through the validated newtype like any other (the id source mints `topos_<hex>`, which
    // always fits — the parse is the type-level proof the path joins below demand).
    let skill_id = crate::id::SkillId::parse(&ctx.ids.new_skill_id())?;
    // A name-resolved `add <skill>` forces the discovered name (what `list` showed, what `publish`/`diff`
    // will resolve) — so an adopt-only registry harness never tracks the bytes under a divergent
    // frontmatter name. Absent an override: a recognized harness skill is keyed by its DIRECTORY name (the
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

    // The built-in skill's name is reserved end-to-end — an adopted skill can never share it (the
    // sidecar, `list`, `publish`, and the placement dirs would all collide with the built-in's).
    if super::builtin::is_builtin(&name) {
        return Err(ClientError::InvalidArgument(
            "the name `topos` is reserved for the built-in topos skill — adopt under another name \
             (`--skill <name>`)"
                .into(),
        ));
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
        },
    )?;
    // Attribute the harness. Either the adapter recognized it (adopt-in-place; auto-update armed below), OR
    // the baked registry places the source under a known harness's skill dir — recorded for forward-compat
    // even when topos has no full adapter for it (a later adapter can arm auto-updates for this adopted skill).
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
    doc::write_map(
        ctx.fs,
        &sp.map,
        &PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: vec![placement],
            applied_commit: version_hex.clone(),
            materialized_sha: digest_hex.clone(),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            // The adopted source dir is the ONE native placement (the adopted bytes ARE what topos
            // recorded, so the per-placement sha starts at the adopted digest).
            placement_state: vec![topos_types::persisted::PlacementState {
                kind: topos_types::persisted::PlacementKind::Native,
                agent: harness_slug.clone(),
                materialized_sha: Some(digest_hex.clone()),
                pre_existing_sha: None,
                swap_capability: SwapCapability::Unsupported,
            }],
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

    // Arm auto-update for a recognized harness — a best-effort, idempotent edit of the harness CONFIG
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
        triggers: Vec::new(),
        // Set by the remote-import wrapper ([`add_remote`]); a local adopt has no upstream.
        origin: None,
        // Set by the manifest-edit step at the composition root (the verb records the demand line).
        manifest: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
    })
}

/// The options a remote `add <owner/repo>` carries beyond the source spec.
#[derive(Debug, Clone, Default)]
pub(crate) struct AddRemoteOpts {
    /// Pick one skill from a multi-skill repo (else: a lone skill is taken; several is a typed error).
    pub skill: Option<String>,
    /// Land into this harness's skills dir (a registry slug); `None` = the active harness.
    pub harness: Option<String>,
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
}

/// Adopt a skill fetched from a REMOTE source (`owner/repo`, a github.com URL). Resolves the destination
/// harness dir, refuses to clobber it, materializes the byte-exact skill there, then adopts it through the
/// unchanged [`add_with_name`] core and records the provenance. Fully non-interactive — the source's
/// trustworthiness is the caller's (user/agent) responsibility, so there is no disclosure gate.
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

    // 1. Destination harness + scope. Default: the active harness (the one topos drives + can arm auto-updates
    //    for). An explicit `--harness` must name a known registry slug.
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
    let scope = if opts.global {
        SkillScope::User
    } else {
        SkillScope::Project
    };
    let dest_root =
        topos_harness::registry::skills_root(&slug, scope, &roots.home, roots.cwd.as_deref())
            .ok_or_else(|| ClientError::InvalidArgument(destination_hint(&slug, opts.global)))?;

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
    ctx.fs.create_dir_all(&dest_root)?;
    let stage_dir = dest_root.join(format!(".topos-import-{}", selected.name));
    if ctx.fs.exists(&stage_dir) {
        ctx.fs.remove_dir_all(&stage_dir)?;
    }
    if let Err(e) = write_skill_dir(ctx, &stage_dir, &selected.files) {
        let _ = ctx.fs.remove_dir_all(&stage_dir);
        return Err(e);
    }
    // An empty pre-existing dest is fine to fill (check_destination allowed it) but blocks the no-replace
    // rename — clear it first. A non-empty dest was already refused above.
    if ctx.fs.exists(&dest_dir) {
        ctx.fs.remove_dir_all(&dest_dir)?;
    }
    if let Err(e) = ctx.fs.rename_dir_noreplace(&stage_dir, &dest_dir) {
        let _ = ctx.fs.remove_dir_all(&stage_dir);
        return Err(ClientError::Io(format!(
            "place import at {}: {e}",
            dest_dir.display()
        )));
    }
    let mut data = match add_with_name(ctx, &dest_dir, Some(&selected.name)) {
        Ok(d) => d,
        Err(e) => {
            let _ = ctx.fs.remove_dir_all(&dest_dir);
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
    if let Ok(id) = crate::id::SkillId::parse(&data.skill_id) {
        let doc = OriginDoc {
            schema_version: PERSISTED_SCHEMA_VERSION,
            origin: origin.clone(),
            imported_at: ctx.clock.now_unix_millis(),
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
    let data = add_with_name(ctx, &dest, Some(name))?;
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
        return Err(ClientError::AlreadyTracked { skill_id });
    }
    Err(ClientError::PlacementOccupied {
        path: dest.display().to_string(),
    })
}

/// Write a selected skill's byte-exact files into `dest`, preserving the executable bit (part of the
/// digest). Paths are archive-relative forward-slash and already `..`/absolute-safe (extraction rejected
/// hazards), so the component-wise join stays inside `dest`.
fn write_skill_dir(ctx: &Ctx<'_>, dest: &Path, files: &[RepoFile]) -> Result<(), ClientError> {
    for f in files {
        let mut path = dest.to_path_buf();
        for comp in f.path.split('/') {
            path.push(comp);
        }
        if let Some(parent) = path.parent() {
            ctx.fs
                .create_dir_all(parent)
                .map_err(|e| ClientError::Io(format!("create {}: {e}", parent.display())))?;
        }
        let executable = f.mode & 0o111 != 0;
        ctx.fs.write_staged(&path, &f.bytes, executable)?;
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
/// The name-resolution family — [`ClientError::AmbiguousHarness`] / [`ClientError::AmbiguousScope`] /
/// [`ClientError::HarnessNotFound`] / [`ClientError::NoUntrackedSkill`] / [`ClientError::AlreadyTrackedName`]
/// / [`ClientError::PathNotName`] — or a discovery read failure.
pub(crate) fn resolve_add_target(
    ctx: &Ctx<'_>,
    roots: &super::DiscoveryRoots,
    target: &str,
) -> Result<(std::path::PathBuf, String), ClientError> {
    let (name, harness) = split_target(target);
    // A residual guard: `crate::source` already routes path shapes to a direct adopt, but a `~`-prefixed
    // bare token can still arrive here — it is never a discovered skill NAME (those are bare basenames),
    // so refuse it with actionable guidance rather than a confusing not-found.
    if is_path_shaped(name) {
        return Err(ClientError::PathNotName {
            arg: target.to_owned(),
        });
    }
    let untracked = super::list::discover_untracked(ctx, roots)?;
    match resolve_name(name, harness, &untracked) {
        NameResolution::Resolved(path) => Ok((std::path::PathBuf::from(path), name.to_owned())),
        NameResolution::AmbiguousHarness(harnesses) => Err(ClientError::AmbiguousHarness {
            name: name.to_owned(),
            harnesses,
        }),
        NameResolution::AmbiguousScope { harness, paths } => Err(ClientError::AmbiguousScope {
            name: name.to_owned(),
            harness,
            paths,
        }),
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
                })
            } else {
                Err(ClientError::NoUntrackedSkill {
                    name: name.to_owned(),
                })
            }
        }
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
    /// `@harness` was given but that harness holds no such skill; `available` are the slugs that DO (sorted).
    HarnessNotFound {
        harness: String,
        available: Vec<String>,
    },
    /// The name sits under more than one harness — the sorted, deduped slugs the caller picks from.
    AmbiguousHarness(Vec<String>),
    /// The name matches more than one directory within a SINGLE harness (e.g. user + project scope) —
    /// `@harness` cannot split them, so the caller adopts one by path.
    AmbiguousScope { harness: String, paths: Vec<String> },
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

/// The pure matcher over the discovered inventory. `harness` filters by registry slug; without it, a
/// same-name collision across harnesses is [`NameResolution::AmbiguousHarness`] and a collision within one
/// harness is [`NameResolution::AmbiguousScope`].
fn resolve_name(name: &str, harness: Option<&str>, untracked: &[UntrackedEntry]) -> NameResolution {
    let by_name: Vec<&UntrackedEntry> = untracked.iter().filter(|u| u.name == name).collect();
    match harness {
        Some(h) => {
            let in_h: Vec<&UntrackedEntry> =
                by_name.iter().copied().filter(|u| u.harness == h).collect();
            match in_h.as_slice() {
                [] => NameResolution::HarnessNotFound {
                    harness: h.to_owned(),
                    available: distinct_sorted_slugs(&by_name),
                },
                [one] => NameResolution::Resolved(one.path.clone()),
                many => NameResolution::AmbiguousScope {
                    harness: h.to_owned(),
                    paths: many.iter().map(|u| u.path.clone()).collect(),
                },
            }
        }
        None => match by_name.as_slice() {
            [] => NameResolution::NoMatch,
            [one] => NameResolution::Resolved(one.path.clone()),
            many => {
                let slugs = distinct_sorted_slugs(many);
                if slugs.len() == 1 {
                    NameResolution::AmbiguousScope {
                        harness: slugs.into_iter().next().expect("len == 1"),
                        paths: many.iter().map(|u| u.path.clone()).collect(),
                    }
                } else {
                    NameResolution::AmbiguousHarness(slugs)
                }
            }
        },
    }
}

/// The distinct harness slugs across a set of entries, sorted (deterministic error copy).
fn distinct_sorted_slugs(entries: &[&UntrackedEntry]) -> Vec<String> {
    let mut slugs: Vec<String> = entries.iter().map(|u| u.harness.clone()).collect();
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

/// Refuse to re-adopt a directory topos already tracks (same canonical path). Best-effort: the writer
/// lock is per fresh skill id, so a rare concurrent `add` of the same dir could still race through to
/// today's same-name `AmbiguousName`; the common re-run is caught here.
///
/// # Errors
/// [`ClientError::AlreadyTracked`] if a tracked skill already records this canonical path; otherwise an
/// [`FsOps`](crate::fs_seam::FsOps) read failure.
fn reject_already_tracked(ctx: &Ctx<'_>, canonical_source: &Path) -> Result<(), ClientError> {
    match tracked_skill_at(ctx, canonical_source)? {
        Some(skill_id) => Err(ClientError::AlreadyTracked { skill_id }),
        None => Ok(()),
    }
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

/// Which known harness's skill dir `source_abs` sits under (baked registry), using the real env home + cwd.
/// Best-effort provenance: no `$HOME` ⇒ no attribution.
fn registry_attribution(source_abs: &Path) -> Option<topos_harness::registry::HarnessAttribution> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let cwd = std::env::current_dir().ok();
    topos_harness::registry::attribute_path(source_abs, &home, cwd.as_deref())
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

    /// One discovered untracked row — only the fields name resolution reads matter.
    fn ue(name: &str, harness: &str, path: &str) -> UntrackedEntry {
        UntrackedEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            harness: harness.to_owned(),
            harness_name: harness.to_owned(),
            adapter_supported: false,
            scope: "user".to_owned(),
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
        let inv = vec![ue("deploy", "claude-code", "/h/.claude/skills/deploy")];
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
    fn a_name_in_two_harnesses_is_ambiguous_until_disambiguated() {
        let inv = vec![
            ue("deploy", "claude-code", "/h/.claude/skills/deploy"),
            ue("deploy", "cursor", "/h/.cursor/skills/deploy"),
        ];
        // Bare name → ambiguous across the two (sorted) slugs.
        assert_eq!(
            resolve_name("deploy", None, &inv),
            NameResolution::AmbiguousHarness(vec!["claude-code".to_owned(), "cursor".to_owned()])
        );
        // `@harness` picks the one.
        assert_eq!(
            resolve_name("deploy", Some("cursor"), &inv),
            NameResolution::Resolved("/h/.cursor/skills/deploy".to_owned())
        );
    }

    #[test]
    fn a_name_twice_in_one_harness_is_a_scope_ambiguity_at_harness_not_split() {
        // Same name, same harness slug, two directories (user + project) — `@harness` cannot split them.
        let inv = vec![
            ue("deploy", "claude-code", "/h/.claude/skills/deploy"),
            ue("deploy", "claude-code", "/proj/.claude/skills/deploy"),
        ];
        let scope = NameResolution::AmbiguousScope {
            harness: "claude-code".to_owned(),
            paths: vec![
                "/h/.claude/skills/deploy".to_owned(),
                "/proj/.claude/skills/deploy".to_owned(),
            ],
        };
        assert_eq!(resolve_name("deploy", None, &inv), scope);
        assert_eq!(resolve_name("deploy", Some("claude-code"), &inv), scope);
    }

    #[test]
    fn a_bare_name_with_no_placement_is_no_match() {
        let inv = vec![ue("deploy", "claude-code", "/h/.claude/skills/deploy")];
        assert_eq!(resolve_name("lint", None, &inv), NameResolution::NoMatch);
    }

    #[test]
    fn a_wrong_harness_reports_where_the_skill_actually_lives() {
        let inv = vec![ue("deploy", "claude-code", "/h/.claude/skills/deploy")];
        // Named in a harness that lacks it → HarnessNotFound, listing where it IS.
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
    fn harness_not_found_message_names_alternatives_when_they_exist() {
        let none = harness_not_found_message("deploy", "cursor", &[]);
        assert!(none.contains("'deploy'") && none.contains("'cursor'"));
        assert!(!none.contains("available in"));
        let some = harness_not_found_message("deploy", "cursor", &["claude-code".to_owned()]);
        assert!(some.contains("available in: claude-code"));
        assert!(some.contains("topos add deploy@<harness>"));
    }
}
