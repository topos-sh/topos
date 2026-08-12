//! `remove [SKILL]...` — the CLASSIC removal: bytes this machine holds that NO manifest row asks
//! for. The manifest arms run first (see [`super::manifest_edit`]); everything they do not claim
//! lands here, and every shape here is a PERMANENT delete, so all of them are describe-first:
//!
//! - a TRACKED, never-published LOCAL skill → the agent dirs AND the sidecar entry go (no other
//!   copy exists).
//! - the same skill when its bytes live in a folder the person ADOPTED IN PLACE → the record and
//!   its config entries retire and the FOLDER STAYS. One rule decides, everywhere in this file:
//!   topos deletes only what topos created. An adopted source existed before the row and belongs
//!   to the person after it, so no arm here — not the orphan fall-through, not `--yes` — is a
//!   route to losing it.
//! - an UNTRACKED local copy sitting in an agent dir (`<name>@<agent>`) → a permanent delete of
//!   that directory (topos never adopted it — deleting it is the only removal there is).
//! - the built-in `topos` skill → the durable device opt-out (`topos add topos` brings it back).
//!
//! A skill a workspace delivers is refused toward the DEMAND — but only while a row actually
//! claims the name: what a folder takes is its `topos.toml` row, what this machine takes is the
//! global file's, and what a workspace gives you is managed on the web. A retained copy whose
//! demand already ended (an ORPHANED record — the next `topos update` resolves it once and
//! retires it) is nobody's demand, so it falls through to the describe-first permanent delete
//! with an honest note. Multi-skill positional; resolve ALL-OR-NONE (a batch either resolves
//! every target or applies nothing).

use std::collections::HashSet;
use std::path::PathBuf;

use topos_types::results::{RemoveData, RemoveItem, RemoveKind};

use super::DiscoveryRoots;
use super::connect::DirectoryConnect;
use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::resolve::{self, ParsedTarget, Resolution};

/// The seams `remove` needs — the directory connector builds the resolution universe and writes the
/// per-device exclusion row.
pub(crate) struct RemoveConnectors<'a> {
    /// The per-session transports (the resolver universe reads ride each session's credential).
    pub session: &'a super::reconcile::SessionConnect<'a>,
    #[allow(dead_code)]
    pub directory: &'a DirectoryConnect<'a>,
}

/// The verb's outcome — the two-phase pair, plus the `--agent` per-agent exclusion (the shared
/// placement-policy surface `unfollow --agent` also runs).
#[derive(Debug)]
pub(crate) enum RemoveOutcome {
    Described {
        data: RemoveData,
        yes_argv: Vec<String>,
    },
    Applied(RemoveData),
}

/// One resolved removal, pre-apply.
enum Removal {
    /// A tracked local skill no row demands → a permanent delete (sidecar entry included). For an
    /// ORPHAN — a retained workspace-delivered copy whose demand already ended, which the next
    /// `topos update` resolves once — the notes carry the honest disclosure (describe-tense and
    /// applied-tense; receipts must stay true in both).
    ///
    /// `dirs` and `kept_dirs` split the record's placements by ONE question: did topos create this
    /// folder? `dirs` are topos's own, and the delete is permanent. `kept_dirs` are the person's —
    /// an ADOPTED SOURCE, a folder that existed before the row and belongs to them after it — and
    /// no removal deletes one. It is the same boundary every sweep already holds
    /// ([`super::reconcile`] spares an adopted source from every clean); this arm holds it too,
    /// because a verb reachable from a listing's suggested command must not be the one place a
    /// person's own directory can be destroyed.
    TrackedLocal {
        /// The store the record lives in — the machine's, or the checkout's own. Every per-record
        /// read and write below rides THIS layout, never `ctx.layout`: a project record resolved
        /// by a home-store layout is a delete aimed at whatever same-named machine record the walk
        /// happened to find first.
        layout: crate::sidecar::Layout,
        skill_id: String,
        name: String,
        dirs: Vec<PathBuf>,
        kept_dirs: Vec<PathBuf>,
        note: Option<OrphanNote>,
    },
    /// An untracked copy in an agent dir → a permanent delete of that directory.
    Untracked { name: String, dir: PathBuf },
    /// The built-in `topos` skill → the durable device opt-out (no sweep re-places it;
    /// `topos add topos` brings it back).
    Builtin { dirs: Vec<PathBuf> },
}

/// Dispatch the `remove` verb: resolve every target (all-or-none), describe (bare) or apply (`--yes`).
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a workspace / channel target (refused toward the right verb);
/// [`ClientError::TargetNotFound`] for an unresolvable one; a transport / io failure on apply.
pub(crate) fn remove(
    ctx: &Ctx<'_>,
    connectors: &RemoveConnectors<'_>,
    targets: &[String],
    agents: &[String],
    roots: Option<&DiscoveryRoots>,
    yes: bool,
) -> Result<RemoveOutcome, ClientError> {
    if targets.is_empty() {
        return Err(ClientError::InvalidArgument(
            "remove needs a skill name (or `<name>@<agent>` for an untracked local copy)".into(),
        ));
    }
    // A single `-a` value scopes untracked locals; more than one is accepted (a copy in several agents).
    let agent_filter: Option<&str> = match agents {
        [] => None,
        [one] if one != "*" => Some(one.as_str()),
        // `*` (every agent) and multi-`-a` fall through to the discovery resolver, which already spans
        // every harness dir — the untracked delete removes whatever the name resolves to.
        _ => None,
    };

    let universe = super::connect::build_universe_sessions(ctx, connectors.session)?;
    let demanded = machine_demand(ctx)?;

    // Resolve ALL-OR-NONE.
    let mut removals = Vec::with_capacity(targets.len());
    for token in targets {
        removals.push(classify(
            ctx,
            &universe,
            &demanded,
            roots,
            agent_filter,
            token,
        )?);
    }

    let mut items: Vec<RemoveItem> = removals.iter().map(|r| describe_item(r, false)).collect();

    // The gate holds the WHOLE verb: every shape that reaches this file is a permanent delete —
    // a tracked local's only copy, an untracked dir topos never adopted, or the built-in's durable
    // opt-out — so every one of them describes first and applies only under `--yes`. There is no
    // ungated arm here; the reversible per-device acts live on the manifest verbs.
    for (removal, item) in removals.iter().zip(items.iter_mut()) {
        // A config-placed bundle's blast radius is not its dirs — the `--yes` gate must name
        // the agent configs the apply will edit BEFORE consent, not only on the receipt after
        // it. The list is knowable now: the bundle's own record names every entry topos placed.
        if let Removal::TrackedLocal {
            layout, skill_id, ..
        } = removal
        {
            let files = mcp_entry_files(ctx, layout, skill_id);
            if let Some(line) = also_removes_line(&files) {
                item.note = Some(match item.note.take() {
                    Some(prev) => format!("{prev} · {line}"),
                    None => line,
                });
            }
        }
    }

    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), "remove".to_owned()];
        yes_argv.extend(targets.iter().cloned());
        for a in agents {
            yes_argv.push("-a".to_owned());
            yes_argv.push(a.clone());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(RemoveOutcome::Described {
            data: RemoveData {
                items,
                applied: false,
                undo: Vec::new(),
                uninstalled: Vec::new(),
            },
            yes_argv,
        });
    }

    // ---- APPLY (`--yes` only — the describe above is the one way in) ----
    // The APPLIED items re-derive so an orphan's note speaks in the right tense (the describe's
    // "doing nothing also resolves this" would be false on a receipt for an act just performed).
    let mut items: Vec<RemoveItem> = removals.iter().map(|r| describe_item(r, true)).collect();
    for (removal, item) in removals.iter().zip(items.iter_mut()) {
        match removal {
            Removal::TrackedLocal {
                layout,
                skill_id,
                dirs,
                ..
            } => {
                // A config-placed bundle's reach is not these dirs — it is the entries it wrote
                // into agents' MCP configs. They go FIRST, while the record that names them still
                // exists: retired afterwards there would be nothing left to prove which entries
                // were ever this bundle's, and they would sit in those files forever.
                retire_mcp_entries(ctx, layout, skill_id, item);
                // ONLY the folders topos made. `kept_dirs` is deliberately not iterated here —
                // an adopted source is the person's directory, and the record retiring is the
                // whole of what this verb ends for it.
                for dir in dirs {
                    if ctx.fs.exists(dir) {
                        ctx.fs.remove_dir_all(dir)?;
                    }
                }
                // Drop the sidecar entry — a never-published local has no other copy.
                let sid = SkillId::parse(skill_id)?;
                let skill_dir = layout.skill_dir(&sid);
                if ctx.fs.exists(&skill_dir) {
                    ctx.fs.remove_dir_all(&skill_dir)?;
                }
            }
            Removal::Untracked { dir, .. } => {
                if ctx.fs.exists(dir) {
                    ctx.fs.remove_dir_all(dir)?;
                }
            }
            Removal::Builtin { .. } => {
                super::builtin::remove_builtin(ctx)?;
            }
        }
    }
    // No undo command: every shape this file applies is a permanent delete, and a receipt offers
    // an inverse only when the inverse restores the whole prior state. A wrong undo is worse than
    // none.
    Ok(RemoveOutcome::Applied(RemoveData {
        items,
        applied: true,
        undo: Vec::new(),
        uninstalled: Vec::new(),
    }))
}

/// The config files this bundle's own record carries standing entries in, `~`-abbreviated, in
/// record order and deduped — what the describe names before consent and what the apply will edit.
/// Empty for an ordinary skill record (no config entries) and for a scope that never config-placed.
fn mcp_entry_files(ctx: &Ctx<'_>, layout: &crate::sidecar::Layout, skill_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in crate::config_custody::entries_of(ctx.fs, layout, skill_id) {
        let file = super::inventory::pretty(ctx, std::path::Path::new(&entry.file));
        if !out.contains(&file) {
            out.push(file);
        }
    }
    out
}

/// The describe's blast-radius line for the config entries a removal will take with it — the same
/// "one is named inline, several are spelled out" shape every destination list here uses, in the
/// tense of something not yet done. `None` when there are none to name.
fn also_removes_line(files: &[String]) -> Option<String> {
    match files {
        [] => None,
        [one] => Some(format!("also removes its MCP server entry from {one}")),
        many => Some(format!(
            "also removes its MCP server entries from {}",
            many.join(", ")
        )),
    }
}

/// Retire this scope's MCP config entries for a record the classic delete is about to erase — the
/// same convergence the manifest arm runs when a row drops ([`super::manifest_edit`]), so the two
/// removal routes leave the machine in the same state. A record that does not classify as MCP, a
/// scope that never config-placed anything, and a machine with no agent roots are all no-ops.
///
/// [`crate::mcp_engine::remove_bundle`] takes the scope's converge lock itself, so this runs under
/// the same serialization every other config write does; a lock it cannot take becomes a receipt
/// line, never a silent skip. The per-agent outcomes fold into the item's note for the same reason
/// the manifest arm folds them: a removal that touched somebody's agent config says which files it
/// touched.
fn retire_mcp_entries(
    ctx: &Ctx<'_>,
    layout: &crate::sidecar::Layout,
    skill_id: &str,
    item: &mut RemoveItem,
) {
    let Some(roots) = ctx.roots.clone() else {
        return;
    };
    if !ctx.fs.exists(&layout.config_custody_path()) {
        return; // nothing was ever config-placed in this scope
    }
    let Ok(sid) = SkillId::parse(skill_id) else {
        return;
    };
    // Classification does not need the placement map: the durable marker answers first, and an
    // unreadable map only costs the manifest-row rung below it.
    let placements = doc::read_map(ctx.fs, &layout.published(&sid).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    // The kind marker and the scope's project root are BOTH per-store facts, so they are asked of
    // the owning store — a project record classified against the home layout reads whatever marker
    // a same-named machine record happens to carry.
    let sctx = super::pull::ctx_with_layout(ctx, layout);
    if !crate::bundle_kind::classify(&sctx, skill_id, &placements).is_mcp() {
        return;
    }
    let project_root = layout.project_root().map(std::path::Path::to_path_buf);
    let detected: std::collections::BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &roots.home,
        project_root.as_deref().or(roots.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    let io = crate::mcp_engine::ScopeIo {
        fs: ctx.fs,
        layout,
        home: roots.home.clone(),
        project_root,
    };
    let outcome = crate::mcp_engine::remove_bundle(
        &io,
        &topos_harness::mcp::descriptor::mcp_harnesses_for_teardown(),
        &detected,
        skill_id,
        &item.name,
    );
    // A DRIFTED entry is left in place, so its custody row survives this removal — and the caller
    // is about to delete the record that holds it. Move those rows to the scope document first, or
    // the hand-edited entry would sit in the person's config forever with nothing left to prove it
    // was ever topos's. There they stay disclosable, and a sweep after the edit is reverted still
    // takes them out.
    let detach_warnings = crate::mcp_engine::detach_bundle_rows(&io, skill_id);
    // Keyed by the config FILE the entry lived in — receipts speak in destinations, never agents.
    // ORDER IS THE MESSAGE — the same rule the manifest arm follows: a hand-edited entry
    // SURVIVING the removal is the fact a person must not miss, so it leads, wherever the
    // harness table happened to put its agent.
    let mut lines: Vec<String> = Vec::with_capacity(outcome.removed.len());
    let mut gone: Vec<String> = Vec::new();
    for removed in &outcome.removed {
        let file = removed.state.file.as_deref().map_or_else(
            || "its config".to_owned(),
            |f| super::inventory::pretty(ctx, std::path::Path::new(f)),
        );
        match removed.state.state {
            topos_types::results::TargetOutcome::Drifted => {
                lines.push(format!(
                    "{file}: the entry you edited by hand is left in place."
                ));
            }
            _ => gone.push(format!("{file}: the server's entry was removed.")),
        }
    }
    lines.extend(gone);
    // The notices land in a person's RECEIPT here, not in the sweep's machine channel — so they
    // arrive as the PROSE the typed message carries. The code rides `messages[].code` on the
    // sweep's envelope and nowhere near a receipt line.
    lines.extend(outcome.notices.iter().map(|n| n.text.clone()));
    lines.extend(outcome.warnings.iter().map(|w| w.text.clone()));
    // A detach that could not take the lock or write the scope document has LOST custody of a
    // drifted row the record is about to take with it. That is the person's business, not a silent
    // fact — it rides the same receipt lines as every other warning this removal produced.
    lines.extend(detach_warnings.iter().map(|w| w.text.clone()));
    if lines.is_empty() {
        return;
    }
    // ONE CLAUSE PER LINE — the renderer leads with the first and indents the rest.
    let folded = lines.join("\n");
    item.note = Some(match item.note.take() {
        Some(prev) => format!("{prev}\n{folded}"),
        None => folded,
    });
}

/// The verbatim demand-refusal: a workspace-delivered skill whose demand still STANDS is removed
/// by editing the demand, never by deleting the copy.
/// A record THIS MACHINE'S OWN file still demands, named from a folder whose scope cannot edit
/// that row. The path form already refuses toward `-g` when no `topos.toml` covers the folder; the
/// name form owes the same answer, because the alternative is deleting bytes a standing row will
/// keep asking for.
fn standing_row_refusal(name: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "'{name}' is demanded by this machine's own topos.toml — deleting the copy would leave \
         that row standing (every later `topos update -g` would fail on the missing folder); \
         `topos remove -g {name}` drops the row and the copy together"
    ))
}

/// A record a CHECKOUT'S OWN file still demands, named from somewhere that file is not the nearest
/// one (a nested checkout, most often). The machine twin above can offer `-g`; here the row lives
/// in a specific file, and the verb that edits it is the same `remove` run from the folder that
/// file governs — so the refusal names both.
fn standing_project_row_refusal(name: &str, file: &str, dir: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "'{name}' is demanded by {file} — deleting the copy would leave that row standing (every \
         later `topos update` there would fail on the missing folder); run `topos remove {name}` \
         from {dir} to drop the row and the copy together"
    ))
}

fn delivered_refusal(name: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "'{name}' is delivered from a workspace — remove the DEMAND, not the copy: `topos \
         remove {name}` drops this folder's line for it; `topos remove -g {name}` edits your \
         machine-wide file (switching it off here). What the workspace assigns you is managed \
         on the web."
    ))
}

/// Every name (and reference) the MACHINE scope still demands — the rows (bundle or `"off"`) of
/// the same offline resolution `list` and `status` render. Half of the demand-guard's key: a
/// record whose name no row claims is an ORPHAN — its demand already ended, so refusing toward a
/// row that does not exist would be false. Offline by construction (no dial).
///
/// This is the MACHINE half only. The classic ladder resolves where you STAND, so it also reaches
/// records held in a checkout's own store, and a row in THAT checkout's file is just as standing a
/// demand as a machine-wide one — [`project_demand`] asks the scope that owns the record.
fn machine_demand(ctx: &Ctx<'_>) -> Result<HashSet<String>, ClientError> {
    let (all, cache) = super::inventory::read_sources(ctx)?;
    let resolved = super::inventory::resolve(ctx, &all, &cache)?;
    let mut out = HashSet::new();
    for row in &resolved.machine().rows {
        out.insert(row.name.clone());
        out.insert(row.reference.clone());
    }
    Ok(out)
}

/// What ONE checkout's own file still demands, and which file that is — the same offline
/// resolution, asked from inside `dir` so the checkout's own `topos.toml` is the nearest one.
///
/// It exists because "where you stand" and "what is demanded" were answered by different scopes. A
/// checkout NESTED inside another resolves its own file as nearest, while the record ladder walks
/// out to the parent — so a bare `remove` from the inner folder found the parent's record, asked
/// only the machine's rows, and read a live parent row as an ended delivery. Resolving at the
/// OWNING checkout is what makes the two answers the same question.
///
/// Full row resolution rather than a plain read of the file's keys: a channel line demands its
/// MEMBERS by name, and a guard that only saw the literal rows would miss every one of them.
fn project_demand(
    ctx: &Ctx<'_>,
    dir: &std::path::Path,
) -> Result<(HashSet<String>, Option<PathBuf>), ClientError> {
    let mut roots = ctx.roots.clone();
    match roots.as_mut() {
        Some(r) => r.cwd = Some(dir.to_path_buf()),
        // No discovery roots means no cwd chain was ever walked, so no project store could have
        // answered — nothing to ask.
        None => return Ok((HashSet::new(), None)),
    }
    let at = Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        triggers: ctx.triggers.clone(),
        plane: ctx.plane,
        follow: ctx.follow,
        roots,
        progress: ctx.progress,
    };
    let (all, cache) = super::inventory::read_sources(&at)?;
    let resolved = super::inventory::resolve(&at, &all, &cache)?;
    let Some(section) = resolved.project() else {
        return Ok((HashSet::new(), None));
    };
    let mut out = HashSet::new();
    for row in &section.rows {
        out.insert(row.name.clone());
        out.insert(row.reference.clone());
    }
    Ok((out, section.manifest_path.clone()))
}

/// Classify ONE target: a followed catalog skill (exclusion), a tracked-local (permanent), or an
/// untracked agent-dir copy (permanent). A workspace / channel target is refused toward the right verb.
fn classify(
    ctx: &Ctx<'_>,
    universe: &[resolve::WorkspaceNames],
    demanded: &HashSet<String>,
    roots: Option<&DiscoveryRoots>,
    agent_filter: Option<&str>,
    token: &str,
) -> Result<Removal, ClientError> {
    // The built-in `topos` skill — recognized before the grammar (the name is reserved end-to-end,
    // so it can never shadow a workspace resource): removal is the durable device opt-out.
    if super::builtin::is_builtin(token) {
        return Ok(Removal::Builtin {
            dirs: super::builtin::placement_dirs(ctx)?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        });
    }
    let parsed = resolve::parse_target(token)?;
    // An explicit `<name>@<agent>` names an untracked agent-dir copy — resolve it through discovery
    // (never a plane resource).
    if let ParsedTarget::LocalAt { name, agent } = &parsed {
        return untracked(ctx, demanded, roots, Some(agent.as_str()), name);
    }
    // Resolve against the plane universe (SKILLS scope). A channel / workspace match is refused toward
    // the verb that acts on it.
    match resolve::resolve_one(universe, &parsed, resolve::KindScope::SKILLS)? {
        Some(Resolution::Resource { name, .. }) => {
            // The DEMAND-GUARD: the refusal fires only when a row (bundle or `"off"`) still
            // claims the name here. A catalog knowing the name is not a demand — with no
            // claiming row, whatever this machine retains is the classic ladder's business.
            if demanded.contains(&name) || demanded.contains(token) {
                return Err(delivered_refusal(&name));
            }
            match super::resolve_skill_here(ctx, &name, None) {
                Ok((layout, sid, lock)) => {
                    tracked_or_followed(ctx, demanded, layout, sid, lock.name)
                }
                Err(ClientError::NoSuchSkill { .. }) => {
                    untracked(ctx, demanded, roots, agent_filter, &name)
                }
                Err(e) => Err(e),
            }
        }
        Some(Resolution::Workspace { workspace_name, .. }) => {
            Err(ClientError::InvalidArgument(format!(
                "'{workspace_name}' is a workspace, not a skill — `remove` takes skills off this \
                 machine; leaving a workspace is `topos logout {workspace_name}`"
            )))
        }
        // Not a plane resource: the local paths — a tracked skill you `add`ed, or an untracked agent-dir
        // copy discovery knows.
        None => match super::resolve_skill_here(ctx, token, None) {
            Ok((layout, sid, lock)) => tracked_or_followed(ctx, demanded, layout, sid, lock.name),
            Err(ClientError::NoSuchSkill { .. }) => {
                untracked(ctx, demanded, roots, agent_filter, token)
            }
            Err(e) => Err(e),
        },
    }
}

/// The tense-matched disclosure an ORPHAN removal carries (describe vs receipt — both must be true
/// when printed).
struct OrphanNote {
    describe: String,
    applied: String,
}

/// A locally-tracked skill resolved by name. The DEMAND decides, whatever the provenance: a row
/// still claiming the name refuses toward the file that carries it — the workspace teaching when
/// the copy is followed, the machine-file teaching when it is this machine's own row. No claiming
/// row means the demand already ended — an ORPHAN, or a purely local record — and the token falls
/// through to the same describe-first permanent delete untracked copies ride, disclosed honestly.
fn tracked_or_followed(
    ctx: &Ctx<'_>,
    demanded: &HashSet<String>,
    layout: crate::sidecar::Layout,
    sid: SkillId,
    name: String,
) -> Result<Removal, ClientError> {
    let skill_id = sid.as_str().to_owned();
    let followed = super::followed_workspace(ctx, &skill_id).is_some();
    // THE DEMAND GUARD, whatever the record's provenance. A demand that still stands is removed by
    // editing the demand, never by deleting the copy: the classic arm would delete the bytes and
    // leave the row, and every later sweep for that row fails on a path that is gone. Which
    // refusal depends only on WHERE the demand lives, not on how the copy got here.
    //
    // ASK THE SCOPE THAT OWNS THE RECORD. The ladder resolved this record in a particular store,
    // and that store's own file is the one that can still be claiming it — which is not the
    // machine's file whenever the record came from a checkout, and not even the NEAREST checkout's
    // when you are standing in one nested inside it.
    if layout.is_project_scope()
        && let Some(dir) = layout.project_root()
    {
        let (claimed, file) = project_demand(ctx, dir)?;
        if claimed.contains(&name) {
            return Err(if followed {
                delivered_refusal(&name)
            } else {
                let spelled = file.as_deref().map_or_else(
                    || "that checkout's topos.toml".to_owned(),
                    |f| super::inventory::pretty(ctx, f),
                );
                standing_project_row_refusal(&name, &spelled, &super::inventory::pretty(ctx, dir))
            });
        }
    } else if demanded.contains(&name) {
        return Err(if followed {
            delivered_refusal(&name)
        } else {
            standing_row_refusal(&name)
        });
    }
    // The placement dirs come from the record's map, SPLIT by the never-deletable marker: a slot
    // carrying `adopted_source` is the folder the person named to `add`, which topos recorded
    // without ever writing into. Everything else topos materialized and may retire.
    let sp = layout.published(&sid);
    let map = doc::read_map(ctx.fs, &sp.map)?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut kept_dirs: Vec<PathBuf> = Vec::new();
    if let Some(m) = map.as_ref() {
        for (i, dir) in m.placements.iter().enumerate() {
            // A slot with NO recorded state is unproven, not proven-topos's: it falls to the
            // permanent arm exactly as it always has, because the marker is what buys the
            // exemption and an absent record buys nothing.
            if m.placement_state.get(i).is_some_and(|st| st.adopted_source) {
                kept_dirs.push(PathBuf::from(dir));
            } else {
                dirs.push(PathBuf::from(dir));
            }
        }
    }
    // The orphan's honest note. When the next update would retire the copy anyway, the describe
    // says so (doing nothing also resolves it); an adopted-in-place source dir is exactly what no
    // update deletes AND what this verb will not delete either, so both claims are withheld and
    // the record — the whole of what is actually ending — is named instead.
    let note = followed.then(|| {
        if kept_dirs.is_empty() {
            OrphanNote {
                describe: "a retained copy of an ended workspace delivery; the next `topos \
                           update` retires it anyway, so doing nothing also resolves this — \
                           applying deletes it now (record included)"
                    .to_owned(),
                applied: "a retained copy of an ended workspace delivery — deleted with its \
                          record"
                    .to_owned(),
            }
        } else {
            OrphanNote {
                describe: "a retained record of an ended workspace delivery".to_owned(),
                applied: "a retained record of an ended workspace delivery".to_owned(),
            }
        }
    });
    Ok(Removal::TrackedLocal {
        layout,
        skill_id,
        name,
        dirs,
        kept_dirs,
        note,
    })
}

/// Resolve an untracked agent-dir copy by name (optionally scoped to one agent) through the same
/// discovery `add` uses. A missing `$HOME` (no discovery) or a genuine miss is the uniform not-found.
fn untracked(
    ctx: &Ctx<'_>,
    demanded: &HashSet<String>,
    roots: Option<&DiscoveryRoots>,
    agent: Option<&str>,
    name: &str,
) -> Result<Removal, ClientError> {
    let Some(roots) = roots else {
        return Err(resolve::not_found(name));
    };
    // `<name>@<agent>` reuses `add`'s resolver (agent disambiguation + the typed ambiguity errors).
    let target = match agent {
        Some(a) => format!("{name}@{a}"),
        None => name.to_owned(),
    };
    match super::resolve_add_target(ctx, roots, &target, "remove") {
        Ok((dir, resolved)) => Ok(Removal::Untracked {
            name: resolved,
            dir,
        }),
        // The resolver's "already tracked" answer means the name IS a tracked skill — reclassify it as a
        // local delete (a bare `remove <name>` of an adopted-but-never-followed skill lands here).
        Err(ClientError::AlreadyTrackedName { .. }) => {
            match super::resolve_skill_here(ctx, name, None) {
                Ok((layout, sid, lock)) => {
                    tracked_or_followed(ctx, demanded, layout, sid, lock.name)
                }
                Err(_) => Err(resolve::not_found(name)),
            }
        }
        Err(ClientError::NoUntrackedSkill { .. }) | Err(ClientError::HarnessNotFound(_)) => {
            Err(resolve::not_found(name))
        }
        Err(e) => Err(e),
    }
}

/// The describe/apply row for one removal (the boundary a followed removal keeps vs a permanent
/// delete). `applied` picks the tense an orphan's note speaks in — each printed only when true.
fn describe_item(removal: &Removal, applied: bool) -> RemoveItem {
    match removal {
        Removal::TrackedLocal {
            name,
            dirs,
            kept_dirs,
            note,
            ..
        } => RemoveItem {
            name: name.clone(),
            // The kind IS the promise about bytes: an adopted source standing means the record
            // (and its config entries) retire while the folder keeps living where it always did.
            kind: if kept_dirs.is_empty() {
                RemoveKind::TrackedLocalPermanent
            } else {
                RemoveKind::TrackedLocalRetired
            },
            manifest: None,
            workspace_id: None,
            dest_dirs: dirs.iter().map(|d| d.display().to_string()).collect(),
            kept_dirs: kept_dirs.iter().map(|d| d.display().to_string()).collect(),
            bytes_kept: !kept_dirs.is_empty(),
            note: note.as_ref().map(|n| {
                if applied {
                    n.applied.clone()
                } else {
                    n.describe.clone()
                }
            }),
        },
        Removal::Untracked { name, dir } => RemoveItem {
            name: name.clone(),
            kind: RemoveKind::UntrackedLocal,
            manifest: None,
            workspace_id: None,
            dest_dirs: vec![dir.display().to_string()],
            kept_dirs: Vec::new(),
            bytes_kept: false,
            note: None,
        },
        Removal::Builtin { dirs } => RemoveItem {
            name: super::builtin::BUILTIN_NAME.to_owned(),
            kind: RemoveKind::BuiltinOptOut,
            manifest: None,
            workspace_id: None,
            dest_dirs: dirs.iter().map(|d| d.display().to_string()).collect(),
            kept_dirs: Vec::new(),
            bytes_kept: false,
            // The kind now carries this shape's whole sentence (`RemoveKind::BuiltinOptOut`), so
            // the note that used to smuggle it in is gone: one producer per line of copy.
            note: None,
        },
    }
}
