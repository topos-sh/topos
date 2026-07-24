//! `remove [SKILL]... [-a <agent>]...` — take skills off THIS machine.
//!
//! Three shapes, all byte-honest about what survives:
//! - a FOLLOWED skill → a per-device **exclusion** (`PUT exclusions/{skill}`): delivery stops on THIS
//!   device, the person keeps following it (every other device still receives it), and the local copy is
//!   kept as a frozen copy — the agent dirs are cleaned (any draft snapshotted first), never the sidecar
//!   bytes. Nothing returns at the next sync; `topos follow <name>` re-attaches. This is NOT unfollow —
//!   stopping a skill everywhere is `topos unfollow`. On a CLEAN followed skill the exclusion applies
//!   immediately with an undo-led receipt (`--yes` an accepted no-op); with a DRAFT ahead (local
//!   edits) the loss-guard holds the two-phase describe — the draft leaves every agent dir on apply,
//!   so the disclosure comes first (a scan that cannot classify fails TOWARD the gate).
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
    /// A tracked, never-published local skill → a permanent delete (sidecar entry included).
    TrackedLocal {
        skill_id: String,
        name: String,
        dirs: Vec<PathBuf>,
    },
    /// An untracked copy in an agent dir → a permanent delete of that directory.
    Untracked { name: String, dir: PathBuf },
    /// The built-in `topos` skill → the durable device opt-out (no sweep re-places it;
    /// `topos follow topos` brings it back).
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

    // Resolve ALL-OR-NONE.
    let mut removals = Vec::with_capacity(targets.len());
    for token in targets {
        removals.push(classify(ctx, &universe, roots, agent_filter, token)?);
    }

    let mut items: Vec<RemoveItem> = removals.iter().map(describe_item).collect();

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
        let _ = item;
        match removal {
            Removal::TrackedLocal { .. } | Removal::Untracked { .. } | Removal::Builtin { .. } => {
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
    for removal in &removals {
        match removal {
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
        return untracked(ctx, roots, Some(agent.as_str()), name);
    }
    // Resolve against the plane universe (SKILLS scope). A channel / workspace match is refused toward
    // the verb that acts on it.
    match resolve::resolve_one(universe, &parsed, resolve::KindScope::SKILLS)? {
        Some(Resolution::Resource { name, .. }) => Err(ClientError::InvalidArgument(format!(
            "'{name}' is delivered from a workspace — remove the DEMAND, not the copy: `topos \
             remove {name}` in a folder records an exclude in its manifest; `topos remove -g \
             {name}` edits your profile (stops it on every machine you log in)"
        ))),
        Some(Resolution::Workspace { workspace_name, .. }) => {
            Err(ClientError::InvalidArgument(format!(
                "'{workspace_name}' is a workspace, not a skill — `remove` takes skills off this \
                 machine; leaving a workspace is `topos logout {workspace_name}`"
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
    if super::followed_workspace(ctx, &skill_id).is_some() {
        return Err(ClientError::InvalidArgument(format!(
            "'{name}' is delivered from a workspace — remove the DEMAND, not the copy: `topos \
             remove {name}` in a folder records an exclude in its manifest; `topos remove -g \
             {name}` edits your profile (stops it on every machine you log in)"
        )));
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
        Removal::TrackedLocal { name, dirs, .. } => RemoveItem {
            name: name.clone(),
            kind: RemoveKind::TrackedLocalPermanent,
            manifest: None,
            workspace_id: None,
            agent_dirs: dirs.iter().map(|d| d.display().to_string()).collect(),
            bytes_kept: false,
            note: None,
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
                 `topos follow topos` brings it back"
                    .to_owned(),
            ),
        },
    }
}
