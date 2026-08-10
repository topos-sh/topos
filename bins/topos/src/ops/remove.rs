//! `remove [SKILL]...` — the CLASSIC removal: bytes this machine holds that NO manifest row asks
//! for. The manifest arms run first (see [`super::manifest_edit`]); everything they do not claim
//! lands here, and every shape here is a PERMANENT delete, so all of them are describe-first:
//!
//! - a TRACKED, never-published LOCAL skill → the agent dirs AND the sidecar entry go (no other
//!   copy exists).
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
    TrackedLocal {
        skill_id: String,
        name: String,
        dirs: Vec<PathBuf>,
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

    // The gate: a followed CLEAN skill is a reversible per-device act — it applies immediately
    // (`--yes` an accepted no-op). Everything else keeps the two-phase describe: a permanent
    // delete (local-only / untracked / the built-in opt-out) destroys the only copy, and the
    // LOSS-GUARD holds a followed skill with a draft ahead — the apply cleans the draft out of
    // every agent dir (snapshot-first into the sidecar, but out of the working copies), so the
    // disclosure comes first. A scan that cannot classify FAILS TOWARD THE GATE — a stale or
    // unreadable copy must never lose a draft to an optimistic apply. One gated target gates the
    // whole batch (all-or-none, like the resolution).
    let mut gated = false;
    // The followed removals whose PRE-apply state the `follow` inverse could not restore: a
    // draft the apply cleans out of the working dirs (the inverse reinstalls only canonical
    // bytes), an unscannable copy, or a FOREIGN recorded placement (the clean drops the
    // reservation but leaves the occupied dir — the inverse re-plans around it, never restoring
    // the prior record) — their receipts offer no undo.
    for (removal, item) in removals.iter().zip(items.iter_mut()) {
        match removal {
            // A config-placed bundle's blast radius is not its dirs — the `--yes` gate must name
            // the agent configs the apply will edit BEFORE consent, not only on the receipt after
            // it. The list is knowable now: the scope ledger records every entry topos placed.
            Removal::TrackedLocal { skill_id, .. } => {
                gated = true;
                let files = mcp_entry_files(ctx, skill_id);
                if let Some(line) = also_removes_line(&files) {
                    item.note = Some(match item.note.take() {
                        Some(prev) => format!("{prev} · {line}"),
                        None => line,
                    });
                }
            }
            Removal::Untracked { .. } | Removal::Builtin { .. } => {
                gated = true;
            }
        }
    }

    if gated && !yes {
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

    // ---- APPLY (immediate for followed clean skills; `--yes` for the gated shapes) ----
    // The UNGATED path re-checks the loss-guard at the apply boundary: an edit landing between
    // the classification above and this point must not slip through the gate it would have held.
    // A residual window remains between this recheck and `snapshot_and_clean` acquiring the skill
    // lock — an edit racing into those milliseconds is cleaned WITHOUT the describe, but never
    // lost: the snapshot-first clean retains every distinct edited copy in the sidecar store
    // under the lock before a byte leaves a dir. Closing the window whole would need the gate
    // decision inside the shared lock (a `snapshot_and_clean` contract change shared with the
    // withdrawal sweep) — deliberately not taken for a consent-courtesy race with no byte loss.
    // The PRE-apply stances — the undo below is withheld from any skill whose LOCAL entry shows
    // a standing stance going in: a repeat remove of an already-excluded skill is a no-op whose
    // "undo" would change pre-existing state, and a remove of an UNFOLLOWED skill's frozen copy
    // must not offer a `follow` that would clear the person's unfollow stance too. A followed
    // skill with NO local entry (resolved through the universe — followed on the web, never
    // received here) is likewise ineligible: after this exclusion the delivery reports it
    // excluded, not detached, so the advertised `follow` would answer a first-trust DESCRIBE
    // instead of the immediate re-attach only a local marker routes to.
    // The APPLIED items re-derive so an orphan's note speaks in the right tense (the describe's
    // "doing nothing also resolves this" would be false on a receipt for an act just performed).
    let mut items: Vec<RemoveItem> = removals.iter().map(|r| describe_item(r, true)).collect();
    for (removal, item) in removals.iter().zip(items.iter_mut()) {
        match removal {
            Removal::TrackedLocal { skill_id, dirs, .. } => {
                // A config-placed bundle's reach is not these dirs — it is the entries it wrote
                // into agents' MCP configs. They go FIRST, while the record that names them still
                // exists: retired afterwards there would be nothing left to prove which entries
                // were ever this bundle's, and they would sit in those files forever.
                retire_mcp_entries(ctx, skill_id, item);
                for dir in dirs {
                    if ctx.fs.exists(dir) {
                        ctx.fs.remove_dir_all(dir)?;
                    }
                }
                // Drop the sidecar entry — a never-published local has no other copy.
                let sid = SkillId::parse(skill_id)?;
                let skill_dir = ctx.layout.skill_dir(&sid);
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
    // The literal inverse, offered ONLY when it restores the whole prior state: every removal a
    // followed exclusion (a permanent delete has no inverse — the batch omits the undo rather
    // than misstating a partial one), every exclusion flipped from a locally ACTIVE follow (a
    // repeat remove, a stanced entry, or a web-followed skill with no local entry — whose
    // `follow` would describe first-trust, not re-attach — all withhold), every pre-apply copy
    // CLEAN (a consented
    // draft removal cleans working edits the inverse would not reinstall — the snapshot keeps
    // them recoverable, but recovery is not this one command), and one workspace (`follow` takes
    // one per invocation). Targets ride QUALIFIED (`<ws>/skills/<name>`) when the address slug is
    // known offline — a name followed in a second workspace would make the bare spelling an
    // ambiguous refusal instead of the promised undo.
    // A local delete is permanent — there is no one-command inverse to advertise.
    let undo: Vec<String> = Vec::new();
    Ok(RemoveOutcome::Applied(RemoveData {
        items,
        applied: true,
        undo,
        uninstalled: Vec::new(),
    }))
}

/// The config files this scope's ledger records standing entries for, `~`-abbreviated, in ledger
/// order and deduped — what the describe names before consent and what the apply will edit. Empty
/// for an ordinary skill record (no ledger entries) and for a scope that never config-placed.
fn mcp_entry_files(ctx: &Ctx<'_>, skill_id: &str) -> Vec<String> {
    if !ctx.fs.exists(&ctx.layout.mcp_ledger_path()) {
        return Vec::new();
    }
    let Ok(ledger) = crate::mcp_ledger::read(ctx.fs, &ctx.layout) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for entry in ledger.entries.values().filter(|e| e.bundle_id == skill_id) {
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
fn retire_mcp_entries(ctx: &Ctx<'_>, skill_id: &str, item: &mut RemoveItem) {
    let Some(roots) = ctx.roots.clone() else {
        return;
    };
    if !ctx.fs.exists(&ctx.layout.mcp_ledger_path()) {
        return; // nothing was ever config-placed in this scope
    }
    let Ok(sid) = SkillId::parse(skill_id) else {
        return;
    };
    // Classification does not need the placement map: the durable marker answers first, and an
    // unreadable map only costs the manifest-row rung below it.
    let placements = doc::read_map(ctx.fs, &ctx.layout.published(&sid).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    if !crate::bundle_kind::classify(ctx, skill_id, &placements).is_mcp() {
        return;
    }
    let project_root = ctx.layout.project_root().map(std::path::Path::to_path_buf);
    let detected: std::collections::BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &roots.home,
        project_root.as_deref().or(roots.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    let io = crate::mcp_engine::ScopeIo {
        fs: ctx.fs,
        layout: &ctx.layout,
        home: roots.home.clone(),
        project_root,
    };
    let outcome = crate::mcp_engine::remove_bundle(
        &io,
        topos_harness::mcp::descriptor::mcp_harnesses(),
        &detected,
        skill_id,
    );
    // Keyed by the config FILE the entry lived in — receipts speak in destinations, never agents.
    let mut lines: Vec<String> = outcome
        .removed
        .iter()
        .map(|removed| {
            let file = removed.state.file.as_deref().map_or_else(
                || "its config".to_owned(),
                |f| super::inventory::pretty(ctx, std::path::Path::new(f)),
            );
            match removed.state.state.as_str() {
                "drifted" => format!("{file}: hand-edited entry left in place"),
                _ => format!("{file}: server entry removed"),
            }
        })
        .collect();
    lines.extend(outcome.warnings.iter().cloned());
    if lines.is_empty() {
        return;
    }
    let folded = lines.join(" · ");
    item.note = Some(match item.note.take() {
        Some(prev) => format!("{prev} · {folded}"),
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

fn delivered_refusal(name: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "'{name}' is delivered from a workspace — remove the DEMAND, not the copy: `topos \
         remove {name}` drops this folder's line for it; `topos remove -g {name}` edits your \
         machine-wide file (switching it off here). What the workspace assigns you is managed \
         on the web."
    ))
}

/// Every name (and reference) the MACHINE scope still demands — the rows (bundle or `"off"`) of
/// the same offline resolution `list` and `status` render. This is the demand-guard's key: the
/// classic ladder only ever deletes HOME-store records, and a workspace-provenance record whose
/// name none of these rows claim is an ORPHAN — its demand already ended, so refusing toward a row
/// that does not exist would be false. Offline by construction (no dial).
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
            match super::resolve_skill(ctx, &name) {
                Ok((sid, lock)) => tracked_or_followed(ctx, demanded, sid, lock.name),
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
        None => match super::resolve_skill(ctx, token) {
            Ok((sid, lock)) => tracked_or_followed(ctx, demanded, sid, lock.name),
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
    sid: SkillId,
    name: String,
) -> Result<Removal, ClientError> {
    let skill_id = sid.as_str().to_owned();
    let followed = super::followed_workspace(ctx, &skill_id).is_some();
    // THE DEMAND GUARD, whatever the record's provenance. A demand that still stands is removed by
    // editing the demand, never by deleting the copy: the classic arm would delete the bytes and
    // leave the row, and every later sweep for that row fails on a path that is gone. Which
    // refusal depends only on WHERE the demand lives, not on how the copy got here.
    if demanded.contains(&name) {
        return Err(if followed {
            delivered_refusal(&name)
        } else {
            standing_row_refusal(&name)
        });
    }
    // The placement dirs to delete come from the record's map.
    let sp = ctx.layout.published(&sid);
    let map = doc::read_map(ctx.fs, &sp.map)?;
    let dirs: Vec<PathBuf> = map
        .as_ref()
        .map(|m| m.placements.iter().map(PathBuf::from).collect())
        .unwrap_or_default();
    // The orphan's honest note. When the next update would retire the copy anyway, the describe
    // says so (doing nothing also resolves it); an adopted-in-place source dir is exactly what no
    // update deletes, so that claim is withheld and the explicit-remove boundary named instead.
    let note = followed.then(|| {
        let adopted = map
            .as_ref()
            .is_some_and(|m| m.placement_state.iter().any(|s| s.adopted_source));
        if adopted {
            OrphanNote {
                describe: "a retained copy of an ended workspace delivery; its dir was adopted \
                           in place, so no sweep deletes it — only an explicit remove does \
                           (record included)"
                    .to_owned(),
                applied: "a retained copy of an ended workspace delivery — deleted with its \
                          record"
                    .to_owned(),
            }
        } else {
            OrphanNote {
                describe: "a retained copy of an ended workspace delivery; the next `topos \
                           update` retires it anyway, so doing nothing also resolves this — \
                           applying deletes it now (record included)"
                    .to_owned(),
                applied: "a retained copy of an ended workspace delivery — deleted with its \
                          record"
                    .to_owned(),
            }
        }
    });
    Ok(Removal::TrackedLocal {
        skill_id,
        name,
        dirs,
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
    match super::resolve_add_target(ctx, roots, &target) {
        Ok((dir, resolved)) => Ok(Removal::Untracked {
            name: resolved,
            dir,
        }),
        // The resolver's "already tracked" answer means the name IS a tracked skill — reclassify it as a
        // local delete (a bare `remove <name>` of an adopted-but-never-followed skill lands here).
        Err(ClientError::AlreadyTrackedName { .. }) => match super::resolve_skill(ctx, name) {
            Ok((sid, lock)) => tracked_or_followed(ctx, demanded, sid, lock.name),
            Err(_) => Err(resolve::not_found(name)),
        },
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
            name, dirs, note, ..
        } => RemoveItem {
            name: name.clone(),
            kind: RemoveKind::TrackedLocalPermanent,
            manifest: None,
            workspace_id: None,
            agent_dirs: dirs.iter().map(|d| d.display().to_string()).collect(),
            bytes_kept: false,
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
            agent_dirs: vec![dir.display().to_string()],
            bytes_kept: false,
            note: None,
        },
        Removal::Builtin { dirs } => RemoveItem {
            name: super::builtin::BUILTIN_NAME.to_owned(),
            kind: RemoveKind::TrackedLocalPermanent,
            manifest: None,
            workspace_id: None,
            agent_dirs: dirs.iter().map(|d| d.display().to_string()).collect(),
            bytes_kept: false,
            note: Some(
                "the built-in topos skill — the opt-out is durable (no sweep re-places it); \
                 `topos add topos` brings it back"
                    .to_owned(),
            ),
        },
    }
}
