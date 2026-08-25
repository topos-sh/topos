//! `topos agents [add|remove] [-g] [--gitignore]` — manage the agents topos touches where you
//! stand, and the pick half of `topos init -a`.
//!
//! The pick ([`crate::agents_pick`]) is the one new state: `<project>/.topos/agents.json` for a
//! project, `<machine store>/agents.json` for the machine. `list` shows the pick in force, what is
//! installed on this machine (the ONE reader of detection beside the pick rule), and each picked
//! agent's hook state. `add` writes the pick, then [`apply_pick`] lands everything for it through
//! the ordinary engine: the scope's manifest reconcile (skills and MCP entries for the picked
//! agents), the built-in `topos` bundle at that scope, the auto-update hooks, and the optional
//! `.gitignore` lines. `remove` is the inverse, and a LOSS: it computes what leaves while the OLD
//! pick is still authoritative, describes it unless `--yes`, cleans and verifies, and writes the
//! reduced pick LAST, atomically, so a failed cleanup leaves the pick exactly as it was and the
//! receipt says what is still there.
//!
//! Each scope's file stands alone: a project starts from its own file or from nobody, never from
//! the machine's list, and the machine never reads a checkout's. A `["*"]` file is MATERIALIZED
//! into the agents installed here before a `remove` shrinks it, so what the cleanup reads and
//! what the reduced file says are one list. A named pick is explicit from then on: what the file
//! spells is what topos touches.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use topos_harness::registry::{self, KnownHarness, SkillScope};
use topos_harness::triggers::{self, TriggerScope};
use topos_types::results::{
    AgentsChanged, AgentsData, PickHook, PickReceipt, PickRemoved, PullAction,
};
use topos_types::{Message, TriggerState};

use crate::agents_pick::{self, AgentsPick, PickScope, PickSource, WILDCARD};
use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::id::SkillId;

use super::agent_hooks;
use super::agents_ask::{self, AskInputs};
use super::reconcile::{ForgeCadence, LockMode, ManifestUpdateOpts, SessionConnect, UpdateScope};

/// The `agents remove` outcome — the loss-led describe, or the applied receipt.
#[derive(Debug)]
pub(crate) enum AgentsOutcome {
    Described {
        data: AgentsChanged,
        yes_argv: Vec<String>,
    },
    Applied(AgentsChanged),
}

/// The pick the panel verbs (`status`) show: what is picked, where it comes from, which file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PickStatus {
    pub agents: Vec<String>,
    /// `"project"`, `"machine"`, or `"legacy"`; `None` with no pick.
    pub source: Option<String>,
    /// The pick file, as a person reads it; `None` with no pick or a legacy record.
    pub path: Option<String>,
}

/// The scope a verb stands in: the machine with `-g`, else the checkout whose `topos.toml` covers
/// the working directory. `None` outside any project without `-g`.
pub(crate) fn scope_here(ctx: &Ctx<'_>, global: bool) -> Option<PickScope> {
    if global {
        return Some(PickScope::Machine);
    }
    agent_hooks::cwd_project(ctx).map(PickScope::Project)
}

/// [`scope_here`] for the verbs that EDIT: outside a project, no file to edit here.
fn scope_for_edit(ctx: &Ctx<'_>, global: bool) -> Result<PickScope, ClientError> {
    scope_here(ctx, global).ok_or(ClientError::NoManifest)
}

fn scope_word(scope: &PickScope) -> &'static str {
    match scope {
        PickScope::Machine => "machine",
        PickScope::Project(_) => "project",
    }
}

fn project_dir(scope: &PickScope) -> Option<&Path> {
    match scope {
        PickScope::Machine => None,
        PickScope::Project(dir) => Some(dir.as_path()),
    }
}

fn trigger_scope(scope: &PickScope) -> TriggerScope {
    match scope {
        PickScope::Machine => TriggerScope::User,
        PickScope::Project(dir) => TriggerScope::Project(dir.clone()),
    }
}

fn skill_scope(scope: &PickScope) -> SkillScope {
    match scope {
        PickScope::Machine => SkillScope::User,
        PickScope::Project(_) => SkillScope::Project,
    }
}

/// A path as the receipt for `scope` spells it: project-relative inside the checkout, else
/// `~`-abbreviated under the home.
fn pretty_at(ctx: &Ctx<'_>, scope: &PickScope, path: &Path) -> String {
    if let PickScope::Project(dir) = scope
        && let Ok(rest) = path.strip_prefix(dir)
    {
        return rest.display().to_string();
    }
    super::inventory::pretty(ctx, path)
}

/// The slugs of every agent installed on this machine (detect dir exists), table order.
fn installed_slugs(ctx: &Ctx<'_>, scope: &PickScope) -> Vec<String> {
    let Some(roots) = &ctx.roots else {
        return Vec::new();
    };
    let cwd = project_dir(scope).or(roots.cwd.as_deref());
    registry::detected_harnesses(&roots.home, cwd)
        .iter()
        .map(|h| h.slug.to_owned())
        .collect()
}

/// A pick spelled out: the wildcard expanded to the installed agents, everything else as named.
fn expand(ctx: &Ctx<'_>, scope: &PickScope, pick: &AgentsPick) -> Vec<String> {
    if pick.is_wildcard() {
        return installed_slugs(ctx, scope);
    }
    let mut out: Vec<String> = Vec::new();
    for slug in &pick.agents {
        if !out.contains(slug) {
            out.push(slug.clone());
        }
    }
    out
}

/// The pick an edit starts from. `(pick, had_own_file)`: the scope's own file when it has one,
/// else the pick of nobody — a scope never starts from another scope's list.
fn current_pick(ctx: &Ctx<'_>, scope: &PickScope) -> Result<(AgentsPick, bool), ClientError> {
    let own = agents_pick::path_for(&ctx.layout, scope);
    match agents_pick::read(ctx.fs, &own, matches!(scope, PickScope::Machine))? {
        Some(pick) => Ok((pick, true)),
        None => Ok((AgentsPick::new(Vec::new()), false)),
    }
}

/// `current` plus `added`: the wildcard asked for is the whole pick (it stands alone); a
/// wildcard already standing is spelled out first, so the named agent joins an explicit list.
fn merged(ctx: &Ctx<'_>, scope: &PickScope, current: &AgentsPick, added: &[String]) -> AgentsPick {
    if added.iter().any(|a| a == WILDCARD) {
        return AgentsPick::everything();
    }
    let mut agents = expand(ctx, scope, current);
    for slug in added {
        if !agents.contains(slug) {
            agents.push(slug.clone());
        }
    }
    AgentsPick::new(agents)
}

/// The agents installed on this machine that `picked` leaves alone, table order.
fn untouched(ctx: &Ctx<'_>, scope: &PickScope, picked: &[&'static KnownHarness]) -> Vec<String> {
    installed_slugs(ctx, scope)
        .into_iter()
        .filter(|slug| !picked.iter().any(|h| h.slug == slug))
        .collect()
}

// =================================================================================================
// `topos agents` — the list.
// =================================================================================================

/// The pick where you stand, what is installed here, and each picked agent's hook state.
/// `gitignore` appends the picked agents' folders to the project's `.gitignore` first.
///
/// # Errors
/// [`ClientError::GitignoreNeedsProject`] for `--gitignore` with `-g` (or outside a project);
/// an unreadable pick file; the `.gitignore` write (a `.gitignore` that is a symlink out of the
/// checkout is a line on the receipt, not a refusal).
pub(crate) fn list(
    ctx: &Ctx<'_>,
    global: bool,
    gitignore: bool,
) -> Result<AgentsData, ClientError> {
    let scope = scope_here(ctx, global).unwrap_or(PickScope::Machine);
    let found = agents_pick::effective(ctx.fs, &ctx.layout, project_dir(&scope))?;
    let (pick_path, source, agents) = match &found {
        Some(e) => {
            let (path, source) = match &e.source {
                PickSource::Project(p) => (p, "project"),
                PickSource::Machine(p) => (p, "machine"),
            };
            (
                Some(pretty_at(ctx, &scope, path)),
                Some(source.to_owned()),
                e.pick.agents.clone(),
            )
        }
        None => (None, None, Vec::new()),
    };
    // A slug this binary's table does not know (a row a newer table dropped, a hand edit) is
    // shown as written and marked: it picks nothing until a table knows it again.
    let not_in_table: Vec<String> = agents
        .iter()
        .filter(|slug| !agents_pick::is_known(slug))
        .cloned()
        .collect();
    let mut warnings: Vec<Message> = Vec::new();
    let gitignored = if gitignore {
        let PickScope::Project(dir) = &scope else {
            return Err(ClientError::GitignoreNeedsProject);
        };
        let rows: Vec<&'static KnownHarness> = found
            .as_ref()
            .map(|e| resolve_at(ctx, &scope, &e.pick))
            .unwrap_or_default();
        if gitignore_within(dir) {
            gitignore_append(ctx.fs, dir, &gitignore_entries_for(&rows))?
        } else {
            warnings.push(crate::message::advisory(
                GITIGNORE_NOT_EDITED,
                GITIGNORE_SYMLINK_LINE.to_owned(),
            ));
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let evidence_doc = crate::hook_evidence::read(ctx.fs, &ctx.layout);
    let evidence = super::EvidenceView {
        agents: &evidence_doc.agents,
        now_ms: i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX),
    };
    Ok(AgentsData {
        scope: scope_word(&scope).to_owned(),
        pick_path,
        source,
        agents,
        not_in_table,
        installed: installed_slugs(ctx, &scope),
        hooks: agent_hooks::probe_effective(ctx, project_dir(&scope), &evidence),
        gitignored,
        warnings,
    })
}

/// The registry rows a pick names at `scope` (the wildcard: what is installed here).
fn resolve_at(ctx: &Ctx<'_>, scope: &PickScope, pick: &AgentsPick) -> Vec<&'static KnownHarness> {
    let Some(roots) = &ctx.roots else {
        return Vec::new();
    };
    agents_pick::resolve(
        pick,
        &roots.home,
        project_dir(scope).or(roots.cwd.as_deref()),
    )
}

// =================================================================================================
// `topos agents add` and the pick half of `init -a`.
// =================================================================================================

/// Add agents to the pick where you stand and land everything for the new pick.
///
/// # Errors
/// [`ClientError::NoManifest`] outside a project without `-g`; the slug refusals;
/// [`ClientError::GitignoreNeedsProject`]; then everything [`apply_pick`] can fail with.
pub(crate) fn add(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    global: bool,
    agents: &[String],
    gitignore: bool,
) -> Result<PickReceipt, ClientError> {
    let scope = scope_for_edit(ctx, global)?;
    if global && gitignore {
        return Err(ClientError::GitignoreNeedsProject);
    }
    agents_pick::validate(agents)?;
    let (current, _) = current_pick(ctx, &scope)?;
    let next = merged(ctx, &scope, &current, agents);
    agents_pick::write(ctx.fs, &ctx.layout, &scope, &next)?;
    apply_pick(
        ctx,
        connect,
        git,
        &scope,
        gitignore,
        "topos agents --gitignore",
    )
}

/// Set the pick `init -a` names: on a scope with a file of its own the named agents JOIN it (run
/// `init -a` again to add one); on a scope with none the named agents ARE the pick — a first
/// setup says which agents to use here, whatever the machine picks elsewhere.
///
/// # Errors
/// The slug refusals; the pick write.
pub(crate) fn set_for_init(
    ctx: &Ctx<'_>,
    scope: &PickScope,
    agents: &[String],
) -> Result<(), ClientError> {
    agents_pick::validate(agents)?;
    let (current, had_own) = current_pick(ctx, scope)?;
    let next = if had_own {
        merged(ctx, scope, &current, agents)
    } else if agents.iter().any(|a| a == WILDCARD) {
        AgentsPick::everything()
    } else {
        AgentsPick::new(agents.to_vec())
    };
    agents_pick::write(ctx.fs, &ctx.layout, scope, &next)?;
    Ok(())
}

/// Land everything for the pick in force at `scope`, through the ordinary engine: the scope's
/// manifest reconcile (skills into the picked agents' folders, MCP entries into their configs),
/// the built-in `topos` bundle at that scope, the picked agents' auto-update hooks, and (asked)
/// the `.gitignore` lines. Idempotent. `gitignore_hint` is the command the receipt offers when
/// the new files are not ignored by git.
///
/// What the reconcile could not do rides the receipt, never the floor: its failure and advisory
/// lines, the bundles it could not carry forward, and the rows it removed are carried on
/// [`PickReceipt`] (the caller exits non-zero on a failure line, as `update` does), the built-in
/// placement's refused roots join those lines, and a `.gitignore` that is a symlink out of the
/// checkout is one more line rather than an abort after the pick, the copies and the hooks
/// already landed.
///
/// # Errors
/// The reconcile's refusals (a manifest the grammar refuses, an unmatched target); the built-in
/// placement's store failures; the `.gitignore` write.
pub(crate) fn apply_pick(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    scope: &PickScope,
    gitignore: bool,
    gitignore_hint: &str,
) -> Result<PickReceipt, ClientError> {
    // Reread: the pick in force is what the file says, never what a caller remembers writing.
    let pick = agents_pick::effective(ctx.fs, &ctx.layout, project_dir(scope))?
        .map(|e| e.pick)
        .unwrap_or_else(|| AgentsPick::new(Vec::new()));
    let picked = resolve_at(ctx, scope, &pick);

    // 1. The scope's reconcile: skills and MCP entries land for the picked agents, exactly as a
    //    `topos install` would place them. Its outcome is the receipt's, not dropped.
    let out = super::reconcile::manifest_update(
        ctx,
        connect,
        git,
        &ManifestUpdateOpts {
            targets: Vec::new(),
            ack_notices: true,
            rebuild: false,
            scope: match scope {
                PickScope::Machine => UpdateScope::Machine,
                PickScope::Project(_) => UpdateScope::Here,
            },
            forge: ForgeCadence::Now,
            lock: LockMode::Install,
        },
    )?;
    let removed: Vec<PickRemoved> = out
        .data
        .skills
        .iter()
        .filter(|s| s.action == PullAction::Removed && !s.destinations.is_empty())
        .map(|s| PickRemoved {
            bundle: s.display.clone().unwrap_or_else(|| s.skill.clone()),
            destinations: s.destinations.clone(),
        })
        .collect();
    let failed_bundles = u64::try_from(out.failed_bundles.len()).unwrap_or(u64::MAX);
    let mut warnings: Vec<Message> = out.warnings;
    let mut note = |line: Message| {
        if !warnings.contains(&line) {
            warnings.push(line);
        }
    };
    for line in out.advisories {
        note(line);
    }
    // 2. The built-in bundle, at the pick's own scope. A root the containment rail refused is
    //    a placement that did not happen, said on the receipt like the reconcile's own.
    let sync = match scope {
        PickScope::Machine => super::builtin::ensure_builtin(ctx)?,
        PickScope::Project(dir) => super::builtin::ensure_builtin_in_project(ctx, dir)?,
    };
    for line in sync.refused {
        note(line);
    }
    // 3. The hooks, for exactly the picked agents at this scope.
    let mut hook_files: Vec<PickHook> = Vec::new();
    let mut hook_notes: Vec<String> = Vec::new();
    if let Some(ports) = ctx.triggers.machine_ports() {
        let slugs: Vec<&str> = picked.iter().map(|h| h.slug).collect();
        let (reports, absent) = agent_hooks::install_for(
            ports.home,
            &trigger_scope(scope),
            slugs.iter().copied(),
            ports.cfg,
            ports.run,
        );
        for r in reports {
            // The file the hook lives in, named whether or not this run wrote it (a rerun over
            // a canonical entry writes nothing and still owes the reader the path).
            let file = r.touched_path.clone().map(PathBuf::from).or_else(|| {
                triggers::adapter_for_slug_at(
                    &r.agent,
                    &trigger_scope(scope),
                    ports.home,
                    ports.cfg,
                    ports.run,
                )
                .and_then(|adapter| hook_file_of(scope, &r.agent, adapter.as_ref()))
            });
            match (r.state, file) {
                (TriggerState::Degraded, _) => {
                    hook_notes.push(format!(
                        "{}: auto-update hook not registered{}",
                        r.agent,
                        r.note.map(|n| format!(": {n}")).unwrap_or_default()
                    ));
                }
                (_, Some(file)) => hook_files.push(PickHook {
                    agent: r.agent,
                    file: pretty_at(ctx, scope, &file),
                    note: r.note,
                }),
                (_, None) => {}
            }
        }
        hook_notes.extend(absent.into_iter().map(|a| a.note));
    }
    // 4. `.gitignore`, only when asked; else the one hint line when the files are not ignored.
    //    A `.gitignore` that is a symlink out of the checkout is not edited, and says so here:
    //    the pick, the copies and the hooks above already landed, so this is a line, not a fault.
    let entries = gitignore_entries_for(&picked);
    let mut gitignored = Vec::new();
    let mut hint = None;
    if let PickScope::Project(dir) = scope {
        if gitignore {
            if gitignore_within(dir) {
                gitignored = gitignore_append(ctx.fs, dir, &entries)?;
            } else {
                note(crate::message::advisory(
                    GITIGNORE_NOT_EDITED,
                    GITIGNORE_SYMLINK_LINE.to_owned(),
                ));
            }
        } else if super::init::inside_a_git_repo(ctx.fs, dir)
            && !gitignore_missing(ctx.fs, dir, &entries).is_empty()
        {
            hint = Some(gitignore_hint.to_owned());
        }
    }
    // 5. What stands now for the pick at this scope.
    let footprint = footprint(ctx, scope, &picked)?;
    Ok(PickReceipt {
        scope: scope_word(scope).to_owned(),
        agents: picked.iter().map(|h| h.slug.to_owned()).collect(),
        pick_path: pretty_at(ctx, scope, &agents_pick::path_for(&ctx.layout, scope)),
        skills: footprint.skills,
        skills_dirs: footprint.skills_dirs,
        mcp_servers: footprint.mcp_servers,
        mcp_files: footprint.mcp_files,
        hook_files,
        hook_notes,
        untouched: untouched(ctx, scope, &picked),
        gitignore_hint: hint,
        gitignored,
        removed,
        warnings,
        failed_bundles,
    })
}

/// What stands for a pick at one scope: how many skill bundles have a copy in a picked agent's
/// folder (and which folders), how many MCP servers have an entry in a picked agent's config
/// (and which files).
#[derive(Debug, Default)]
struct Footprint {
    skills: u64,
    skills_dirs: Vec<String>,
    mcp_servers: u64,
    mcp_files: Vec<String>,
}

/// The store a scope's records live in, when it exists — a project's store is minted on first
/// write, and a checkout nothing was ever placed into has none.
fn store_of(ctx: &Ctx<'_>, scope: &PickScope) -> Option<crate::sidecar::Layout> {
    match scope {
        PickScope::Machine => Some(ctx.layout.clone()),
        PickScope::Project(dir) => crate::sidecar::existing_project_store(ctx.fs, dir),
    }
}

/// The skills root each of `rows` reads at `scope`, in `rows` order, deduplicated (a cwd-only
/// harness has none under `~`).
fn skills_roots(ctx: &Ctx<'_>, scope: &PickScope, rows: &[&'static KnownHarness]) -> Vec<PathBuf> {
    let Some(roots) = &ctx.roots else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for h in rows {
        if let Some(root) =
            registry::skills_root(h.slug, skill_scope(scope), &roots.home, project_dir(scope))
            && !out.contains(&root)
        {
            out.push(root);
        }
    }
    out
}

/// Every record in a store: the id and its placement map (records without a map are skipped).
pub(crate) fn records(
    fs: &dyn FsOps,
    layout: &crate::sidecar::Layout,
) -> Vec<(SkillId, topos_types::persisted::PlacementMap)> {
    let Ok(entries) = fs.read_dir(&layout.skills_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let Some(id) = entry
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| SkillId::parse(n).ok())
        else {
            continue;
        };
        if let Ok(Some(map)) = doc::read_map(fs, &layout.published(&id).map) {
            out.push((id, map));
        }
    }
    out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out
}

fn footprint(
    ctx: &Ctx<'_>,
    scope: &PickScope,
    picked: &[&'static KnownHarness],
) -> Result<Footprint, ClientError> {
    let Some(layout) = store_of(ctx, scope) else {
        return Ok(Footprint::default());
    };
    // Folders and files in PICK order (the order the receipt names the agents in), never the
    // alphabetical one a set would impose.
    let roots = skills_roots(ctx, scope, picked);
    let mut bundles: BTreeSet<String> = BTreeSet::new();
    let mut held: BTreeSet<PathBuf> = BTreeSet::new();
    for (id, map) in records(ctx.fs, &layout) {
        for (dir, st) in map.placements.iter().zip(&map.placement_state) {
            let dir = Path::new(dir);
            if st.materialized_sha.is_some()
                && let Some(parent) = dir.parent()
                && roots.iter().any(|r| r == parent)
            {
                bundles.insert(id.as_str().to_owned());
                held.insert(parent.to_path_buf());
            }
        }
    }
    let skills_dirs: Vec<String> = roots
        .iter()
        .filter(|r| held.contains(*r))
        .map(|r| pretty_at(ctx, scope, r))
        .collect();
    let mut servers: BTreeSet<String> = BTreeSet::new();
    let mut mcp_files: Vec<String> = Vec::new();
    if ctx.fs.exists(&layout.config_custody_path()) {
        let custody = crate::config_custody::ScopeEntries::load(ctx.fs, &layout)?;
        for h in picked {
            for (_, bundle_id, row) in custody.iter() {
                if row.agent == h.slug {
                    servers.insert(bundle_id.clone());
                    let file = pretty_at(ctx, scope, Path::new(&row.file));
                    if !mcp_files.contains(&file) {
                        mcp_files.push(file);
                    }
                }
            }
        }
    }
    Ok(Footprint {
        skills: bundles.len() as u64,
        skills_dirs,
        mcp_servers: servers.len() as u64,
        mcp_files,
    })
}

// =================================================================================================
// `topos agents remove` — the loss.
// =================================================================================================

/// What leaves with some agents at one scope, computed while the old pick is authoritative.
#[derive(Debug, Default)]
struct Loss {
    /// The skill copies to retire (display paths).
    dirs: Vec<String>,
    /// The roots the leaving agents read that STAY (another picked agent reads them).
    untouched: Vec<String>,
    /// The MCP config files holding the leaving agents' entries.
    files: Vec<String>,
    /// The hook files registered for the leaving agents.
    hooks: Vec<String>,
    /// The roots whose placements retire (absolute).
    retire_roots: Vec<PathBuf>,
    /// Every dir the leaving agents' artifacts sit in (absolute): the skills roots that retire,
    /// the MCP config files' dirs, the hook files' dirs. What
    /// [`agent_hooks::prune_emptied_dirs`] deletes afterwards IF this removal is what emptied it.
    prune_dirs: Vec<PathBuf>,
}

fn plan_loss(
    ctx: &Ctx<'_>,
    scope: &PickScope,
    leaving: &[&'static KnownHarness],
    remaining: &[&'static KnownHarness],
) -> Result<Loss, ClientError> {
    let mut loss = Loss::default();
    let leaving_roots = skills_roots(ctx, scope, leaving);
    let remaining_roots = skills_roots(ctx, scope, remaining);
    loss.untouched = leaving_roots
        .iter()
        .filter(|r| remaining_roots.contains(r))
        .map(|r| pretty_at(ctx, scope, r))
        .collect();
    loss.retire_roots = leaving_roots
        .into_iter()
        .filter(|r| !remaining_roots.contains(r))
        .collect();
    if let Some(layout) = store_of(ctx, scope) {
        for (_, map) in records(ctx.fs, &layout) {
            for (dir, st) in map.placements.iter().zip(&map.placement_state) {
                let dir = Path::new(dir);
                if st.materialized_sha.is_some()
                    && !st.adopted_source
                    && dir
                        .parent()
                        .is_some_and(|p| loss.retire_roots.iter().any(|r| r == p))
                {
                    loss.dirs.push(pretty_at(ctx, scope, dir));
                }
            }
        }
        if ctx.fs.exists(&layout.config_custody_path()) {
            let custody = crate::config_custody::ScopeEntries::load(ctx.fs, &layout)?;
            let mut files: BTreeSet<String> = BTreeSet::new();
            for (_, _, row) in custody.iter() {
                if leaving.iter().any(|h| h.slug == row.agent) {
                    let file = Path::new(&row.file);
                    files.insert(pretty_at(ctx, scope, file));
                    if let Some(dir) = file.parent() {
                        loss.prune_dirs.push(dir.to_path_buf());
                    }
                }
            }
            loss.files = files.into_iter().collect();
        }
    }
    loss.dirs.sort();
    loss.dirs.dedup();
    if let Some(ports) = ctx.triggers.machine_ports() {
        for h in leaving {
            let Some(adapter) = triggers::adapter_for_slug_at(
                h.slug,
                &trigger_scope(scope),
                ports.home,
                ports.cfg,
                ports.run,
            ) else {
                continue;
            };
            let Some(file) = hook_file_of(scope, h.slug, adapter.as_ref()) else {
                continue;
            };
            if let Some(dir) = file.parent() {
                loss.prune_dirs.push(dir.to_path_buf());
            }
            if adapter.present() {
                loss.hooks.push(pretty_at(ctx, scope, &file));
            }
        }
    }
    loss.prune_dirs.extend(loss.retire_roots.iter().cloned());
    loss.prune_dirs.sort();
    loss.prune_dirs.dedup();
    Ok(loss)
}

/// How far up a folder cleanup may walk: the checkout for a project pick, the home for a machine
/// one. Nothing at or above it is ever removed. `None` (no home known) prunes nothing at all.
fn prune_boundary(ctx: &Ctx<'_>, scope: &PickScope) -> Option<PathBuf> {
    match scope {
        PickScope::Project(dir) => Some(dir.clone()),
        PickScope::Machine => ctx.roots.as_ref().map(|r| r.home.clone()),
    }
}

/// The file a registered hook lives in: the project hook file for a project pick, the adapter's
/// own config file under `~`.
fn hook_file_of(
    scope: &PickScope,
    slug: &str,
    adapter: &dyn triggers::TriggerAdapter,
) -> Option<PathBuf> {
    match scope {
        PickScope::Project(dir) => triggers::project_hook_file(slug, dir),
        PickScope::Machine => adapter.config_file(),
    }
}

/// Remove agents from the pick where you stand: describe the loss, or (`--yes`) clean what
/// topos wrote for them, verify, and write the reduced pick last. A removal with nothing to
/// delete applies at once (no loss, no gate). A slug is checked against the PICK, never the
/// harness table: a row a newer table dropped can always be taken out (it has nothing to lose
/// here, so it applies at once).
///
/// # Errors
/// [`ClientError::NoManifest`] outside a project without `-g`; a slug not in the pick
/// ([`ClientError::InvalidArgument`]); [`ClientError::AgentRemoveIncomplete`] when the cleanup
/// left something standing (the pick is unchanged); the store, config, and pick write failures.
pub(crate) fn remove(
    ctx: &Ctx<'_>,
    global: bool,
    agents: &[String],
    yes: bool,
) -> Result<AgentsOutcome, ClientError> {
    let scope = scope_for_edit(ctx, global)?;
    if agents.iter().any(|a| a == WILDCARD) {
        return Err(ClientError::InvalidArgument(
            "name the agents to remove; `topos agents` lists the pick".into(),
        ));
    }
    let (current, _) = current_pick(ctx, &scope)?;
    let standing = expand(ctx, &scope, &current);
    let where_ = match scope {
        PickScope::Machine => "this machine's",
        PickScope::Project(_) => "this project's",
    };
    for slug in agents {
        if !standing.contains(slug) {
            return Err(ClientError::InvalidArgument(format!(
                "{slug} is not one of {where_} agents; `topos agents` lists them"
            )));
        }
    }
    let remaining: Vec<String> = standing
        .iter()
        .filter(|s| !agents.contains(s))
        .cloned()
        .collect();
    let leaving_rows = resolve_at(ctx, &scope, &AgentsPick::new(agents.to_vec()));
    let remaining_rows = resolve_at(ctx, &scope, &AgentsPick::new(remaining.clone()));
    let loss = plan_loss(ctx, &scope, &leaving_rows, &remaining_rows)?;
    let pick_path = pretty_at(ctx, &scope, &agents_pick::path_for(&ctx.layout, &scope));
    let mut data = AgentsChanged {
        scope: scope_word(&scope).to_owned(),
        // As the pick spells them (a slug the table dropped resolves to no row, and still leaves).
        removed: standing
            .iter()
            .filter(|s| agents.contains(s))
            .cloned()
            .collect(),
        agents: remaining.clone(),
        pick_path,
        applied: false,
        removed_dirs: loss.dirs.clone(),
        removed_files: loss.files.clone(),
        hooks: loss.hooks.clone(),
        untouched: loss.untouched.clone(),
        kept: Vec::new(),
    };
    let nothing_to_lose = loss.dirs.is_empty() && loss.files.is_empty() && loss.hooks.is_empty();
    if !yes && !nothing_to_lose {
        let mut yes_argv: Vec<String> = vec!["topos".into(), "agents".into(), "remove".into()];
        if global {
            yes_argv.push("-g".into());
        }
        yes_argv.extend(agents.iter().cloned());
        yes_argv.push("--yes".into());
        return Ok(AgentsOutcome::Described { data, yes_argv });
    }

    // The old pick stays authoritative through the cleanup. A scope holding the wildcard first
    // gets the SAME set spelled out in its own file, so what the cleanup reads and what the
    // reduced file will say are one list.
    if current.is_wildcard() {
        agents_pick::write(
            ctx.fs,
            &ctx.layout,
            &scope,
            &AgentsPick::new(standing.clone()),
        )?;
    }
    let mut left: Vec<String> = Vec::new();
    let mut removed_dirs: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let mut removed_files: BTreeSet<String> = BTreeSet::new();
    if let Some(layout) = store_of(ctx, &scope) {
        let sctx = super::pull::ctx_with_layout(ctx, &layout);
        // Skill copies: retire every placement under a root no remaining agent reads,
        // snapshot-first; an edited copy is kept in place and its record released.
        if !loss.retire_roots.is_empty() {
            for (id, _) in records(ctx.fs, &layout) {
                if let Some(clean) =
                    super::reconcile::clean_dest_roots(&sctx, &id, &loss.retire_roots)?
                {
                    // The clean spells its paths for the machine (`~`-abbreviated); this
                    // receipt spells them for the scope.
                    removed_dirs.extend(
                        clean
                            .removed
                            .iter()
                            .map(|d| pretty_at(ctx, &scope, Path::new(d))),
                    );
                    kept.extend(
                        clean
                            .kept
                            .iter()
                            .map(|d| pretty_at(ctx, &scope, Path::new(d))),
                    );
                }
            }
            for (_, map) in records(ctx.fs, &layout) {
                for (dir, st) in map.placements.iter().zip(&map.placement_state) {
                    let dir = Path::new(dir);
                    if st.materialized_sha.is_some()
                        && !st.adopted_source
                        && dir
                            .parent()
                            .is_some_and(|p| loss.retire_roots.iter().any(|r| r == p))
                        && ctx.fs.exists(dir)
                    {
                        left.push(pretty_at(ctx, &scope, dir));
                    }
                }
            }
        }
        // MCP entries: each bundle's entries leave the leaving agents' configs and nothing else's
        // (the engine's removal, narrowed to those agents' surfaces); a hand-edited entry is
        // left in place and disclosed as kept.
        if let Some(roots) = &ctx.roots
            && ctx.fs.exists(&layout.config_custody_path())
        {
            let descriptors: Vec<&'static KnownHarness> = leaving_rows
                .iter()
                .copied()
                .filter(|h| h.mcp().is_some())
                .collect();
            let leaving_set: BTreeSet<String> =
                descriptors.iter().map(|h| h.slug.to_owned()).collect();
            let io = crate::mcp_engine::ScopeIo {
                fs: ctx.fs,
                runtimes: &crate::mcp_render::PathRuntimes,
                relay_program: crate::mcp_engine::relay_program(),
                layout: &layout,
                home: roots.home.clone(),
                project_root: project_dir(&scope).map(Path::to_path_buf),
            };
            let custody = crate::config_custody::ScopeEntries::load(ctx.fs, &layout)?;
            let bundles: BTreeSet<String> = custody
                .iter()
                .filter(|(_, _, row)| leaving_set.contains(&row.agent))
                .map(|(_, bundle_id, _)| bundle_id.clone())
                .collect();
            let mut drifted: BTreeSet<String> = BTreeSet::new();
            for bundle_id in &bundles {
                let name = SkillId::parse(bundle_id)
                    .ok()
                    .and_then(|sid| {
                        doc::read_doc::<topos_types::persisted::Lock>(
                            ctx.fs,
                            &layout.published(&sid).lock,
                        )
                        .ok()
                        .flatten()
                    })
                    .map_or_else(|| bundle_id.clone(), |lock| lock.name);
                let outcome = crate::mcp_engine::remove_bundle(
                    &io,
                    &descriptors,
                    &leaving_set,
                    bundle_id,
                    &name,
                );
                for entry in &outcome.removed {
                    let Some(file) = entry.state.file.as_deref() else {
                        continue;
                    };
                    let file = pretty_at(ctx, &scope, Path::new(file));
                    match entry.state.state {
                        topos_types::results::TargetOutcome::Drifted => {
                            drifted.insert(file);
                        }
                        _ => {
                            removed_files.insert(file);
                        }
                    }
                }
                for w in &outcome.warnings {
                    left.push(w.text.clone());
                }
            }
            let custody = crate::config_custody::ScopeEntries::load(ctx.fs, &layout)?;
            for (_, _, row) in custody.iter() {
                if leaving_set.contains(&row.agent) {
                    let file = pretty_at(ctx, &scope, Path::new(&row.file));
                    if !drifted.contains(&file) {
                        left.push(format!("an entry in {file}"));
                    }
                }
            }
            kept.extend(
                drifted
                    .into_iter()
                    .map(|f| format!("{f} (an entry you edited by hand)")),
            );
        }
    }
    // The hooks.
    let mut removed_hooks: Vec<String> = Vec::new();
    if let Some(ports) = ctx.triggers.machine_ports() {
        let slugs: Vec<&str> = leaving_rows.iter().map(|h| h.slug).collect();
        let reports = agent_hooks::remove_for(
            ports.home,
            &trigger_scope(&scope),
            slugs.iter().copied(),
            ports.cfg,
            ports.run,
        );
        for r in reports {
            // A degraded scrub is an INCOMPLETE cleanup: the entry may still be live in a file
            // topos could not read, parse, or reach through the rail (`present()` fails closed
            // there, so the check below would say nothing). The receipt names the file and the
            // reason, and the pick stays.
            if r.state == TriggerState::Degraded {
                let file = triggers::adapter_for_slug_at(
                    &r.agent,
                    &trigger_scope(&scope),
                    ports.home,
                    ports.cfg,
                    ports.run,
                )
                .and_then(|adapter| hook_file_of(&scope, &r.agent, adapter.as_ref()))
                .map_or_else(
                    || format!("{}'s auto-update hook", r.agent),
                    |file| pretty_at(ctx, &scope, &file),
                );
                left.push(format!(
                    "{file} ({})",
                    r.note
                        .as_deref()
                        .unwrap_or("topos could not edit that file")
                ));
                continue;
            }
            if let Some(path) = r.touched_path {
                removed_hooks.push(pretty_at(ctx, &scope, Path::new(&path)));
            }
        }
        for h in &leaving_rows {
            if let Some(adapter) = triggers::adapter_for_slug_at(
                h.slug,
                &trigger_scope(&scope),
                ports.home,
                ports.cfg,
                ports.run,
            ) && adapter.present()
                && let Some(file) = hook_file_of(&scope, h.slug, adapter.as_ref())
            {
                left.push(pretty_at(ctx, &scope, &file));
            }
        }
    }
    left.sort();
    left.dedup();
    if !left.is_empty() {
        return Err(ClientError::AgentRemoveIncomplete {
            agent: agents.join(", "),
            left,
        });
    }
    // Everything topos wrote for those agents is gone. What can still be standing is the shape it
    // wrote them INTO — a `.cursor/skills/` with nothing left in it, and a `.cursor/` that did not
    // exist before topos put a hook there. Removing an agent leaves no folder topos made.
    if let Some(boundary) = prune_boundary(ctx, &scope) {
        agent_hooks::prune_emptied_dirs(ctx.fs, &boundary, &loss.prune_dirs);
    }
    // LAST: the reduced pick, atomically.
    agents_pick::write(ctx.fs, &ctx.layout, &scope, &AgentsPick::new(remaining))?;
    removed_dirs.sort();
    removed_dirs.dedup();
    kept.sort();
    kept.dedup();
    data.applied = true;
    data.removed_dirs = removed_dirs;
    data.removed_files = removed_files.into_iter().collect();
    data.hooks = if removed_hooks.is_empty() {
        loss.hooks
    } else {
        removed_hooks
    };
    data.kept = kept;
    Ok(AgentsOutcome::Applied(data))
}

// =================================================================================================
// The pick a reconcile needs — `install` / `update` with none.
// =================================================================================================

/// What the pick rule did at a scope that had no pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickDerived {
    /// A pick already stood here. Nothing was recorded, nothing to say.
    Stood,
    /// Recorded this run — the effective pick, for the caller to say.
    Recorded(Vec<String>),
    /// No agent topos knows is installed here. Nothing was recorded (a pick naming nobody is a
    /// decision nobody made), the verb goes on and places nothing, and the caller says so.
    NoneInstalled,
    /// Several agents are installed and this run may not ask which (`--frozen`). Nothing was
    /// recorded and nothing is refused: the run goes on, places nothing, and the caller says so.
    NotPicked,
}

/// The sentence a scope with no installed agent earns, said once per run: nothing was picked,
/// so nothing was placed. ONE string wherever it is said — stderr for a verb whose receipt has
/// no line for it, the `init` receipt's own first line, [`no_agent_installed_line`] on `--json`.
pub(crate) const NO_AGENT_INSTALLED: &str =
    "No agent topos knows is installed here; nothing placed.";

/// [`NO_AGENT_INSTALLED`] as the machine-readable line a `--json` envelope carries.
pub(crate) fn no_agent_installed_line() -> Message {
    crate::message::disclosure("NO_AGENT_INSTALLED", NO_AGENT_INSTALLED.to_owned())
}

/// The sentence a `--frozen` run earns where several agents are installed and none is picked:
/// what happened, then the command that changes it. A frozen run may not ask, so this is the
/// whole answer — and it is a statement, not a refusal, because a build box has nothing to
/// answer with (the pick is personal and never in git).
pub(crate) fn no_pick(global: bool) -> String {
    format!(
        "No agents picked here; nothing placed. Pick with: {}",
        crate::error::pick_command(global)
    )
}

/// [`no_pick`] as the machine-readable line a `--json` envelope carries.
pub(crate) fn no_pick_line(global: bool) -> Message {
    crate::message::disclosure("NO_PICK", no_pick(global))
}

/// Before a verb lands anything at `scope` with no pick OF ITS OWN (`install`, `update`, `init`,
/// every `add`): one agent installed here is the pick (recorded at that scope, then reread, so
/// the converge reads what the file says, and the caller says so); several installed and none
/// picked is [`ClientError::PickRequired`], which names them and the command; none at all is
/// [`PickDerived::NoneInstalled`] — not a question, not a refusal, and no file written. A
/// `--frozen` run never reaches the refusal: several installed agents are
/// [`PickDerived::NotPicked`], on the same terms. The quiet hook sweep never calls this: it
/// derives nothing and places nothing for a scope with no pick.
///
/// The scope decides which pick file counts and which way out is spelled: a project's own file
/// for a project, the machine file for `-g`, never the other.
///
/// # Errors
/// [`ClientError::PickRequired`]; an unreadable pick file; the pick write.
pub(crate) fn derive_pick_if_missing(
    ctx: &Ctx<'_>,
    scope: &PickScope,
    ask: &AskInputs,
) -> Result<PickDerived, ClientError> {
    if agents_pick::effective(ctx.fs, &ctx.layout, project_dir(scope))?.is_some() {
        return Ok(PickDerived::Stood);
    }
    let installed = installed_slugs(ctx, scope);
    let global = matches!(scope, PickScope::Machine);
    // FROZEN never refuses and never guesses. One installed agent is not a question and stays the
    // pick; an ambiguous machine gets no pick at all, because the answer would otherwise depend
    // on which runner the pipeline landed on.
    if ask.frozen && installed.len() > 1 {
        return Ok(PickDerived::NotPicked);
    }
    let chosen = agents_ask::choose(&installed, ask, global)?;
    if chosen.is_empty() {
        return Ok(PickDerived::NoneInstalled);
    }
    agents_pick::write(ctx.fs, &ctx.layout, scope, &AgentsPick::new(chosen.clone()))?;
    let effective = agents_pick::effective(ctx.fs, &ctx.layout, project_dir(scope))?
        .map_or(chosen, |e| e.pick.agents);
    Ok(PickDerived::Recorded(effective))
}

/// The pick the `status` panel shows for the scope `project_dir` stands for — that scope's own
/// file, never another's. `status` never seeds, so a MACHINE that predates the pick still shows
/// what its legacy record holds; a project shows its own file or nothing, because the legacy
/// record is the machine's and never governed a checkout.
pub(crate) fn status_pick(ctx: &Ctx<'_>, project_dir: Option<&Path>) -> PickStatus {
    match agents_pick::effective(ctx.fs, &ctx.layout, project_dir) {
        Ok(Some(e)) => {
            let (path, source) = match &e.source {
                PickSource::Project(p) => (p, "project"),
                PickSource::Machine(p) => (p, "machine"),
            };
            let scope =
                project_dir.map_or(PickScope::Machine, |d| PickScope::Project(d.to_path_buf()));
            PickStatus {
                agents: e.pick.agents,
                source: Some(source.to_owned()),
                path: Some(pretty_at(ctx, &scope, path)),
            }
        }
        // The legacy record is the MACHINE's, so it answers only the machine scope. A project
        // with no file of its own says so instead of showing a list that never governed here.
        Ok(None) if project_dir.is_none() => {
            let legacy = agents_pick::legacy_registered(ctx.fs, &ctx.layout);
            if legacy.is_empty() {
                PickStatus::default()
            } else {
                PickStatus {
                    agents: legacy,
                    source: Some("legacy".to_owned()),
                    path: None,
                }
            }
        }
        Ok(None) => PickStatus::default(),
        Err(_) => PickStatus::default(),
    }
}

// =================================================================================================
// `--gitignore`.
// =================================================================================================

/// The `.gitignore` lines for one agent's per-project folders: the whole folders topos writes
/// into (skills, hook, MCP config), never a file inside them.
pub(crate) fn gitignore_entries(slug: &str) -> Vec<String> {
    let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
    match slug {
        // Claude Code's MCP entries live in its own machine file now, so the only thing topos
        // writes into this checkout for it is `.claude/`. A `.mcp.json` is written only where a
        // row's own `dest` names it — and a committed one is the whole point of naming it, so it
        // is never ignored on topos's say-so.
        "claude-code" => owned(&[".claude/"]),
        "cursor" => owned(&[".cursor/"]),
        other => {
            let Some(h) = registry::known_harness(other) else {
                return Vec::new();
            };
            let mut out: Vec<String> = Vec::new();
            if let Some(first) = h.project_dir().split('/').next()
                && !first.is_empty()
            {
                out.push(format!("{first}/"));
            }
            // Only an IN-CHECKOUT project surface earns a line: a machine file this scope writes
            // puts nothing in the repo to ignore.
            if let Some(registry::McpProjectLoc::InCheckout(path)) =
                h.mcp().and_then(|m| m.project).map(|p| p.loc)
            {
                let entry = match path.split_once('/') {
                    Some((first, _)) => format!("{first}/"),
                    None => path.to_owned(),
                };
                if !out.contains(&entry) {
                    out.push(entry);
                }
            }
            out
        }
    }
}

/// [`gitignore_entries`] over a pick, deduplicated in pick order.
fn gitignore_entries_for(rows: &[&'static KnownHarness]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for h in rows {
        for entry in gitignore_entries(h.slug) {
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out
}

fn gitignore_lines(fs: &dyn FsOps, path: &Path) -> Vec<String> {
    fs.read_opt(path)
        .ok()
        .flatten()
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(|l| l.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The entries `.gitignore` does not hold yet (a line is matched whole, trimmed).
fn gitignore_missing(fs: &dyn FsOps, project_dir: &Path, entries: &[String]) -> Vec<String> {
    let standing = gitignore_lines(fs, &project_dir.join(".gitignore"));
    entries
        .iter()
        .filter(|e| !standing.iter().any(|l| l == *e))
        .cloned()
        .collect()
}

/// The receipt line for a `.gitignore` that is a symlink out of the checkout, and its code.
const GITIGNORE_SYMLINK_LINE: &str = "`.gitignore` is a symlink out of this checkout; not edited";
const GITIGNORE_NOT_EDITED: &str = "GITIGNORE_NOT_EDITED";

/// Whether the checkout's `.gitignore` resolves inside it (absent counts: it would be created
/// there). A symlink out of the checkout is never written through.
fn gitignore_within(project_dir: &Path) -> bool {
    crate::placement::within_project(project_dir, &project_dir.join(".gitignore"))
}

/// Append the entries `.gitignore` does not hold yet, one per line, creating the file when
/// absent. Idempotent: a second run with the same entries writes nothing. Never touches
/// `.git/info/exclude`, never edits an existing line. Returns what was appended.
///
/// # Errors
/// A `.gitignore` that does not resolve inside the checkout (a symlink out of it) refuses with
/// [`ClientError::PlacementUnsupported`] carrying [`GITIGNORE_SYMLINK_LINE`]; the write failure.
pub(crate) fn gitignore_append(
    fs: &dyn FsOps,
    project_dir: &Path,
    entries: &[String],
) -> Result<Vec<String>, ClientError> {
    let path = project_dir.join(".gitignore");
    if !gitignore_within(project_dir) {
        return Err(ClientError::PlacementUnsupported {
            reason: GITIGNORE_SYMLINK_LINE.to_owned(),
        });
    }
    let missing = gitignore_missing(fs, project_dir, entries);
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let mut text = fs
        .read_opt(&path)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for entry in &missing {
        text.push_str(entry);
        text.push('\n');
    }
    crate::atomic::atomic_write(fs, &path, text.as_bytes())?;
    Ok(missing)
}

/// The known-slug map the receipts spell display names from, in one place.
pub(crate) fn display_names(slugs: &[String]) -> Vec<String> {
    slugs
        .iter()
        .map(|slug| {
            registry::known_harness(slug)
                .map_or_else(|| slug.clone(), |h| h.display_name.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-agents-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap_or(dir)
    }

    #[test]
    fn gitignore_append_is_idempotent() {
        let dir = scratch("gitignore");
        let fs = RealFs;
        let entries: Vec<String> = [".claude/", ".mcp.json", ".codex/", ".agents/"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        // Created when absent, every entry on its own line.
        let added = gitignore_append(&fs, &dir, &entries).unwrap();
        assert_eq!(added, entries);
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text, ".claude/\n.mcp.json\n.codex/\n.agents/\n");
        // A second run appends nothing.
        assert!(gitignore_append(&fs, &dir, &entries).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            text
        );
        // An existing file keeps its lines (a missing trailing newline is supplied), and only the
        // entries it lacks are appended.
        std::fs::write(dir.join(".gitignore"), "node_modules\n.codex/").unwrap();
        let added = gitignore_append(&fs, &dir, &entries).unwrap();
        assert_eq!(added, [".claude/", ".mcp.json", ".agents/"]);
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "node_modules\n.codex/\n.claude/\n.mcp.json\n.agents/\n"
        );
        assert!(
            !dir.join(".git").exists(),
            "nothing under .git is ever touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gitignore_table_names_whole_folders_per_agent() {
        assert_eq!(gitignore_entries("claude-code"), [".claude/"]);
        assert_eq!(gitignore_entries("cursor"), [".cursor/"]);
        // Codex and OpenCode have no arm of their own: skills, hook and MCP config all sit under
        // the agent's one folder, which is exactly what the generic rule below derives.
        assert_eq!(gitignore_entries("codex"), [".codex/"]);
        assert_eq!(gitignore_entries("opencode"), [".opencode/"]);
        // Every other row: the first segment of its project skills dir, plus its project MCP
        // file's folder (or the file itself at the root).
        assert_eq!(gitignore_entries("gemini-cli"), [".agents/", ".gemini/"]);
        assert!(gitignore_entries("not-a-harness").is_empty());
        let rows: Vec<&'static KnownHarness> = ["claude-code", "codex", "gemini-cli"]
            .iter()
            .filter_map(|s| registry::known_harness(s))
            .collect();
        assert_eq!(
            gitignore_entries_for(&rows),
            [".claude/", ".codex/", ".agents/", ".gemini/"],
            "deduplicated, pick order"
        );
    }

    #[test]
    fn display_names_read_the_table() {
        assert_eq!(
            display_names(&["claude-code".to_owned(), "codex".to_owned()]),
            ["Claude Code", "Codex"]
        );
    }
}
