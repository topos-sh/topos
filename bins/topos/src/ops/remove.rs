//! `remove [SKILL]... [-a <agent>]...` — take skills off THIS machine, two-phase (describe → `--yes`).
//!
//! Three shapes, all byte-honest about what survives:
//! - a FOLLOWED skill → a per-device **exclusion** (`PUT exclusions/{skill}`): delivery stops on THIS
//!   device, the person keeps following it (every other device still receives it), and the local copy is
//!   kept as a frozen copy — the agent dirs are cleaned (any draft snapshotted first), never the sidecar
//!   bytes. Nothing returns at the next sync; `topos follow <name>` re-attaches. This is NOT unfollow —
//!   stopping a skill everywhere is `topos unfollow`.
//! - a TRACKED, never-published LOCAL skill → a **permanent** delete: no other copy exists, so the agent
//!   dirs AND the sidecar entry go.
//! - an UNTRACKED local copy sitting in an agent dir (`<name>@<agent>`, or `-a <agent>` scoped) → a
//!   **permanent** delete of that directory (topos never adopted it — deleting it is the only removal).
//!
//! Multi-skill positional; resolve ALL-OR-NONE (a batch either resolves every target or applies nothing).
//! `-a/--agent` on a FOLLOWED skill is the PER-AGENT exclusion (placement policy — one shared
//! implementation with `unfollow --agent`, see [`super::agent_scope`]); on untracked locals it keeps
//! its classic discovery-scoping semantics, and a bare followed removal stays device-wide.

use std::path::PathBuf;

use topos_types::results::{RemoveData, RemoveItem, RemoveKind};

use super::DiscoveryRoots;
use super::follow::{DirectoryConnect, build_universe_via};
use super::pull::{WithdrawReason, snapshot_and_clean};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::resolve::{self, ParsedTarget, Resolution};
use crate::{doc, enroll};

/// The seams `remove` needs — the directory connector builds the resolution universe and writes the
/// per-device exclusion row.
pub(crate) struct RemoveConnectors<'a> {
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
    AgentScope(super::agent_scope::AgentScopeOutcome),
}

/// One resolved removal, pre-apply.
enum Removal {
    /// A followed skill → a per-device exclusion.
    Followed {
        workspace_id: String,
        skill_id: String,
        name: String,
    },
    /// A tracked, never-published local skill → a permanent delete (sidecar entry included).
    TrackedLocal {
        skill_id: String,
        name: String,
        dirs: Vec<PathBuf>,
    },
    /// An untracked copy in an agent dir → a permanent delete of that directory.
    Untracked { name: String, dir: PathBuf },
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
    // `remove <followed> --agent <slug>` is the PER-AGENT exclusion — one shared implementation with
    // `unfollow --agent` (placement policy; the subscription and the whole-device exclusion row are
    // untouched). It engages only when every bare target is a FOLLOWED tracked skill; the classic
    // `-a` semantics for untracked/local copies (and `-a '*'`) stay exactly as they were.
    if !agents.is_empty()
        && !agents.iter().any(|a| a == "*")
        && targets.iter().all(|t| {
            !t.contains('@')
                && !t.contains('/')
                && super::resolve_skill(ctx, t)
                    .is_ok_and(|(sid, _)| super::followed_workspace(ctx, sid.as_str()).is_some())
        })
    {
        return Ok(RemoveOutcome::AgentScope(
            super::agent_scope::exclude_agents(ctx, "remove", targets, agents, None, yes)?,
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

    let (base_url, universe) = build_universe_via(ctx, connectors.directory)?;

    // Resolve ALL-OR-NONE.
    let mut removals = Vec::with_capacity(targets.len());
    for token in targets {
        removals.push(classify(ctx, &universe, roots, agent_filter, token)?);
    }

    let items: Vec<RemoveItem> = removals.iter().map(describe_item).collect();

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
            },
            yes_argv,
        });
    }

    // ---- APPLY (`--yes`) ----
    let needs_server = removals
        .iter()
        .any(|r| matches!(r, Removal::Followed { .. }));
    let directory = match (&base_url, needs_server) {
        (Some(b), true) => Some((connectors.directory)(b)),
        _ => None,
    };
    for removal in &removals {
        match removal {
            Removal::Followed {
                workspace_id,
                skill_id,
                ..
            } => {
                let directory = directory.as_deref().ok_or_else(|| {
                    ClientError::Enrollment("not enrolled; nothing to remove".into())
                })?;
                // 1) The server exclusion row — delivery stops on THIS device (the person keeps following).
                directory.exclude_device(workspace_id, skill_id)?;
                let sid = SkillId::parse(skill_id)?;
                // 2) Snapshot any draft, then clean the agent dirs — KEEPING every sidecar byte —
                //    and reset the sync state to the never-received baseline, so a later `follow`
                //    that lifts the exclusion actually re-materializes the bytes (without the
                //    reset, `applied == observed` and the absent placement would read as "already
                //    current" forever).
                let prior = snapshot_and_clean(ctx, &sid, WithdrawReason::RemoveExclusion)?;
                super::pull::reset_to_never_received(ctx, &sid, prior.as_ref())?;
                // 3) The local exclusion cause marker (for `list`, offline).
                enroll::set_excluded(ctx.fs, &ctx.layout, skill_id, true)?;
            }
            Removal::TrackedLocal { skill_id, dirs, .. } => {
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
        }
    }
    Ok(RemoveOutcome::Applied(RemoveData {
        items,
        applied: true,
    }))
}

/// Classify ONE target: a followed catalog skill (exclusion), a tracked-local (permanent), or an
/// untracked agent-dir copy (permanent). A workspace / channel target is refused toward the right verb.
fn classify(
    ctx: &Ctx<'_>,
    universe: &[resolve::WorkspaceNames],
    roots: Option<&DiscoveryRoots>,
    agent_filter: Option<&str>,
    token: &str,
) -> Result<Removal, ClientError> {
    let parsed = resolve::parse_target(token)?;
    // An explicit `<name>@<agent>` names an untracked agent-dir copy — resolve it through discovery
    // (never a plane resource).
    if let ParsedTarget::LocalAt { name, agent } = &parsed {
        return untracked(ctx, roots, Some(agent.as_str()), name);
    }
    // Resolve against the plane universe (SKILLS scope). A channel / workspace match is refused toward
    // the verb that acts on it.
    match resolve::resolve_one(universe, &parsed, resolve::KindScope::SKILLS)? {
        Some(Resolution::Resource {
            workspace_id,
            skill_id,
            name,
            ..
        }) => {
            let skill_id = skill_id
                .ok_or_else(|| ClientError::WireInvalid("a resolved skill carried no id".into()))?;
            Ok(Removal::Followed {
                workspace_id,
                skill_id,
                name,
            })
        }
        Some(Resolution::Workspace { workspace_name, .. }) => {
            Err(ClientError::InvalidArgument(format!(
                "'{workspace_name}' is a workspace, not a skill — `remove` takes skills off this \
                 device; to stop deliveries use `topos unfollow`"
            )))
        }
        // Not a plane resource: the local paths — a tracked skill you `add`ed, or an untracked agent-dir
        // copy discovery knows.
        None => match super::resolve_skill(ctx, token) {
            Ok((sid, lock)) => tracked_or_followed(ctx, sid, lock.name),
            Err(ClientError::NoSuchSkill { .. }) => untracked(ctx, roots, agent_filter, token),
            Err(e) => Err(e),
        },
    }
}

/// A locally-tracked skill resolved by name: a followed one (with a workspace row) becomes an exclusion
/// even when the universe read did not surface it (offline / a since-removed catalog row); a
/// never-followed one is a permanent local delete.
fn tracked_or_followed(ctx: &Ctx<'_>, sid: SkillId, name: String) -> Result<Removal, ClientError> {
    let skill_id = sid.as_str().to_owned();
    if let Some(ws) = super::followed_workspace(ctx, &skill_id) {
        return Ok(Removal::Followed {
            workspace_id: ws,
            skill_id,
            name,
        });
    }
    // A purely-local skill — the placement dirs to delete come from its map.
    let sp = ctx.layout.published(&sid);
    let dirs = doc::read_map(ctx.fs, &sp.map)?
        .map(|m| m.placements.iter().map(PathBuf::from).collect())
        .unwrap_or_default();
    Ok(Removal::TrackedLocal {
        skill_id,
        name,
        dirs,
    })
}

/// Resolve an untracked agent-dir copy by name (optionally scoped to one agent) through the same
/// discovery `add` uses. A missing `$HOME` (no discovery) or a genuine miss is the uniform not-found.
fn untracked(
    ctx: &Ctx<'_>,
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
            Ok((sid, lock)) => tracked_or_followed(ctx, sid, lock.name),
            Err(_) => Err(resolve::not_found(name)),
        },
        Err(ClientError::NoUntrackedSkill { .. }) | Err(ClientError::HarnessNotFound(_)) => {
            Err(resolve::not_found(name))
        }
        Err(e) => Err(e),
    }
}

/// The describe/apply row for one removal (the boundary a followed removal keeps vs a permanent delete).
fn describe_item(removal: &Removal) -> RemoveItem {
    match removal {
        Removal::Followed {
            workspace_id, name, ..
        } => RemoveItem {
            name: name.clone(),
            kind: RemoveKind::FollowedExclusion,
            workspace_id: Some(workspace_id.clone()),
            agent_dirs: Vec::new(),
            bytes_kept: true,
        },
        Removal::TrackedLocal { name, dirs, .. } => RemoveItem {
            name: name.clone(),
            kind: RemoveKind::TrackedLocalPermanent,
            workspace_id: None,
            agent_dirs: dirs.iter().map(|d| d.display().to_string()).collect(),
            bytes_kept: false,
        },
        Removal::Untracked { name, dir } => RemoveItem {
            name: name.clone(),
            kind: RemoveKind::UntrackedLocal,
            workspace_id: None,
            agent_dirs: vec![dir.display().to_string()],
            bytes_kept: false,
        },
    }
}
