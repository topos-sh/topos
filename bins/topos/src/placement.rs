//! The **placement engine** — WHERE a followed bundle's bytes land on this machine, computed from
//! the machine alone: which agents are detected, and which of them read the shared
//! `~/.agents/skills` convention dir. A row that names its own destinations bypasses detection
//! entirely ([`dest_plan`]).
//!
//! ## Two target shapes, one plan
//!
//! A [`PlacementPlan`] carries [`PlannedTarget`]s of two shapes, and a bundle's kind picks which
//! arm plans: a **DIRECTORY** this bundle owns ([`DirTarget`] — the dir planners below, written by
//! [`crate::materialize`]), or **ENTRIES** it owns inside a config file shared with everything else
//! on the machine ([`EntriesTarget`] — [`entries_plan`], applied by [`crate::mcp_engine`]'s
//! per-scope converge, one write per file carrying every bundle's entries at once). Nothing forces
//! one writer; what the two shapes share is the plan, and what the plan means: **WHAT SHOULD
//! STAND**. What topos actually put there is CUSTODY, and it lives in the bundle's own record.
//!
//! ## The policy: shared-dir-first
//!
//! A skill lands ONE copy in the shared cross-agent dir when at least one detected harness is
//! covered by it ([`topos_harness::coverage`]), PLUS one native copy per detected harness the shared
//! dir does NOT cover. With no harness detected at all (or no machine roots injected), the classic
//! behavior holds: the active harness's single placement.
//!
//! **Every target dir comes from the harness's registry ROW**, resolved through the one resolver —
//! the active harness's exactly like every other detected one ([`adapter_choice`]). Detection
//! already follows a machine-local table that moved an agent's skills dir; a placement that asked
//! the compiled adapter instead would keep writing where the agent no longer reads. The
//! [`topos_harness::HarnessAdapter`] answers for the rest of its own behavior, and for the dir only
//! where no row can resolve (no machine roots at all).
//!
//! ## Target-set reconciliation
//!
//! Targets are recomputed each sync. A NEW target (a newly detected harness, newly true coverage) is
//! APPENDED to the map with no materialized bytes yet and lands on the next apply. A placement
//! LEAVES the record only through an explicit verb (a `remove`, a `dest` edit), which cleans its dir
//! snapshot-first; detection loss alone never deletes a byte — the recorded copy freezes in place,
//! unmanaged (skipped by the apply, kept on disk).
//!
//! ## Naming + never-clobber
//!
//! Every new target dir is named by the ONE discipline the reference adapter uses
//! ([`topos_harness::choose_skill_dir`]): the sanitized display name, workspace-suffixed on a
//! collision (`<name>-<ws>`), the validated id as the last resort — and only a FREE dir or one this
//! skill's own placement record already owns is ever chosen. An already-recorded (kind, agent)
//! target keeps its dir verbatim (stability comes from the record, not from re-derivation). One
//! CLI-side refinement on top: an occupant that is a byte-identical copy of the incoming version —
//! and that no OTHER tracked skill's record owns — is ADOPTED in place (the caller arms the choice
//! with the incoming bundle digest), never duplicated under a namespaced sibling.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use topos_harness::coverage;
use topos_harness::mcp::{EntryState, McpDialect, plugin_dir};
use topos_harness::{PlacementNaming, registry};
use topos_types::persisted::{Lock, PlacementKind, PlacementMap, PlacementState, SwapCapability};
use topos_types::results::TargetOutcome;

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::scan::{self, ScannedBundle};
use crate::stat_cache;

/// One planned DIRECTORY target — a folder this bundle owns, written by [`crate::materialize`].
#[derive(Debug, Clone)]
pub(crate) struct DirTarget {
    pub dir: PathBuf,
    pub kind: PlacementKind,
    /// The registry slug a `Native` target serves (`None` for the shared dir).
    pub agent: Option<String>,
}

/// One planned ENTRIES target — the shared config FILE a bundle's entries belong in, and the
/// dialect that file speaks. Written by the scope's config converge (`crate::mcp_engine`), never
/// by the materializer: one write per file carries every bundle's entries at once.
#[derive(Debug, Clone)]
pub(crate) struct EntriesTarget {
    /// The DRIVER surface file the entries land in — the plugin dir's own `.mcp.json` under that
    /// dialect, the descriptor's path everywhere else.
    pub file: PathBuf,
    /// The registry slug whose config this is.
    pub agent: String,
    pub dialect: McpDialect,
}

/// One planned target — the two shapes a bundle's bytes reach an agent through: a DIRECTORY this
/// bundle owns, or ENTRIES it owns inside a config file shared with everything else on the
/// machine. A kind picks a mechanic; nothing forces one writer.
#[derive(Debug, Clone)]
pub(crate) enum PlannedTarget {
    Dir(DirTarget),
    Entries(EntriesTarget),
}

/// A surface this plan deliberately does NOT reach, with the reason the receipt states. Reach is
/// the plan's answer, so what reach COST is the plan's answer too — without it a bundle that lands
/// nowhere on some agent would simply go unmentioned.
#[derive(Debug, Clone)]
pub(crate) struct WithheldSurface {
    /// The registry slug that was withheld.
    pub agent: String,
    /// Why the surface was withheld, in [the one outcome vocabulary](TargetOutcome):
    /// `Withheld` — no surface of that kind at this scope; `Unprovable` — the surface exists but
    /// cannot be edited safely.
    pub state: TargetOutcome,
    /// The note a person reads beside it.
    pub note: String,
}

impl PlacementPlan {
    /// The plan's DIR targets, in plan order — the apply set [`crate::materialize`] writes.
    pub(crate) fn dirs(&self) -> impl Iterator<Item = &DirTarget> {
        self.targets.iter().filter_map(|t| match t {
            PlannedTarget::Dir(d) => Some(d),
            PlannedTarget::Entries(_) => None,
        })
    }

    /// The plan's ENTRIES targets, in plan order — the demand the scope's config converge applies.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &EntriesTarget> {
        self.targets.iter().filter_map(|t| match t {
            PlannedTarget::Entries(e) => Some(e),
            PlannedTarget::Dir(_) => None,
        })
    }

    /// The planned entries target for one registry slug, if the plan reaches it.
    pub(crate) fn entries_for(&self, agent: &str) -> Option<&EntriesTarget> {
        self.entries().find(|e| e.agent == agent)
    }

    /// What this plan withheld from one registry slug, if anything.
    pub(crate) fn withheld_for(&self, agent: &str) -> Option<&WithheldSurface> {
        self.withheld.iter().find(|w| w.agent == agent)
    }

    /// Record one dir target.
    fn push_dir(&mut self, dir: PathBuf, kind: PlacementKind, agent: Option<String>) {
        self.targets
            .push(PlannedTarget::Dir(DirTarget { dir, kind, agent }));
    }

    /// Whether a dir is already planned — one dir, one copy, one record.
    fn holds_dir(&self, dir: &Path) -> bool {
        self.dirs().any(|d| d.dir == dir)
    }

    /// [`Self::holds_dir`] for the callers outside this module (the retire scans, which ask
    /// whether a RECORDED placement is still one the plan wants).
    pub(crate) fn holds_planned_dir(&self, dir: &Path) -> bool {
        self.holds_dir(dir)
    }
}

/// The full placement plan for one bundle at one scope — WHAT SHOULD STAND. Its targets are the
/// demand; what topos actually put there is CUSTODY, and that lives in the bundle's own record
/// (`map.json` for dirs, `entries.json` for config entries), never here.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlacementPlan {
    pub targets: Vec<PlannedTarget>,
    /// Whether a shared target is planned — i.e. at least one detected harness rides the one
    /// shared copy (the rest take native copies of their own).
    pub shared_covered: bool,
    /// PROJECT scope only: the candidate roots [`within_project`] refused — each already rendered
    /// as its typed disclosure ([`escape_line`]). The placement is skipped, never redirected; the
    /// caller surfaces the lines so a silent non-delivery is impossible.
    pub refused: Vec<topos_types::Message>,
    /// ENTRIES plans only: the surfaces this plan does not reach, and why (see
    /// [`WithheldSurface`]).
    pub withheld: Vec<WithheldSurface>,
}

/// Compute the placement plan for one skill. `naming` carries the untrusted display name + workspace
/// slug (the collision namespace); `prior` is the durable record whose dirs are kept verbatim for
/// already-recorded targets; `adopt` is the INCOMING version's bundle digest, arming adopt-in-place
/// (a first receive passes it so a byte-identical occupant becomes the placement instead of a
/// namespaced sibling — and a recorded adoption reservation is reusable only for THAT digest).
/// With no roots (no `$HOME`, or a test that does not exercise detection) or no detected harness,
/// the plan is the CLASSIC single placement — the prior record as-is, else the active adapter's
/// placement.
pub(crate) fn plan_targets(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    prior: Option<&PlacementMap>,
    adopt: Option<[u8; 32]>,
) -> PlacementPlan {
    let detected: Vec<&'static registry::KnownHarness> = match &ctx.roots {
        Some(roots) => registry::detected_harnesses(&roots.home, roots.cwd.as_deref()),
        None => Vec::new(),
    };
    if detected.is_empty() {
        return classic_plan(ctx, skill_id, naming, prior, adopt);
    }
    let home = &ctx
        .roots
        .as_ref()
        .expect("detected harnesses imply roots")
        .home;
    let cwd = ctx.roots.as_ref().and_then(|r| r.cwd.as_deref());

    let owned = owned_predicate(prior);
    // The CLI's taken-probe: a path is unavailable when a filesystem entry holds it (lstat, so a
    // dangling symlink counts) OR when ANOTHER tracked skill's record names it — an absent-on-disk
    // recorded path stays reserved for its owner, so the ladder suffixes past it exactly like an
    // occupied dir.
    let taken =
        |p: &Path| topos_harness::dir_taken(p) || recorded_by_another_skill(ctx, skill_id, p);
    let mut plan = PlacementPlan::default();

    // Shared-dir-first: the covered detected harnesses ride ONE copy in the convention dir, and
    // every uncovered one takes a native copy of its own.
    let mut native: Vec<&'static registry::KnownHarness> = Vec::new();
    for h in detected {
        let support = coverage::shared_dir_support(h.slug);
        if support.covered() {
            plan.shared_covered = true;
        } else {
            native.push(h);
        }
    }
    if plan.shared_covered {
        let dir = prior_dir(ctx, prior, PlacementKind::Shared, None, adopt).unwrap_or_else(|| {
            adopt_override(
                ctx,
                topos_harness::choose_skill_dir(
                    &coverage::shared_skills_dir(home),
                    skill_id,
                    naming,
                    &taken,
                    &owned,
                ),
                skill_id,
                naming,
                adopt,
            )
        });
        plan.push_dir(dir, PlacementKind::Shared, None);
    }

    let active_slug = ctx.harness.id().slug();
    for h in native {
        let dir = match prior_dir(ctx, prior, PlacementKind::Native, Some(h.slug), adopt) {
            Some(dir) => dir,
            // EVERY detected harness — the active one included — takes its user skills root from
            // its registry row, through one resolver and the one naming discipline. The active
            // slug keeps its own arm only for what the row cannot answer (see [`adapter_choice`]):
            // a cwd-only harness still lands somewhere instead of silently placing nothing.
            None if h.slug == active_slug => {
                adapter_choice(ctx, skill_id, naming, &taken, &owned, adopt)
            }
            None => {
                let Some(root) =
                    registry::skills_root(h.slug, registry::SkillScope::User, home, cwd)
                else {
                    continue; // a cwd-only harness has no user-scope dir — nothing to place
                };
                adopt_override(
                    ctx,
                    topos_harness::choose_skill_dir(&root, skill_id, naming, &taken, &owned),
                    skill_id,
                    naming,
                    adopt,
                )
            }
        };
        // A native dir may coincide with an already-planned target (a harness whose native user dir
        // IS the shared convention dir, placed under a scope) — one dir, one copy, one record.
        if plan.holds_dir(&dir) {
            continue;
        }
        plan.push_dir(dir, PlacementKind::Native, Some(h.slug.to_owned()));
    }

    // The AGENT-LESS recorded placements — an adopt-in-place source dir, a plain tracked dir with no
    // known harness — are ALWAYS managed: they are the user's own chosen location (often the author's
    // working copy), and detection does not speak for them.
    if let Some(map) = prior {
        for (dir, st) in map.placements.iter().zip(&map.placement_state) {
            if st.kind == PlacementKind::Native
                && st.agent.is_none()
                && !plan.holds_dir(Path::new(dir))
            {
                plan.push_dir(PathBuf::from(dir), PlacementKind::Native, None);
            }
        }
    }
    keep_claimed(&mut plan, prior);

    if plan.targets.is_empty() {
        // Detection found harnesses but none of them resolves to a dir here (every uncovered one is
        // cwd-only, with no user-scope skills root): the classic single placement stands in, so a
        // followed skill is never left with nowhere to land.
        return classic_plan(ctx, skill_id, naming, prior, adopt);
    }
    plan
}

/// A CLAIMED folder is a target of every plan, whatever the row's shape or the scope's detection
/// says. The claim's whole promise is that updates land there from now on, and the four dir
/// planners each reach their targets a different way — detection keys, project keys, the row's
/// frozen `dest` roots — none of which a folder the PERSON named is guaranteed to sit under. So the
/// invariant is applied once, here, at the end of each of them.
///
/// An adopted SOURCE folder is deliberately NOT swept in: it is where the person works, it makes no
/// currency promise, and a destination-frozen row that never named it does not manage it. Only the
/// claim's own marker ([`topos_types::persisted::PlacementClaim`]) opts a folder in.
fn keep_claimed(plan: &mut PlacementPlan, prior: Option<&PlacementMap>) {
    let Some(map) = prior else { return };
    for (dir, st) in map.placements.iter().zip(&map.placement_state) {
        if st.claim.is_some() && !plan.holds_dir(Path::new(dir)) {
            plan.push_dir(PathBuf::from(dir), st.kind, st.agent.clone());
        }
    }
}

/// What a PROJECT-scope prior record yields for one `(kind, agent)` key: nothing recorded, a
/// recorded dir re-proven inside the checkout, or a recorded dir that no longer resolves inside it
/// (a committed symlink can be swapped in AFTER the record was written — a record is a memory, not
/// a permission).
enum PriorProjectDir {
    None,
    Reuse(PathBuf),
    Escaped(PathBuf),
}

/// The PROJECT-scope placement plan — where a project manifest's bundles land INSIDE the checkout
/// (a project-scope bundle materializes in the project itself, so every agent visiting the
/// checkout reads the same bytes): its harness dirs, never committed (each landed dir carries the
/// self-ignore sentinel — see [`crate::scan::IGNORE_SENTINEL`]). Mirrors the shared-dir-first
/// policy ROOTED AT THE PROJECT: one `<project>/.agents/skills` copy for the covered detected
/// agents, plus a native project dir per detected-but-uncovered harness that has one; with nothing
/// detected, the active adapter's project dir (else `<project>/.claude/skills`). A row that pins
/// its own destinations instead goes through [`dest_plan`]. Prior-dir stability considers ONLY
/// placements under the project root, so a same-skill person-scope record never leaks into the
/// project plan.
pub(crate) fn project_plan(
    ctx: &Ctx<'_>,
    project_dir: &Path,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    prior: Option<&PlacementMap>,
    adopt: Option<[u8; 32]>,
) -> PlacementPlan {
    let owned = owned_predicate(prior);
    let taken =
        |p: &Path| topos_harness::dir_taken(p) || recorded_by_another_skill(ctx, skill_id, p);
    // Prior stability, PROJECT-LOCAL: the recorded dir for a (kind, agent) key is reused only when
    // it sits under this project (a home-dir record for the same key belongs to the person scope).
    //
    // A record is not a permission. `starts_with` is LEXICAL — it proves the string, not the path —
    // and the checkout can turn any recorded ancestor into a symlink after the record was written.
    // So a reused prior path passes the SAME containment proof a fresh root does, and one that
    // fails is refused exactly like a fresh escape: a typed line, the placement skipped, the link
    // never followed.
    let prior_in = |kind: PlacementKind, agent: Option<&str>| -> PriorProjectDir {
        let Some(map) = prior else {
            return PriorProjectDir::None;
        };
        let hit = map
            .placements
            .iter()
            .zip(&map.placement_state)
            .find(|(dir, st)| {
                Path::new(dir).starts_with(project_dir)
                    && st.kind == kind
                    && st.agent.as_deref() == agent
                    && (st.materialized_sha.is_some()
                        || !topos_harness::dir_taken(Path::new(dir))
                        || adoption_reservation_holds(dir, st, adopt))
            })
            .map(|(dir, _)| PathBuf::from(dir));
        match hit {
            None => PriorProjectDir::None,
            Some(dir) if within_project(project_dir, &dir) => PriorProjectDir::Reuse(dir),
            Some(dir) => PriorProjectDir::Escaped(dir),
        }
    };
    let choose = |root: &Path| {
        adopt_override(
            ctx,
            topos_harness::choose_skill_dir(root, skill_id, naming, &taken, &owned),
            skill_id,
            naming,
            adopt,
        )
    };
    let mut plan = PlacementPlan::default();

    let home = ctx.roots.as_ref().map(|r| r.home.clone());
    let detected: Vec<&'static registry::KnownHarness> = match &ctx.roots {
        Some(roots) => registry::detected_harnesses(&roots.home, Some(project_dir)),
        None => Vec::new(),
    };

    let mut native: Vec<&'static registry::KnownHarness> = Vec::new();
    for h in &detected {
        let support = coverage::shared_dir_support(h.slug);
        if support.covered() {
            plan.shared_covered = true;
        } else {
            native.push(h);
        }
    }
    if plan.shared_covered {
        let shared_root = project_dir.join(".agents/skills");
        match prior_in(PlacementKind::Shared, None) {
            PriorProjectDir::Reuse(dir) => plan.push_dir(dir, PlacementKind::Shared, None),
            // A recorded dir that no longer resolves inside the checkout is refused, never
            // followed — the rail does not care whether a path is fresh or remembered.
            PriorProjectDir::Escaped(dir) => {
                plan.refused
                    .push(escape_message("the recorded shared dir", &dir));
            }
            // THE CONTAINMENT RAIL, on the DEFAULT root — not just the override. A committed
            // `.agents/skills` symlink aiming out of the checkout would otherwise place every
            // covered agent's bytes wherever it points.
            PriorProjectDir::None if !within_project(project_dir, &shared_root) => {
                plan.refused
                    .push(escape_message("the shared agents dir", &shared_root));
            }
            PriorProjectDir::None => {
                plan.push_dir(choose(&shared_root), PlacementKind::Shared, None);
            }
        }
    }
    for h in native {
        let Some(root) = home.as_deref().and_then(|home| {
            registry::skills_root(
                h.slug,
                registry::SkillScope::Project,
                home,
                Some(project_dir),
            )
        }) else {
            continue; // no project-scope dir for this harness — nothing to place in-project
        };
        let dir = match prior_in(PlacementKind::Native, Some(h.slug)) {
            PriorProjectDir::Reuse(dir) => dir,
            PriorProjectDir::Escaped(dir) => {
                plan.refused.push(escape_message(h.slug, &dir));
                continue;
            }
            PriorProjectDir::None if !within_project(project_dir, &root) => {
                plan.refused.push(escape_message(h.slug, &root));
                continue;
            }
            PriorProjectDir::None => choose(&root),
        };
        if plan.holds_dir(&dir) {
            continue;
        }
        plan.push_dir(dir, PlacementKind::Native, Some(h.slug.to_owned()));
    }

    if plan.targets.is_empty() {
        // Nothing detected (or nothing with a project dir): the active adapter's project root,
        // else the Claude-Code-shaped default — a project manifest must always have somewhere to
        // land, and `.claude/skills` is the one convention every teammate's machine resolves.
        let active = ctx.harness.id().slug();
        let root = home
            .as_deref()
            .and_then(|home| {
                registry::skills_root(
                    active,
                    registry::SkillScope::Project,
                    home,
                    Some(project_dir),
                )
            })
            .unwrap_or_else(|| project_dir.join(".claude/skills"));
        match prior_in(PlacementKind::Native, Some(active)) {
            PriorProjectDir::Reuse(dir) => {
                plan.push_dir(dir, PlacementKind::Native, Some(active.to_owned()));
            }
            PriorProjectDir::Escaped(dir) => plan.refused.push(escape_message(active, &dir)),
            // The last root is the rail's last stand: refusing leaves this scope with NO target,
            // which is the honest answer — nothing lands rather than landing outside the checkout.
            PriorProjectDir::None if !within_project(project_dir, &root) => {
                plan.refused.push(escape_message(active, &root));
            }
            PriorProjectDir::None => {
                plan.push_dir(
                    choose(&root),
                    PlacementKind::Native,
                    Some(active.to_owned()),
                );
            }
        }
    }
    keep_claimed(&mut plan, prior);
    plan
}

/// The DEST-FROZEN placement plan — a manifest row carrying `dest = [...]`: exactly one target
/// per dest entry, DETECTION IGNORED (the row froze its destinations; a newly appearing agent
/// changes nothing until the row does). `~/` entries resolve against the machine home, absolute
/// entries stand as-is; `project_dir` = the PROJECT scope, whose entries resolve against the
/// checkout behind the [`within_project`] containment rail (refused + disclosed, never
/// redirected). Prior-dir stability: a recorded placement directly under an entry's root keeps
/// its dir verbatim; a fresh entry chooses through the ONE naming discipline with the usual
/// adopt-in-place override. Applies to both scopes — the person scope gains the override seam
/// it never had.
pub(crate) fn dest_plan(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    dest: &[String],
    project_dir: Option<&Path>,
    prior: Option<&PlacementMap>,
    adopt: Option<[u8; 32]>,
) -> PlacementPlan {
    let owned = owned_predicate(prior);
    let taken =
        |p: &Path| topos_harness::dir_taken(p) || recorded_by_another_skill(ctx, skill_id, p);
    let mut plan = PlacementPlan::default();
    for entry in dest {
        let root: PathBuf = match project_dir {
            Some(dir) => {
                if !safe_project_rel(entry) {
                    plan.refused
                        .push(escape_message("the dest entry", Path::new(entry)));
                    continue;
                }
                let root = dir.join(entry.trim_start_matches("./"));
                if !within_project(dir, &root) {
                    plan.refused.push(escape_message("the dest entry", &root));
                    continue;
                }
                root
            }
            None => {
                if let Some(rest) = entry.strip_prefix("~/") {
                    let Some(roots) = &ctx.roots else {
                        continue; // no home known — the entry cannot resolve this run
                    };
                    roots.home.join(rest)
                } else {
                    PathBuf::from(entry)
                }
            }
        };
        // Prior stability: the recorded placement under THIS root keeps its dir (the record,
        // not re-derivation, is what holds a dest copy still). The same reusability rule as
        // every other prior probe: materialized, or an unoccupied/still-valid reservation.
        let dir = prior
            .and_then(|map| {
                map.placements
                    .iter()
                    .zip(&map.placement_state)
                    .find(|(d, st)| {
                        Path::new(d).parent() == Some(root.as_path())
                            && (st.materialized_sha.is_some()
                                || !topos_harness::dir_taken(Path::new(d))
                                || adoption_reservation_holds(d, st, adopt))
                    })
                    .map(|(d, _)| PathBuf::from(d))
            })
            .unwrap_or_else(|| {
                adopt_override(
                    ctx,
                    topos_harness::choose_skill_dir(&root, skill_id, naming, &taken, &owned),
                    skill_id,
                    naming,
                    adopt,
                )
            });
        if plan.holds_dir(&dir) {
            continue;
        }
        plan.push_dir(dir, PlacementKind::Native, None);
    }
    keep_claimed(&mut plan, prior);
    plan
}

// ---------------------------------------------------------------------------------------------
// The ENTRIES half of the planner — where a bundle's entries belong inside SHARED config files.
// ---------------------------------------------------------------------------------------------

/// What one harness's MCP config surface resolves to at one scope — the ONE resolution both the
/// planner (which decides reach) and the converge (which must find the files custody names) ask.
#[derive(Debug, Clone)]
pub(crate) enum ConfigSurface {
    /// A usable surface: the driver file, the path engagement is probed at, and the dialect.
    Ready {
        root: PathBuf,
        file: PathBuf,
        dialect: McpDialect,
    },
    /// The harness has no config surface AT THIS SCOPE (a project scope never falls back to the
    /// user surface). `note` is the phrase the receipt states.
    NotSupported { note: &'static str },
    /// PROJECT scope: the surface does not resolve inside the checkout — refused and disclosed,
    /// never redirected (the same rail every project path passes, see [`within_project`]).
    Escaped { path: PathBuf },
}

/// Resolve one harness's MCP config surface at one scope. `project_root` `Some` = the PROJECT
/// scope, whose surfaces are the checkout-relative ones, containment-proven.
pub(crate) fn config_surface(
    h: &registry::KnownHarness,
    home: &Path,
    project_root: Option<&Path>,
) -> ConfigSurface {
    let resolved = match project_root {
        Some(root) => match h.mcp().and_then(|m| m.project) {
            Some((rel, dialect)) => {
                let path = root.join(rel);
                // THE CONTAINMENT RAIL, before ANY read or write: the resolved path (symlinks
                // followed for the check) must stay inside the checkout.
                if !within_project(root, &path) {
                    return ConfigSurface::Escaped { path };
                }
                Some((path, dialect))
            }
            None => None,
        },
        None => h
            .mcp()
            .and_then(|m| m.user)
            .and_then(|s| h.mcp_user_path(home).map(|p| (p, s.dialect))),
    };
    match resolved {
        Some((root, dialect)) => ConfigSurface::Ready {
            file: surface_file(&root, dialect),
            root,
            dialect,
        },
        None => ConfigSurface::NotSupported {
            note: if project_root.is_some() {
                "no project-level config"
            } else {
                "no user-level config"
            },
        },
    }
}

/// The DRIVER surface file for a resolved descriptor surface: the plugin DIR's `.mcp.json` for
/// [`McpDialect::ClaudePluginDir`], the path itself for every file dialect.
pub(crate) fn surface_file(path: &Path, dialect: McpDialect) -> PathBuf {
    if dialect == McpDialect::ClaudePluginDir {
        path.join(plugin_dir::PLUGIN_MCP_PATH)
    } else {
        path.to_path_buf()
    }
}

/// **The ENTRIES plan for one bundle at one scope** — the config files its entries belong in, plus
/// the surfaces the scope WITHHELD and why. The mirror of the dir planners: it says what SHOULD
/// stand, and nothing about what already does (that is the bundle's custody record's answer).
///
/// `reach` is the harness narrowing the caller resolved — a row's `dest` entries mapped to the
/// config files they name, or (for a targeted verb) the harnesses whose recorded rows prove the
/// bundle already stands there. `None` = every MCP-capable harness. Narrowing resolves HERE, once:
/// a harness outside it earns no target and no withheld line, because the row never asked for it.
///
/// The three outcomes, in the order the surface decides them: no surface at this scope, or one the
/// containment rail refuses ⇒ WITHHELD, disclosed; a surface the machine does not engage (the
/// harness is neither detected nor does its config already exist) ⇒ nothing at all, because there
/// is no agent here to reach; else an ENTRIES target.
pub(crate) fn entries_plan(
    ctx: &Ctx<'_>,
    project_root: Option<&Path>,
    reach: Option<&[String]>,
) -> PlacementPlan {
    let Some(roots) = &ctx.roots else {
        return PlacementPlan::default(); // no machine roots: no config surface is resolvable
    };
    // A PROJECT scope detects against the checkout; the person scope against the machine's cwd.
    let detected: BTreeSet<String> =
        registry::detected_harnesses(&roots.home, project_root.or(roots.cwd.as_deref()))
            .iter()
            .map(|h| h.slug.to_owned())
            .collect();
    entries_plan_at(
        ctx.fs,
        &topos_harness::mcp::descriptor::mcp_harnesses(),
        &roots.home,
        &detected,
        project_root,
        reach,
    )
}

/// [`entries_plan`] over the primitives — the machine home, the harness table, and the DETECTED
/// set as arguments (the callers that already probed detection for their converge pass the same
/// one, so a plan and the converge it feeds can never disagree about which agents are here).
pub(crate) fn entries_plan_at(
    fs: &dyn crate::fs_seam::FsOps,
    descriptors: &[&'static registry::KnownHarness],
    home: &Path,
    detected: &BTreeSet<String>,
    project_root: Option<&Path>,
    reach: Option<&[String]>,
) -> PlacementPlan {
    let mut plan = PlacementPlan::default();
    for h in descriptors {
        if reach.is_some_and(|r| !r.iter().any(|s| s == h.slug)) {
            continue;
        }
        match config_surface(h, home, project_root) {
            ConfigSurface::NotSupported { note } => plan.withheld.push(WithheldSurface {
                agent: h.slug.to_owned(),
                state: TargetOutcome::Withheld,
                note: note.to_owned(),
            }),
            ConfigSurface::Escaped { .. } => plan.withheld.push(WithheldSurface {
                agent: h.slug.to_owned(),
                state: TargetOutcome::Unprovable,
                note: "the config path does not resolve inside this checkout".to_owned(),
            }),
            ConfigSurface::Ready {
                root,
                file,
                dialect,
            } => {
                // Engagement: the harness is detected on this machine, OR its config surface
                // already exists (entries were placed while it was detected — an update or a
                // removal must still reach them).
                if !(detected.contains(h.slug) || fs.exists(&root)) {
                    continue;
                }
                plan.targets.push(PlannedTarget::Entries(EntriesTarget {
                    file,
                    agent: h.slug.to_owned(),
                    dialect,
                }));
            }
        }
    }
    plan
}

/// The classic single-placement plan (no detection): the prior record's targets as-is — except a
/// STALE adoption reservation (never materialized, dir occupied, the occupant no longer matching
/// its recorded digest), which is re-chosen fresh so [`reconcile_map`] can replace it (mirroring
/// [`prior_dir`]'s validation on the detection path; returned verbatim it would wedge every apply
/// on the never-clobber refusal) — else the active adapter's placement.
fn classic_plan(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    prior: Option<&PlacementMap>,
    adopt: Option<[u8; 32]>,
) -> PlacementPlan {
    let owned = owned_predicate(prior);
    let taken =
        |p: &Path| topos_harness::dir_taken(p) || recorded_by_another_skill(ctx, skill_id, p);
    if let Some(map) = prior
        && !map.placements.is_empty()
    {
        let mut targets: Vec<DirTarget> = Vec::new();
        let mut invalid: Vec<(PlacementKind, Option<String>)> = Vec::new();
        for (dir, st) in map.placements.iter().zip(&map.placement_state) {
            let reusable = st.materialized_sha.is_some()
                || !topos_harness::dir_taken(Path::new(dir))
                || adoption_reservation_holds(dir, st, adopt);
            if reusable {
                targets.push(DirTarget {
                    dir: PathBuf::from(dir),
                    kind: st.kind,
                    agent: st.agent.clone(),
                });
            } else {
                invalid.push((st.kind, st.agent.clone()));
            }
        }
        // Each invalidated key re-chooses through the active adapter (+ the caller's adopt digest);
        // an equal dir already planned collapses to one target.
        for (kind, agent) in invalid {
            let dir = adapter_choice(ctx, skill_id, naming, &taken, &owned, adopt);
            if targets.iter().any(|t| t.dir == dir) {
                continue;
            }
            targets.push(DirTarget { dir, kind, agent });
        }
        return PlacementPlan {
            targets: targets.into_iter().map(PlannedTarget::Dir).collect(),
            ..PlacementPlan::default()
        };
    }
    PlacementPlan {
        targets: vec![PlannedTarget::Dir(DirTarget {
            dir: adapter_choice(ctx, skill_id, naming, &taken, &owned, adopt),
            kind: PlacementKind::Native,
            agent: Some(ctx.harness.id().slug().to_owned()),
        })],
        ..PlacementPlan::default()
    }
}

/// The ACTIVE harness's placement dir — **the root from its registry ROW, the name from the ONE
/// naming discipline.**
///
/// The row is the single source of the dir, exactly as it is for every other detected harness
/// ([`plan_targets`]'s sibling branch resolves through the same [`registry::skills_root`]).
/// Detection already follows a machine-local table that moved an agent's skills dir; a plan that
/// asked the COMPILED adapter instead would keep landing bytes at the spelling this build was
/// born with, in a folder the agent no longer reads. What stays the adapter's is what is genuinely
/// its own — its id, its discovery, its config surface — none of which is a directory.
///
/// The adapter still answers where the row CANNOT: with no machine roots injected (production with
/// no `$HOME`, and every test that does not exercise detection) there is nothing to resolve a row
/// against. That path keeps its own hardening against paths ANOTHER tracked skill's record owns —
/// the adapter probes only the filesystem, being content-blind about the sidecar, so when its
/// answer lands on a recorded-elsewhere path the ONE ladder re-runs over the same root with the
/// CLI's full taken-probe (no duplicate naming logic; the adapter told us the root).
///
/// The adopt override then applies as usual, whichever root answered.
fn adapter_choice(
    ctx: &Ctx<'_>,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    taken: &dyn Fn(&Path) -> bool,
    owned: &dyn Fn(&Path) -> bool,
    adopt: Option<[u8; 32]>,
) -> PathBuf {
    let dir = match active_skills_root(ctx) {
        Some(root) => topos_harness::choose_skill_dir(&root, skill_id, naming, taken, owned),
        None => {
            let mut dir = ctx.harness.placement_for(skill_id, naming, None).dir;
            if recorded_by_another_skill(ctx, skill_id, &dir)
                && let Some(root) = dir.parent().map(Path::to_path_buf)
            {
                dir = topos_harness::choose_skill_dir(&root, skill_id, naming, taken, owned);
            }
            dir
        }
    };
    adopt_override(ctx, dir, skill_id, naming, adopt)
}

/// The active harness's USER-scope skills root, from its registry row through the one resolver.
/// `None` when no machine roots are injected (nothing to resolve against) or the row names no
/// user-scope dir at all (a cwd-only harness) — the two cases in which the adapter's own compiled
/// default is the honest answer.
fn active_skills_root(ctx: &Ctx<'_>) -> Option<PathBuf> {
    let roots = ctx.roots.as_ref()?;
    registry::skills_root(
        ctx.harness.id().slug(),
        registry::SkillScope::User,
        &roots.home,
        roots.cwd.as_deref(),
    )
}

/// Adopt-in-place override on a chosen placement dir: when the by-name candidate under the same
/// root is OCCUPIED by a byte-identical copy of the incoming version — and no OTHER tracked
/// skill's record already owns that dir — that dir IS the placement, never a second namespaced/id
/// copy. A dir name topos reserves for itself is never overridden. When the naming discipline
/// already chose the by-name dir (free, or owned), the candidate equals the choice and nothing
/// changes.
fn adopt_override(
    ctx: &Ctx<'_>,
    dir: PathBuf,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    adopt: Option<[u8; 32]>,
) -> PathBuf {
    let Some(digest) = adopt else {
        return dir;
    };
    let Some(name) = naming.name.and_then(topos_harness::sanitize_skill_dir) else {
        return dir;
    };
    if topos_harness::is_reserved_skill_dir(&name, skill_id) {
        return dir;
    }
    let Some(parent) = dir.parent() else {
        return dir;
    };
    let candidate = parent.join(&name);
    if candidate != dir
        && topos_harness::dir_taken(&candidate)
        && !recorded_by_another_skill(ctx, skill_id, &candidate)
        && digest_probe(digest)(&candidate)
    {
        candidate
    } else {
        dir
    }
}

/// The dir the prior record holds for a (kind, agent) key — target stability comes from the record.
/// A record that was NEVER materialized and whose dir has since been occupied by someone else is not
/// reusable (never clobber a foreign dir): the key re-chooses, and [`reconcile_map`] replaces the
/// stale reservation. ONE occupied-but-reusable exception: an ADOPTION RESERVATION — a
/// never-materialized placement whose `pre_existing_sha` records the occupant's digest, laid at
/// first-receive baseline time for a byte-identical occupant — stays reusable while the occupant is
/// unchanged (a fresh scan reproduces the recorded sha) AND, when the caller supplied the incoming
/// version's digest, that version is still the recorded one (an adoption recorded for version A is
/// never reused for an apply of version B). A failed scan or any mismatch means the reservation
/// lapsed: NOT reusable, and it is replaced like any other stale reservation.
fn prior_dir(
    ctx: &Ctx<'_>,
    prior: Option<&PlacementMap>,
    kind: PlacementKind,
    agent: Option<&str>,
    adopt: Option<[u8; 32]>,
) -> Option<PathBuf> {
    let map = prior?;
    map.placements
        .iter()
        .zip(&map.placement_state)
        .find(|(dir, st)| {
            st.kind == kind
                && st.agent.as_deref() == agent
                // The mirror of `project_plan`'s project-local rule: a dir recorded INSIDE a
                // project checkout belongs to that scope — the person plan never reuses it as its
                // own (kind, agent) slot (it would swallow the home placement whole).
                && !under_project_manifest(ctx, Path::new(dir))
                && (st.materialized_sha.is_some()
                    || !topos_harness::dir_taken(Path::new(dir))
                    || adoption_reservation_holds(dir, st, adopt))
        })
        .map(|(dir, _)| PathBuf::from(dir))
}

/// **The containment rail every PROJECT-scope path passes** — the override's proof, generalized to
/// the property it always was: a path a checkout can aim is a path a checkout can aim ANYWHERE, and
/// `.claude/skills` committed as a symlink to `~/` is exactly as effective as a `path = "../.."`
/// override the grammar already refuses. So the rail is not the override's rail; it is the project
/// scope's, and every root the plan yields (plus the project's own `.topos` store) goes through it.
///
/// Two proofs, both needed:
/// - **(a)** no component of `candidate` below `project_dir` is a symlink (an existing prefix that
///   is one aims the rest of the walk wherever it points);
/// - **(b)** the canonicalized deepest-existing ancestor of `candidate` still sits under the
///   canonicalized `project_dir` (which catches an ancestor symlink (a) could not see, and a
///   `..` climb).
///
/// A `candidate` that is not even lexically under `project_dir`, and an unresolvable
/// `project_dir`, both answer `false` — fail closed.
pub(crate) fn within_project(project_dir: &Path, candidate: &Path) -> bool {
    let Ok(proj_real) = project_dir.canonicalize() else {
        return false;
    };
    let Ok(rel) = candidate.strip_prefix(project_dir) else {
        return false;
    };
    // (a) Reject symlink components: every existing prefix below the project dir must be a plain
    // entry.
    let mut prefix = project_dir.to_path_buf();
    for comp in rel.components() {
        prefix = prefix.join(comp);
        match std::fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(_) => break, // the rest does not exist yet — nothing left to lstat
        }
    }
    // (b) The canonicalized containment proof over the deepest existing ancestor.
    let mut probe = candidate.to_path_buf();
    let real_prefix = loop {
        match probe.canonicalize() {
            Ok(r) => break r,
            // Walk up to the deepest existing ancestor; the loop terminates at project_dir
            // (which canonicalized above) or bails at the filesystem root.
            Err(_) => match probe.parent() {
                Some(up) => probe = up.to_path_buf(),
                None => return false,
            },
        }
    };
    real_prefix.starts_with(&proj_real)
}

/// The disclosure a refused project root earns — the override's voice, one rail wider.
pub(crate) fn escape_line(what: &str, path: &Path) -> String {
    format!(
        "{} does not resolve inside this checkout ({what}) — a symlink, or a path that climbs out \
         of it. A committed file must never aim managed files elsewhere, so topos skipped it. \
         Point it at a path inside the checkout, then run 'topos update'.",
        path.display()
    )
}

/// [`escape_line`] as the typed disclosure a sweep carries. The refusal that rides a
/// [`crate::error::ClientError`] keeps the bare sentence — a code belongs on the message channel,
/// never inside another error's prose.
pub(crate) fn escape_message(what: &str, path: &Path) -> topos_types::Message {
    crate::message::failure("PLACEMENT_ESCAPES_PROJECT", escape_line(what, path))
}

/// Whether a manifest `[placement]` value is a SAFE project-relative path: non-empty, relative,
/// and every component ordinary (no `..`, no root) — so `project_dir.join(value)` provably stays
/// inside the checkout.
pub(crate) fn safe_project_rel(raw: &str) -> bool {
    use std::path::Component;
    let p = Path::new(raw);
    !raw.trim().is_empty()
        && p.is_relative()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Whether a dir sits inside some PROJECT checkout — an ancestor holds a `topos.toml` (the
/// manifest travels with the repo; its placements are that scope's business, never the person
/// scope's prior).
pub(crate) fn under_project_manifest(ctx: &Ctx<'_>, dir: &Path) -> bool {
    let mut cur = dir.parent();
    while let Some(d) = cur {
        if ctx.fs.exists(&d.join(crate::manifest::MANIFEST_FILE)) {
            return true;
        }
        cur = d.parent();
    }
    false
}

/// Whether a never-materialized-but-occupied placement is a still-valid ADOPTION reservation: its
/// recorded `pre_existing_sha` (the adopted occupant's digest) still matches a fresh scan of the
/// dir, and — when the caller supplied the incoming version's digest — that digest too. Fails
/// closed — an unscannable or changed occupant, or a version the adoption was not recorded for, is
/// not ours to reuse.
fn adoption_reservation_holds(dir: &str, st: &PlacementState, adopt: Option<[u8; 32]>) -> bool {
    st.pre_existing_sha.as_deref().is_some_and(|sha| {
        adopt.is_none_or(|d| topos_core::digest::to_hex(&d) == sha)
            && scan::scan(Path::new(dir))
                .is_ok_and(|s| topos_core::digest::to_hex(&s.bundle_digest) == sha)
    })
}

/// The standard adopt-in-place content probe over an incoming/current bundle digest: an occupant
/// whose scanned bytes digest-equal it is adoptable.
fn digest_probe(digest: [u8; 32]) -> impl Fn(&Path) -> bool {
    move |p: &Path| scan::scan(p).is_ok_and(|s| s.bundle_digest == digest)
}

/// A path's canonical form for record comparison, resolvable even when the LEAF is absent: a
/// deleted placement dir still compares by canonicalizing its (real) parent and re-joining the
/// leaf — records store canonicalized paths, and a deleted dir must stay comparable or its
/// reservation would silently stop matching.
fn canonical_or_parent(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().or_else(|| {
        let parent = p.parent()?;
        let leaf = p.file_name()?;
        std::fs::canonicalize(parent).ok().map(|c| c.join(leaf))
    })
}

/// Whether ANOTHER tracked skill's placement map records `dir` (materialized or reserved,
/// PRESENT on disk or deleted — an absent recorded path stays reserved for its owner) — a
/// candidate some other skill's record names must never be claimed or adopted, or two records
/// would own one path. Best-effort over the sidecar walk (naming-choice moments are rare, so the
/// cost is fine): an absent sidecar records nothing; an unreadable entry is skipped — the
/// materializer's never-clobber backstop stays the hard rail behind this planning-time hygiene.
pub(crate) fn recorded_by_another_skill(ctx: &Ctx<'_>, skill_id: &str, dir: &Path) -> bool {
    if !ctx.fs.exists(&ctx.layout.skills_dir()) {
        return false;
    }
    let Ok(entries) = ctx.fs.read_dir(&ctx.layout.skills_dir()) else {
        return false;
    };
    // Records store CANONICALIZED paths (adopt-in-place canonicalizes its source); the candidate
    // may arrive through a symlinked root — and either side may be absent on disk. Raw equality
    // first, then the parent-resolved canonical forms.
    let canonical = canonical_or_parent(dir);
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() || id == skill_id {
            continue;
        }
        let Ok(sid) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let Ok(Some(map)) = crate::doc::read_map(ctx.fs, &ctx.layout.published(&sid).map) else {
            continue;
        };
        if map.placements.iter().any(|p| {
            let recorded = Path::new(p);
            recorded == dir
                || canonical
                    .as_deref()
                    .is_some_and(|c| canonical_or_parent(recorded).is_some_and(|r| r == c))
        }) {
            return true;
        }
    }
    false
}

/// Record ADOPTION RESERVATIONS on a freshly reconciled map: every never-materialized placement
/// whose dir already exists under the sanitized display `name` with content digest-equal to
/// `digest` — and which no OTHER tracked skill's record owns (the same guard the plan's adopt
/// choice applies) — gets that digest into `pre_existing_sha` — the durable adoption record
/// ([`prior_dir`] and [`classic_plan`] reuse it across plans; the sticky prior-bytes semantics
/// ride it). No bytes move here; `materialized_sha` stays untouched.
pub(crate) fn record_adoptions(
    ctx: &Ctx<'_>,
    map: &mut PlacementMap,
    skill_id: &str,
    name: &str,
    digest: &[u8; 32],
) {
    let Some(sanitized) = topos_harness::sanitize_skill_dir(name) else {
        return;
    };
    let probe = digest_probe(*digest);
    for (dir, st) in map.placements.iter().zip(map.placement_state.iter_mut()) {
        if st.materialized_sha.is_some() {
            continue;
        }
        let p = Path::new(dir);
        if p.file_name().and_then(|l| l.to_str()) == Some(sanitized.as_str())
            && topos_harness::dir_taken(p)
            && !recorded_by_another_skill(ctx, skill_id, p)
            && probe(p)
        {
            st.pre_existing_sha = Some(topos_core::digest::to_hex(digest));
        }
    }
}

/// The never-clobber ownership predicate: a dir counts as this skill's own iff the record names it
/// AND topos actually materialized bytes there (a recorded-but-never-placed reservation that someone
/// else has since occupied is NOT ours to overwrite).
fn owned_predicate(prior: Option<&PlacementMap>) -> impl Fn(&Path) -> bool + '_ {
    move |p: &Path| {
        prior.is_some_and(|map| {
            map.placements
                .iter()
                .zip(&map.placement_state)
                .any(|(dir, st)| Path::new(dir) == p && st.materialized_sha.is_some())
        })
    }
}

/// Reconcile the durable record with a fresh plan's DIR half: every prior placement is KEPT (its dir
/// and state verbatim — a placement leaves the record only through an explicit verb), and every
/// planned dir target the record does not yet hold is APPENDED never-materialized. Returns the next
/// map.
///
/// The ENTRY half is deliberately not reconciled here. A dir target is RESERVED before its bytes
/// land (the reservation is what holds the name still); an entry has no name to hold — its row IS
/// the fingerprint of what the converge wrote, so it is born by the write and by nothing else.
/// A planned-but-unwritten entry would be a row claiming custody of bytes nobody put there.
pub(crate) fn reconcile_map(prior: &PlacementMap, plan: &PlacementPlan) -> PlacementMap {
    let mut next = prior.clone();
    for t in plan.dirs() {
        let dir = t.dir.to_string_lossy().into_owned();
        if next.placements.contains(&dir) {
            continue;
        }
        // A stale RESERVATION for the same (kind, agent) key — recorded, never materialized, and
        // re-chosen because its dir got occupied — is REPLACED in place (a reservation holds no
        // bytes, so nothing freezes); everything else appends. The slot's state resets with the
        // dir: the old reservation's `pre_existing_sha` described the OLD dir's occupant and must
        // never migrate to the new one. A reservation whose dir the plan STILL wants is not
        // stale — a dest plan holds several (Native, agent-less) targets at once, and replacing
        // a sibling's live slot would cannibalize it.
        if let Some(i) = next
            .placements
            .iter()
            .zip(&next.placement_state)
            .position(|(d, st)| {
                st.kind == t.kind
                    && st.agent.as_deref() == t.agent.as_deref()
                    && st.materialized_sha.is_none()
                    && !plan.holds_dir(Path::new(d))
            })
        {
            next.placements[i] = dir;
            next.placement_state[i] = PlacementState {
                kind: t.kind,
                agent: t.agent.clone(),
                materialized_sha: None,
                pre_existing_sha: None,
                swap_capability: SwapCapability::Unsupported,
                adopted_source: false,
                claim: None,
            };
            continue;
        }
        next.placements.push(dir);
        next.placement_state.push(PlacementState {
            kind: t.kind,
            agent: t.agent.clone(),
            materialized_sha: None,
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            adopted_source: false,
            claim: None,
        });
    }
    next
}

/// The indices of `map`'s placements the CURRENT plan manages — the apply set. A recorded placement
/// outside the plan (a lost detection, an excluded agent whose clean has not run) is skipped: frozen
/// in place, never written, never deleted.
pub(crate) fn managed_indices(map: &PlacementMap, plan: &PlacementPlan) -> Vec<usize> {
    map.placements
        .iter()
        .enumerate()
        .filter(|(_, dir)| plan.holds_dir(Path::new(dir)))
        .map(|(i, _)| i)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The multi-placement work-tree scan — draft-anywhere classification.
// ---------------------------------------------------------------------------------------------

/// **The ONE drift vocabulary** — what a managed target LOOKS LIKE against what the record says
/// topos put there. Both target shapes project onto these five: a placement DIR through
/// [`ScanStatus::drift`], a config ENTRY through [`Drift::of_entry`].
///
/// It is deliberately PAYLOAD-FREE, and a projection rather than a replacement: the payload stays
/// where it is load-bearing ([`ScanStatus::Modified`] carries the scanned bytes every consumer
/// commits), and this is what the words a person reads — and the wire states — are chosen from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drift {
    /// Nothing is there: an absent dir, or an entry gone from its config file.
    Absent,
    /// Byte-for-byte what the record says topos wrote.
    Clean,
    /// Changed since topos wrote it — a local edit, never clobbered.
    Modified,
    /// Content topos holds no record of writing: not ours, never overwritten.
    Foreign,
    /// It cannot be read safely — fail closed.
    Unscannable,
}

impl Drift {
    /// One config ENTRY's apply outcome projected onto the vocabulary: what the converge FOUND
    /// before it wrote. A first placement was absent; an update sat at our own recorded
    /// fingerprint; a hand edit is drift; a `topos-` key we hold no record of is foreign; and an
    /// entry the removal took out is absent once the write lands.
    pub(crate) fn of_entry(state: EntryState) -> Self {
        match state {
            EntryState::PlacedNew | EntryState::Removed => Self::Absent,
            EntryState::Current | EntryState::Updated => Self::Clean,
            EntryState::Drifted => Self::Modified,
            EntryState::Foreign => Self::Foreign,
        }
    }

    /// This drift plus the one bit the record cannot hold — whether the run WROTE the target —
    /// as the [one outcome vocabulary](TargetOutcome) every receipt and every wire state is
    /// chosen from. This is the ONLY place an outcome word is derived: a render site picks a
    /// word by asking the record what happened, never by naming a state of its own.
    ///
    /// The two write outcomes are what a person most needs told apart, and only the found-state
    /// distinguishes them: writing where nothing stood is a first placement (`created`), writing
    /// where our own recorded target stood is a catch-up or a repair (`refreshed`). A run that
    /// wrote nothing reports what it found.
    pub(crate) fn outcome(self, wrote: bool) -> TargetOutcome {
        match (self, wrote) {
            (Self::Absent, true) => TargetOutcome::Created,
            (Self::Clean, true) => TargetOutcome::Refreshed,
            // Drift and foreign content are never written over, so `wrote` cannot be true of
            // them; if a caller ever claims otherwise the record's word still wins.
            (Self::Modified, _) => TargetOutcome::Drifted,
            (Self::Foreign, _) => TargetOutcome::Conflicting,
            (Self::Unscannable, _) => TargetOutcome::Unprovable,
            (Self::Absent, false) => TargetOutcome::Removed,
            (Self::Clean, false) => TargetOutcome::Current,
        }
    }
}

/// One placement's scan outcome, against ITS OWN recorded materialized sha.
pub(crate) enum ScanStatus {
    /// The dir does not exist (or is a dangling symlink).
    Absent,
    /// Bytes match the recorded sha — no local edits in this copy. Carries ONLY the `bundle_digest`
    /// (which equals the recorded sha): the stat cache may have proven this without reading a byte,
    /// so there is no `ScannedBundle` to hand out. A consumer that needs the working bytes of a clean
    /// copy re-scans the dir (the cold stale-replica / merge-escape paths do exactly that).
    Clean { digest: [u8; 32] },
    /// Bytes differ from the recorded sha — a local edit in this copy. ALWAYS carries the full scanned
    /// bundle (bytes), because every `Modified` consumer snapshots or commits those exact bytes.
    Modified { scanned: ScannedBundle },
    /// The record says topos never wrote here, yet the dir holds content — not ours; never scanned
    /// into drafts, never overwritten.
    Foreign,
    /// The dir exists but cannot be scanned safely — fail closed, never overwrite it.
    Unscannable,
}

impl ScanStatus {
    /// This dir's projection onto the [one drift vocabulary](Drift) — the classification without
    /// the bytes, for every consumer that asks WHAT it is rather than WHICH bytes it holds.
    pub(crate) fn drift(&self) -> Drift {
        match self {
            Self::Absent => Drift::Absent,
            Self::Clean { .. } => Drift::Clean,
            Self::Modified { .. } => Drift::Modified,
            Self::Foreign => Drift::Foreign,
            Self::Unscannable => Drift::Unscannable,
        }
    }
}

/// One placement's scan row.
pub(crate) struct PlacementScan {
    pub idx: usize,
    pub dir: PathBuf,
    pub status: ScanStatus,
}

/// Scan every recorded placement against its per-placement materialized sha. The caller classifies
/// (see [`crate::ops::sync_engine::compute_work`]) or snapshots (reset / withdraw) from these rows.
///
/// The routine drift verdict is accelerated by the stat cache ([`crate::stat_cache`]) — a clean copy
/// is confirmed by `(mtime_ns, ctime_ns, size)` rather than a re-hash — unless `TOPOS_NO_STAT_CACHE=1`
/// disables it. The verdict is byte-for-byte identical either way (the cache only spares reads).
pub(crate) fn scan_placements(
    ctx: &Ctx<'_>,
    map: &PlacementMap,
) -> Result<Vec<PlacementScan>, ClientError> {
    scan_placements_cached(ctx, map, stat_cache::enabled_from_env())
}

/// The cache-mode-explicit core of [`scan_placements`] — the equivalence tests drive both modes here
/// without touching process-global env.
pub(crate) fn scan_placements_cached(
    ctx: &Ctx<'_>,
    map: &PlacementMap,
    cache_on: bool,
) -> Result<Vec<PlacementScan>, ClientError> {
    let mut cache = if cache_on {
        stat_cache::load(ctx.fs, &ctx.layout)
    } else {
        stat_cache::StatCache::default()
    };
    let original = cache.clone();
    // The racy-clean reference: when the cache was last persisted. Read BEFORE this scan writes it,
    // so a file touched at/after the last write is re-hashed rather than trusted.
    let racy_ref = cache_on
        .then(|| stat_cache::last_written_ns(&ctx.layout))
        .flatten();

    let mut out = Vec::with_capacity(map.placements.len());
    for (idx, (placement, state)) in map.placements.iter().zip(&map.placement_state).enumerate() {
        let dir = PathBuf::from(placement);
        let status = scan_one(ctx, &dir, state, cache_on.then_some(&mut cache), racy_ref)?;
        out.push(PlacementScan { idx, dir, status });
    }

    // Persist the refreshed cache only when it moved — best-effort, never a scan blocker.
    if cache_on && cache != original {
        let _ = stat_cache::store(ctx.fs, &ctx.layout, &cache);
    }
    Ok(out)
}

fn scan_one(
    ctx: &Ctx<'_>,
    dir: &Path,
    state: &PlacementState,
    cache: Option<&mut stat_cache::StatCache>,
    racy_ref: Option<i64>,
) -> Result<ScanStatus, ClientError> {
    match ctx.fs.path_kind(dir)? {
        None => return Ok(ScanStatus::Absent),
        // A dangling symlink (its target is gone — e.g. a crash in the rename-dance absent window)
        // is effectively ABSENT: the next apply first-installs into the resolved target and recovers.
        Some(crate::fs_seam::PathKind::Symlink) if std::fs::canonicalize(dir).is_err() => {
            return Ok(ScanStatus::Absent);
        }
        _ => {}
    }

    // A dir the record says we never wrote (no recorded sha) is FOREIGN when scannable, else
    // UNSCANNABLE — decided by a full scan (rare; never cache-accelerated, no digest to compare).
    let Some(recorded) = state.materialized_sha.as_deref() else {
        return Ok(match scan::scan(dir) {
            Ok(_) => ScanStatus::Foreign,
            Err(_) => ScanStatus::Unscannable,
        });
    };

    // The FAST path: prove clean-vs-modified from the cached per-file shas, reading only changed
    // files. A cached-walk failure (or the cache disabled) falls through to a full scan below.
    if let Some(cache) = cache {
        let key = dir.to_string_lossy().into_owned();
        let prev = cache
            .placements
            .get(&key)
            .and_then(|b| b.usable_rows(recorded).cloned());
        if let Ok(drift) = scan::drift_digest(dir, prev.as_ref(), racy_ref) {
            let clean = topos_core::digest::to_hex(&drift.bundle_digest) == *recorded;
            let digest = drift.bundle_digest;
            // Refresh the bucket to the freshly observed rows (basis = the recorded sha these rows
            // were compared against); bump the generation when anything moved.
            update_bucket(
                cache.placements.entry(key).or_default(),
                recorded,
                drift.files,
            );
            return Ok(if clean {
                ScanStatus::Clean { digest }
            } else {
                // A draft: the byte-shipping consumers need the exact bytes, so a Modified status
                // always carries the FULL scan (never the digest-only fast path).
                match scan::scan(dir) {
                    Ok(scanned) => ScanStatus::Modified { scanned },
                    Err(_) => ScanStatus::Unscannable,
                }
            });
        }
        // The cached walk hit a hazard (or a read error) — fall through to the full scan, which
        // classifies it identically (Unscannable on the same failure); no empty bucket is left.
    }

    let Ok(scanned) = scan::scan(dir) else {
        return Ok(ScanStatus::Unscannable);
    };
    Ok(
        if topos_core::digest::to_hex(&scanned.bundle_digest) == *recorded {
            ScanStatus::Clean {
                digest: scanned.bundle_digest,
            }
        } else {
            ScanStatus::Modified { scanned }
        },
    )
}

/// Replace a placement bucket's rows with the freshly observed set, tagging them with the recorded
/// sha they were compared against and bumping the generation whenever the basis or rows moved (the
/// visible marker that a swap invalidation, or an edit, was absorbed).
fn update_bucket(
    bucket: &mut stat_cache::PlacementBucket,
    recorded: &str,
    files: std::collections::BTreeMap<String, stat_cache::FileStat>,
) {
    let changed = bucket.basis.as_deref() != Some(recorded) || bucket.files != files;
    if changed {
        bucket.generation = bucket.generation.saturating_add(1);
        bucket.basis = Some(recorded.to_owned());
        bucket.files = files;
    }
}

/// The distinct MODIFIED copies among the scans, deduped by digest (several byte-identical edited
/// copies are ONE logical draft). Returns `(index of the first copy per distinct digest, digest)`.
pub(crate) fn distinct_modified(scans: &[PlacementScan]) -> Vec<(usize, String)> {
    let mut seen: Vec<(usize, String)> = Vec::new();
    for s in scans {
        if let ScanStatus::Modified { scanned } = &s.status {
            let hex = topos_core::digest::to_hex(&scanned.bundle_digest);
            if !seen.iter().any(|(_, d)| *d == hex) {
                seen.push((s.idx, hex));
            }
        }
    }
    seen
}

// ---------------------------------------------------------------------------------------------
// The two detectors, split.
//
// DRAFT: a placement's bytes vs the PRISTINE version (its recorded materialized sha; the lock
// digest decides clean-vs-draft for the kernel) — any difference is edits, and ONE distinct edited
// content per bundle+scope is THE draft.
//
// CONFLICT: placements vs EACH OTHER. Two placements whose contents differ are competitors ONLY
// when neither's bytes equal the other's RECORDED BASELINE. A copy sitting at a sibling's recorded
// baseline is merely STALE BEHIND that sibling's draft — a sync target, never a competitor — so
// the draft resolves to the advanced copy. Only true competitors produce the typed freeze.
// ---------------------------------------------------------------------------------------------

/// One MODIFIED placement's row for the conflict detector: where it is, what it holds, and what
/// its record says was last placed there.
#[derive(Debug, Clone)]
pub(crate) struct DraftRow {
    /// The placement index (into the map's rows).
    pub idx: usize,
    /// sha256 hex of the copy's current bytes.
    pub content: String,
    /// The placement's recorded baseline (its `materialized_sha`), if any.
    pub baseline: Option<String>,
}

/// What the modified copies of one bundle+scope amount to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DraftVerdict {
    /// No edited copy at all.
    NoDraft,
    /// Exactly one ADVANCED content — THE draft. `stale` lists the modified copies sitting behind
    /// it (each at some draft copy's recorded baseline): sync targets, never competitors.
    One { idx: usize, stale: Vec<usize> },
    /// Two or more contents that are true competitors (none explains another as its baseline) —
    /// the typed freeze, one representative index per distinct content.
    Competitors(Vec<usize>),
}

/// The pure conflict/draft classifier over the modified rows (table-tested below). A row is STALE
/// BEHIND when another modified row with different content records this row's content as its
/// baseline; the non-stale rows are the candidate drafts. One distinct candidate content wins as
/// THE draft; several are competitors; a degenerate mutual cycle (each sitting at the other's
/// baseline) fails TOWARD the freeze.
pub(crate) fn resolve_draft_rows(rows: &[DraftRow]) -> DraftVerdict {
    if rows.is_empty() {
        return DraftVerdict::NoDraft;
    }
    let stale = |j: &DraftRow| {
        rows.iter()
            .any(|i| i.content != j.content && i.baseline.as_deref() == Some(j.content.as_str()))
    };
    // One representative row per distinct CANDIDATE content, in placement order.
    let mut candidates: Vec<&DraftRow> = Vec::new();
    for r in rows {
        if !stale(r) && !candidates.iter().any(|c| c.content == r.content) {
            candidates.push(r);
        }
    }
    match candidates.as_slice() {
        // A mutual cycle staled every row: nothing explains the divergence — freeze, listing one
        // representative per distinct content.
        [] => {
            let mut reps: Vec<&DraftRow> = Vec::new();
            for r in rows {
                if !reps.iter().any(|c| c.content == r.content) {
                    reps.push(r);
                }
            }
            DraftVerdict::Competitors(reps.iter().map(|r| r.idx).collect())
        }
        [winner] => DraftVerdict::One {
            idx: winner.idx,
            stale: rows
                .iter()
                .filter(|r| r.content != winner.content)
                .map(|r| r.idx)
                .collect(),
        },
        several => DraftVerdict::Competitors(several.iter().map(|r| r.idx).collect()),
    }
}

/// Classify the scans' modified copies against the map's recorded baselines (the I/O-shaped entry
/// over [`resolve_draft_rows`]).
pub(crate) fn classify_draft(scans: &[PlacementScan], map: &PlacementMap) -> DraftVerdict {
    let rows: Vec<DraftRow> = scans
        .iter()
        .filter_map(|s| match &s.status {
            ScanStatus::Modified { scanned } => Some(DraftRow {
                idx: s.idx,
                content: topos_core::digest::to_hex(&scanned.bundle_digest),
                baseline: map
                    .placement_state
                    .get(s.idx)
                    .and_then(|st| st.materialized_sha.clone()),
            }),
            _ => None,
        })
        .collect();
    resolve_draft_rows(&rows)
}

/// The dir the single-work-tree surfaces (diff / publish / merge) read: the ONE advanced modified
/// copy when the classifier resolves a draft (a copy sitting at a sibling's recorded baseline is
/// stale behind it, never a competitor), else the first materialized placement. TRUE competitors
/// are the typed freeze — nothing to read until reconciled.
///
/// # Errors
/// [`ClientError::PlacementsDiverged`] on true competitors;
/// [`ClientError::Corrupt`] when the map records no placement at all.
pub(crate) fn work_tree_dir(
    ctx: &Ctx<'_>,
    skill_name: &str,
    map: &PlacementMap,
) -> Result<PathBuf, ClientError> {
    let scans = scan_placements(ctx, map)?;
    match classify_draft(&scans, map) {
        DraftVerdict::Competitors(indices) => {
            return Err(placements_diverged(ctx, skill_name, &scans, &indices));
        }
        DraftVerdict::One { idx, .. } => return Ok(scans[idx].dir.clone()),
        DraftVerdict::NoDraft => {}
    }
    // No draft: the first placement that holds our bytes, else the first recorded placement (the
    // classic read surface for an absent working tree — callers report their own absence).
    let first_clean = scans
        .iter()
        .find(|s| matches!(s.status, ScanStatus::Clean { .. }))
        .map(|s| s.dir.clone());
    first_clean
        .or_else(|| map.placements.first().map(PathBuf::from))
        .ok_or_else(|| ClientError::Corrupt("placement map has no placement".into()))
}

/// The typed competitor freeze, with its per-copy disclosure: exactly the TRUE competitors are
/// named (a copy merely stale behind the draft is a sync target, not part of the freeze).
///
/// Each is carried in BOTH spellings the refusal's menu needs — the folder a person reads and the
/// `--dest` value that names it back — through the one shared spelling helper, so the folders the
/// menu prints and the folders its commands accept are the same strings.
pub(crate) fn placements_diverged(
    ctx: &Ctx<'_>,
    skill_name: &str,
    scans: &[PlacementScan],
    competitor_indices: &[usize],
) -> ClientError {
    let copies: Vec<crate::error::DivergedCopy> = scans
        .iter()
        .filter(|s| competitor_indices.contains(&s.idx))
        .map(|s| {
            let sp = crate::ops::dest_select::copy_spellings(ctx, &s.dir);
            crate::error::DivergedCopy {
                display: sp.display,
                dest: sp.dest,
            }
        })
        .collect();
    ClientError::PlacementsDiverged {
        skill: skill_name.to_owned(),
        copies,
        // The scope the frozen copies live in — the layout IS the scope, and every command the
        // refusal offers is spelled for it.
        global: !ctx.layout.is_project_scope(),
    }
}

/// The workspace ADDRESS slug for `workspace_id` (the collision namespace `choose_skill_dir`
/// suffixes with), from the enrolled memberships — best-effort (`None` offline / unenrolled).
pub(crate) fn workspace_slug(ctx: &Ctx<'_>, workspace_id: Option<&str>) -> Option<String> {
    let ws = workspace_id?;
    // The sessions file first (the live identity), then the offline delivery cache's record.
    if let Ok(all) = crate::sessions::read_sessions(ctx.fs, &ctx.layout)
        && let Some(s) = all.sessions.iter().find(|s| s.workspace_id == ws)
    {
        return Some(s.workspace_name.clone());
    }
    crate::sync_status::read(ctx.fs, &ctx.layout)
        .ok()
        .and_then(|st| st.workspaces.get(ws).and_then(|e| e.workspace_name.clone()))
}

/// The plan for an ALREADY-TRACKED skill: naming from its lock, scope + workspace slug from the
/// follow-state. The one entry every re-plan site (sync / reset / go-back / the verbs) calls, so the
/// target set is computed identically everywhere. The engine plans breadth for FOLLOWED skills only
/// — a purely-local skill (adopted in place, never followed) keeps its recorded placement as-is: its
/// dir is the user's own working location, and nothing distributes it.
///
/// **The layout IS the scope** — for a bundle whose dirs the engine owns. A ctx running against a
/// PROJECT's own store plans PROJECT-scoped, the same planner the reconcile hands its project rows.
/// Without that, a targeted verb run on a project-held bundle (a go-back, a reset, an accept from
/// inside the checkout) would re-plan it onto the machine's harness dirs — which the project store's
/// containment rail then refuses outright, so the verb would not merely misplace the bytes, it would
/// fail.
///
/// **The purely-local arm comes first, in BOTH scopes.** A skill adopted in place from a path and
/// never followed has no engine-chosen dir at all: the recorded placement IS the folder the person
/// works in. Re-planning it into the scope's harness dirs would materialize a second copy there and
/// leave the edited original untouched — a reset would report the edits discarded while they sat on
/// disk, unchanged, at the source. Only a followed / workspace-delivered bundle reaches the scope
/// planners below.
pub(crate) fn plan_for_skill(
    ctx: &Ctx<'_>,
    skill_id: &str,
    lock: &Lock,
    prior: &PlacementMap,
) -> PlacementPlan {
    let ws = crate::ops::followed_workspace(ctx, skill_id);
    let slug = workspace_slug(ctx, ws.as_deref());
    // A manifest row that FROZE this bundle's destinations governs the targeted verbs exactly as
    // it governs the reconcile: the same dest planner, never detection — so an `update <name>`,
    // a go-back, or a reset re-plans onto the row's own destinations.
    if let Some(dest) = row_dest_for(ctx, skill_id, lock, ws.as_deref(), prior) {
        return dest_plan(
            ctx,
            skill_id,
            PlacementNaming {
                name: Some(&lock.name),
                workspace_slug: slug.as_deref(),
            },
            &dest,
            ctx.layout.project_root(),
            Some(prior),
            None,
        );
    }
    if ws.is_none() {
        return classic_plan(
            ctx,
            skill_id,
            PlacementNaming {
                name: Some(&lock.name),
                workspace_slug: None,
            },
            Some(prior),
            None,
        );
    }
    if let Some(root) = ctx.layout.project_root() {
        return project_plan(
            ctx,
            root,
            skill_id,
            PlacementNaming {
                name: Some(&lock.name),
                workspace_slug: slug.as_deref(),
            },
            Some(prior),
            None,
        );
    }
    // No adopt probe here: a tracked skill's adoption continuity rides the record — the baseline's
    // adoption reservation (`pre_existing_sha`) keeps [`prior_dir`] answering the adopted dir.
    plan_targets(
        ctx,
        skill_id,
        PlacementNaming {
            name: Some(&lock.name),
            workspace_slug: slug.as_deref(),
        },
        Some(prior),
        None,
    )
}

/// The `dest` field of the manifest row that DEMANDS this bundle in the scope this ctx's layout
/// represents (the project's own `topos.toml` for a project store, the machine's global file
/// otherwise) — the row context every targeted verb already resolved by name. `None` when no row
/// carries one (the detection planners stand), and for an `mcp` bundle (its dest entries are
/// config FILES — the config converge's business, never a dir plan).
fn row_dest_for(
    ctx: &Ctx<'_>,
    skill_id: &str,
    lock: &Lock,
    ws: Option<&str>,
    prior: &PlacementMap,
) -> Option<Vec<String>> {
    use crate::manifest::keys::KeyShape;
    let doc = {
        let (path, scope) = match ctx.layout.project_root() {
            Some(root) => (
                root.join(crate::manifest::MANIFEST_FILE),
                crate::manifest::document::ManifestScope::Project,
            ),
            None => (
                ctx.layout.home().join(crate::manifest::MANIFEST_FILE),
                crate::manifest::document::ManifestScope::Global,
            ),
        };
        let bytes = ctx.fs.read_opt(&path).ok().flatten()?;
        let text = String::from_utf8(bytes).ok()?;
        crate::manifest::document::parse_manifest(&text, scope).ok()?
    };
    // A workspace-delivered bundle: its explicit row, else the channel line that delivers it.
    if let Some(ws_id) = ws {
        // The machine-wide delivery cache (one per machine, in the HOME sidecar — a project ctx's
        // layout is the project store, so resolve the home store from the roots).
        let home_layout = match ctx.layout.project_root() {
            None => ctx.layout.clone(),
            Some(_) => crate::sidecar::Layout::new(&ctx.roots.as_ref()?.home.join(".topos")),
        };
        let cache = crate::sync_status::read(ctx.fs, &home_layout).ok()?;
        let entry = cache.workspaces.get(ws_id)?;
        let (host, workspace) = (entry.host.clone()?, entry.workspace_name.clone()?);
        let ds = entry.delivered.get(skill_id)?;
        // A CONFIG-PLACED bundle's `dest` names config files, not placement folders — and a
        // kind this build does not own places nothing here either.
        if crate::bundle_kind::BundleKind::of_tag(ds.kind.as_deref())
            != Some(crate::bundle_kind::BundleKind::Skill)
        {
            return None;
        }
        let canonical = format!("{host}/{workspace}/{}", ds.name);
        for row in &doc.rows {
            if row.shape.canonical() == canonical {
                return value_dest(&row.value);
            }
        }
        for row in &doc.rows {
            if let KeyShape::Channel {
                host: h,
                workspace: w,
                channel,
            } = &row.shape
                && *h == host
                && *w == workspace
                && ds.via_channels.iter().any(|c| c == channel)
            {
                return value_dest(&row.value);
            }
        }
        return None;
    }
    // A forge import: its own 4-segment row, else the whole-repo line.
    if let Ok(sid) = crate::id::SkillId::parse(skill_id)
        && let Ok(Some(origin)) = crate::doc::read_doc::<crate::ops::OriginDoc>(
            ctx.fs,
            &ctx.layout.published(&sid).origin,
        )
    {
        let member = format!("{}/{}", origin.origin.source, lock.name);
        for row in &doc.rows {
            if row.shape.canonical() == member {
                if row.value.declared_kind() != Some(crate::bundle_kind::BundleKind::Skill) {
                    return None;
                }
                return value_dest(&row.value);
            }
        }
        for row in &doc.rows {
            if row.shape.canonical() == origin.origin.source {
                return value_dest(&row.value);
            }
        }
        return None;
    }
    // A local adopted bundle: the path row whose folder is one of the RECORDED placements.
    for row in &doc.rows {
        let KeyShape::LocalPath { raw } = &row.shape else {
            continue;
        };
        if row.value.declared_kind() != Some(crate::bundle_kind::BundleKind::Skill) {
            continue;
        }
        let base = match ctx.layout.project_root() {
            Some(root) => root.to_path_buf(),
            None => match &ctx.roots {
                Some(roots) => roots.home.clone(),
                None => continue,
            },
        };
        let dir = if let Some(rest) = raw.strip_prefix("~/") {
            match &ctx.roots {
                Some(roots) => roots.home.join(rest),
                None => continue,
            }
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            base.join(raw.trim_start_matches("./"))
        };
        let dir = dir.canonicalize().unwrap_or(dir);
        let hit = prior.placements.iter().any(|p| {
            let p = Path::new(p);
            p.canonicalize().unwrap_or_else(|_| p.to_path_buf()) == dir
        });
        if hit {
            return value_dest(&row.value);
        }
    }
    None
}

/// A parsed row value's `dest`, when it names one (non-empty).
fn value_dest(value: &crate::manifest::document::EntryValue) -> Option<Vec<String>> {
    match value {
        crate::manifest::document::EntryValue::Fields(f) => {
            f.dest.clone().filter(|d| !d.is_empty())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
    use topos_types::HarnessId;

    /// A self-cleaning temp dir (RAII) — a stand-in machine home.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-plc-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An adapter whose placement answer is a dir NO registry row names — the stand-in for a
    /// compiled spelling that has fallen behind the table (a machine-local registry that moved
    /// this harness's skills dir leaves exactly this gap: detection follows the row, and a
    /// placement asking the adapter would not).
    #[derive(Debug)]
    struct AdapterElsewhere {
        elsewhere: PathBuf,
    }
    impl HarnessAdapter for AdapterElsewhere {
        fn id(&self) -> HarnessId {
            HarnessId::ClaudeCode
        }
        fn discover(&self) -> Vec<DiscoveredPlacement> {
            Vec::new()
        }
        fn placement_for(
            &self,
            skill_id: &str,
            _naming: PlacementNaming<'_>,
            _d: Option<&DiscoveredPlacement>,
        ) -> PlacementTarget {
            PlacementTarget {
                dir: self.elsewhere.join(skill_id),
            }
        }
    }

    /// **The active harness's target dir is its registry ROW's, never the compiled adapter's.**
    /// The row is what detection, attribution and every sibling harness's placement already read;
    /// a machine carrying a table that moved this agent's skills dir must move the bytes with it,
    /// and the only way that holds is for one source to name the dir.
    ///
    /// Rooted at a temp home with no agent detected, so the plan takes the classic single-target
    /// path — the one branch that used to hand the question to the adapter outright.
    #[test]
    fn the_active_harnesss_target_dir_comes_from_its_registry_row() {
        let home = Scratch::new("row-home");
        let elsewhere = Scratch::new("row-adapter");
        let harness = AdapterElsewhere {
            elsewhere: elsewhere.0.clone(),
        };
        let fs = crate::fs_seam::RealFs;
        let ids = crate::ids::test_sources::SeqIds::new("p");
        let clock = crate::ids::test_sources::FixedClock(1);
        let plane = crate::plane::InertPlane;
        let follow = crate::plane::InertFollow;
        let ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: "d_test".to_owned(),
            layout: crate::sidecar::Layout::new(&home.0),
            harness: &harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &plane,
            follow: &follow,
            roots: Some(crate::ctx::AgentRoots::new(home.0.clone(), None)),
        };
        let naming = PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some("acme"),
        };

        let plan = plan_targets(&ctx, "topos_aabbccdd", naming, None, None);
        let dirs: Vec<PathBuf> = plan.dirs().map(|d| d.dir.clone()).collect();

        // The row's own user skills root, resolved the one way (env overrides honored), plus the
        // ONE naming discipline's choice of folder inside it.
        let root = registry::skills_root(
            HarnessId::ClaudeCode.slug(),
            registry::SkillScope::User,
            &home.0,
            None,
        )
        .expect("claude-code has a user skills root");
        assert_eq!(dirs, vec![root.join("deploy")]);
        assert!(
            !dirs.iter().any(|d| d.starts_with(&elsewhere.0)),
            "the adapter's compiled dir is not a placement: {dirs:?}"
        );
    }

    /// A one-entry never-materialized map (a reservation) whose slot carries a recorded adoption.
    fn reservation_map(dir: &str, pre_existing: Option<&str>) -> PlacementMap {
        PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: vec![dir.to_owned()],
            applied_commit: "0".repeat(64),
            materialized_sha: "0".repeat(64),
            placement_state: vec![PlacementState {
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
                materialized_sha: None,
                pre_existing_sha: pre_existing.map(str::to_owned),
                swap_capability: SwapCapability::AtomicExchange,
                adopted_source: false,
                claim: None,
            }],
            harness: None,
            harness_slug: None,
        }
    }

    /// Replacing a stale reservation's dir RESETS the slot's state (kind/agent kept): the old
    /// adoption's `pre_existing_sha` described the OLD occupant and must never migrate to the new
    /// dir, and the cached swap capability was probed against the old parent.
    #[test]
    fn a_replaced_stale_reservation_resets_the_slot_state() {
        let prior = reservation_map("/skills/deploy", Some(&"a".repeat(64)));
        let plan = PlacementPlan {
            targets: vec![PlannedTarget::Dir(DirTarget {
                dir: PathBuf::from("/skills/deploy-acme"),
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
            })],
            ..PlacementPlan::default()
        };
        let next = reconcile_map(&prior, &plan);
        assert_eq!(next.placements, vec!["/skills/deploy-acme".to_owned()]);
        let st = &next.placement_state[0];
        assert_eq!(st.kind, PlacementKind::Native);
        assert_eq!(st.agent.as_deref(), Some("claude-code"));
        assert!(st.materialized_sha.is_none());
        assert!(st.pre_existing_sha.is_none(), "no adoption record migrates");
        assert_eq!(st.swap_capability, SwapCapability::Unsupported);
    }

    /// The conflict detector's whole decision table, over pure rows: content-equal copies are one
    /// draft; a copy at a sibling's recorded baseline is stale behind it (draft = the advanced
    /// one); true divergence freezes; three-placement mixes resolve per the same rules.
    #[test]
    fn the_draft_and_conflict_detectors_split_by_baseline() {
        let row = |idx: usize, content: &str, baseline: Option<&str>| DraftRow {
            idx,
            content: content.to_owned(),
            baseline: baseline.map(str::to_owned),
        };
        let cases: Vec<(&str, Vec<DraftRow>, DraftVerdict)> = vec![
            ("no modified copy", vec![], DraftVerdict::NoDraft),
            (
                "one edited copy is the draft",
                vec![row(0, "d1", Some("base"))],
                DraftVerdict::One {
                    idx: 0,
                    stale: vec![],
                },
            ),
            (
                "content-equal edited copies are one draft",
                vec![row(0, "d1", Some("base")), row(2, "d1", Some("base"))],
                DraftVerdict::One {
                    idx: 0,
                    stale: vec![],
                },
            ),
            (
                "a copy at the sibling's baseline is stale behind, not a competitor",
                vec![row(0, "d2", Some("d1")), row(1, "d1", Some("base"))],
                DraftVerdict::One {
                    idx: 0,
                    stale: vec![1],
                },
            ),
            (
                "true divergence (neither equals the other's baseline) freezes",
                vec![row(0, "d1", Some("base")), row(1, "d2", Some("base"))],
                DraftVerdict::Competitors(vec![0, 1]),
            ),
            (
                "three placements: an advanced draft, its base copy, a content twin",
                vec![
                    row(0, "d1", Some("base")),
                    row(1, "d2", Some("d1")),
                    row(2, "d1", Some("base")),
                ],
                DraftVerdict::One {
                    idx: 1,
                    stale: vec![0, 2],
                },
            ),
            (
                "three placements: two true competitors still freeze past a stale copy",
                vec![
                    row(0, "d2", Some("d1")),
                    row(1, "d1", Some("base")),
                    row(2, "d3", Some("base")),
                ],
                DraftVerdict::Competitors(vec![0, 2]),
            ),
            (
                "a mutual baseline cycle fails toward the freeze",
                vec![row(0, "d1", Some("d2")), row(1, "d2", Some("d1"))],
                DraftVerdict::Competitors(vec![0, 1]),
            ),
            (
                "a copy with no recorded baseline can still be the draft",
                vec![row(0, "d2", None), row(1, "d1", Some("d2"))],
                DraftVerdict::One {
                    idx: 1,
                    stale: vec![0],
                },
            ),
        ];
        for (what, rows, want) in cases {
            assert_eq!(resolve_draft_rows(&rows), want, "{what}");
        }
    }

    /// The same-dir plan (a still-valid reservation) keeps the slot verbatim — the reset fires
    /// only on an actual replacement.
    #[test]
    fn an_unchanged_reservation_keeps_its_recorded_state() {
        let prior = reservation_map("/skills/deploy", Some(&"a".repeat(64)));
        let plan = PlacementPlan {
            targets: vec![PlannedTarget::Dir(DirTarget {
                dir: PathBuf::from("/skills/deploy"),
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
            })],
            ..PlacementPlan::default()
        };
        let next = reconcile_map(&prior, &plan);
        assert_eq!(next.placements, vec!["/skills/deploy".to_owned()]);
        assert_eq!(
            next.placement_state[0].pre_existing_sha.as_deref(),
            Some("a".repeat(64).as_str())
        );
    }

    /// THE ONE OUTCOME VOCABULARY, derived. Both target shapes reach it through the same drift
    /// projection, so a folder and a config entry cannot name one outcome two different ways —
    /// which is exactly what they used to do (dirs through the row's action, entries through a
    /// free-form per-agent string).
    #[test]
    fn every_outcome_is_derived_from_the_record_and_the_write() {
        use topos_harness::mcp::EntryState;
        use topos_types::results::TargetOutcome as T;

        // The DIR side: what the scan found, plus whether this run wrote.
        assert_eq!(Drift::Absent.outcome(true), T::Created);
        assert_eq!(Drift::Clean.outcome(true), T::Refreshed);
        assert_eq!(Drift::Clean.outcome(false), T::Current);
        assert_eq!(Drift::Absent.outcome(false), T::Removed);
        // Never written over, so the record's word wins whatever the caller claims.
        for wrote in [true, false] {
            assert_eq!(Drift::Modified.outcome(wrote), T::Drifted);
            assert_eq!(Drift::Foreign.outcome(wrote), T::Conflicting);
            assert_eq!(Drift::Unscannable.outcome(wrote), T::Unprovable);
        }

        // The ENTRY side lands on the SAME set, through the same projection. The two write
        // outcomes are the ones a person most needs told apart: a first placement and a repair
        // of a hand-deleted entry are both `created`, while rewriting our own recorded entry is
        // `refreshed`.
        let entry = |st: EntryState| {
            let wrote = matches!(st, EntryState::PlacedNew | EntryState::Updated);
            Drift::of_entry(st).outcome(wrote)
        };
        assert_eq!(entry(EntryState::PlacedNew), T::Created);
        assert_eq!(entry(EntryState::Updated), T::Refreshed);
        assert_eq!(entry(EntryState::Current), T::Current);
        assert_eq!(entry(EntryState::Drifted), T::Drifted);
        assert_eq!(entry(EntryState::Foreign), T::Conflicting);
        assert_eq!(entry(EntryState::Removed), T::Removed);

        // `wrote()` is the ONE rule for "this run changed something here" — the two outcomes that
        // wrote, and no others.
        for (o, wrote) in [
            (T::Created, true),
            (T::Refreshed, true),
            (T::Current, false),
            (T::Drifted, false),
            (T::Conflicting, false),
            (T::Unprovable, false),
            (T::Removed, false),
            (T::Withheld, false),
        ] {
            assert_eq!(o.wrote(), wrote, "{o:?}");
            // Every outcome has a word, and no two share one — a person meets one name per
            // outcome, on either target shape.
            assert!(!o.word().is_empty(), "{o:?}");
        }
        let mut words: Vec<&str> = [
            T::Created,
            T::Refreshed,
            T::Current,
            T::Drifted,
            T::Conflicting,
            T::Unprovable,
            T::Removed,
            T::Withheld,
        ]
        .iter()
        .map(|o| o.word())
        .collect();
        words.sort_unstable();
        let before = words.len();
        words.dedup();
        assert_eq!(
            words.len(),
            before,
            "two outcomes share one word: {words:?}"
        );
    }
}
