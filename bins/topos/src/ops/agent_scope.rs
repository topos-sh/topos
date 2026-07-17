//! The `--agent` scope verbs — DEVICE-LOCAL placement policy for a followed skill, two-phase
//! (describe → `--yes`), fully offline (the plane is never told; the subscription never moves):
//!
//! - **`follow <skill> --agent <slug>…`** ([`set_scope`]) — record the include-list (placement lands
//!   in exactly those agents' native dirs; `'*'` clears it back to unscoped), then reconcile the
//!   placements: dirs leaving the set are cleaned snapshot-first, new native dirs land from the
//!   local store.
//! - **`unfollow <skill> --agent <slug>…`** and **`remove <skill> --agent <slug>…`** on a followed
//!   skill ([`exclude_agents`]) — the SAME implementation (one function, two spellings): record the
//!   per-agent exclusion and clean exactly that agent's placement (snapshot-first). The subscription
//!   is untouched — the person keeps following the skill, every other device keeps receiving it,
//!   and the whole-device exclusion stays what bare `remove` does.
//!
//! Because a shared cross-agent dir cannot express narrowing, ANY scope (an include-list or an
//! exclusion) flips the skill to native-only placement: the shared copy is cleaned (snapshot-first)
//! and native copies land for the remaining in-scope detected agents. Placements whose harness is
//! merely UNDETECTED are never cleaned by a scope change (detection loss alone never deletes bytes).

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;
use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_harness::coverage;
use topos_types::persisted::{Lock, PlacementKind, PlacementMap};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::placement::{self, AgentScope, ScanStatus};
use crate::{doc, enroll, sidecar};

use super::sync_engine;

/// One skill's scope-change disclosure: what lands, what is cleaned, what stays.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentScopeItem {
    pub skill: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Dirs the change REMOVES (cleaned snapshot-first; the bytes stay recoverable in the sidecar).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cleaned: Vec<String>,
    /// Dirs the change ADDS (native copies landing from the local store on apply).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Dirs the change KEEPS maintaining.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// Honest per-agent notes: an undetected (but known) slug, a docs-level shared-coverage claim.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The `--agent` verbs' describe/apply payload.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentScopeData {
    /// `scope` (`follow --agent`) or `exclude` (`unfollow`/`remove` `--agent`).
    pub action: String,
    /// The slugs the invocation named (`[]` for the `'*'` clear).
    pub agents: Vec<String>,
    pub items: Vec<AgentScopeItem>,
    /// The standing constant: the subscription is untouched — this device's placement only.
    pub subscription_note: String,
    pub applied: bool,
}

/// The two-phase outcome.
#[derive(Debug)]
pub(crate) enum AgentScopeOutcome {
    Described {
        data: AgentScopeData,
        yes_argv: Vec<String>,
    },
    Applied(AgentScopeData),
}

/// `unfollow <skill> --agent <slug>…` == `remove <skill> --agent <slug>…` on a followed skill — the
/// ONE shared implementation: record the per-agent exclusions and clean exactly those agents'
/// placements (snapshot-first). `verb` names the calling spelling for the paste-ready `--yes` argv.
///
/// # Errors
/// [`ClientError::InvalidArgument`] on an unknown slug (naming the valid ones), a `'*'` (the
/// whole-device exclusion is bare `remove`), or a target that is not a followed skill; resolution
/// errors; a store/io failure on apply.
pub(crate) fn exclude_agents(
    ctx: &Ctx<'_>,
    verb: &str,
    targets: &[String],
    agents: &[String],
    workspace: Option<&str>,
    yes: bool,
) -> Result<AgentScopeOutcome, ClientError> {
    if agents.iter().any(|a| a == "*") {
        return Err(ClientError::InvalidArgument(
            "`--agent '*'` does not exclude — taking a skill off EVERY agent on this device is \
             `topos remove <skill>` (the whole-device exclusion)"
                .into(),
        ));
    }
    let undetected = placement::validate_agent_slugs(ctx, agents)?;
    let resolved = resolve_followed(ctx, targets, workspace, verb)?;

    let mut items = Vec::with_capacity(resolved.len());
    for (sid, lock, ws) in &resolved {
        let map = sync_engine::read_map_required(ctx, &ctx.layout.published(sid))?;
        let (cur_agents, cur_excluded) = scope_state(ctx, sid.as_str())?;
        // The hypothetical post-exclusion scope: the named slugs join the exclusions and leave the
        // include-list (the same folding `enroll::add_excluded_agents` applies durably).
        let next_excluded: Vec<String> = cur_excluded
            .iter()
            .chain(agents.iter())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let next_agents: Vec<String> = cur_agents
            .iter()
            .filter(|a| !agents.contains(a))
            .cloned()
            .collect();
        let mut item = plan_item(
            ctx,
            sid,
            lock,
            ws.clone(),
            &map,
            AgentScope {
                agents: &next_agents,
                excluded: &next_excluded,
            },
        );
        for slug in &undetected {
            item.notes.push(format!(
                "'{slug}' is not detected on this machine — the exclusion is recorded and engages \
                 if it appears"
            ));
        }
        items.push(item);
    }

    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), verb.to_owned()];
        yes_argv.extend(targets.iter().cloned());
        for a in agents {
            yes_argv.push("--agent".to_owned());
            yes_argv.push(a.clone());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(AgentScopeOutcome::Described {
            data: data(agents, items, "exclude", false),
            yes_argv,
        });
    }

    // ---- APPLY (`--yes`) ---- record the exclusions, then reconcile the placements. The scope is
    // re-derived per skill from the CURRENT follow-state folded with this verb's slugs (the seam on
    // `ctx` predates the write we just made, so the fold is explicit).
    for (sid, lock, _) in &resolved {
        if super::builtin::is_builtin(sid.as_str()) {
            super::builtin::add_excluded(ctx, agents)?;
        } else {
            enroll::add_excluded_agents(ctx.fs, &ctx.layout, sid.as_str(), agents)?;
        }
        let (cur_agents, cur_excluded) = scope_state(ctx, sid.as_str())?;
        let next_excluded: Vec<String> = cur_excluded
            .iter()
            .chain(agents.iter())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let next_agents: Vec<String> = cur_agents
            .iter()
            .filter(|a| !agents.contains(a))
            .cloned()
            .collect();
        apply_scope_change(
            ctx,
            sid,
            lock,
            AgentScope {
                agents: &next_agents,
                excluded: &next_excluded,
            },
        )?;
    }
    Ok(AgentScopeOutcome::Applied(data(
        agents, items, "exclude", true,
    )))
}

/// `follow <skill> --agent <slug>…` on an ALREADY-followed skill — the scope UPDATE: replace the
/// include-list (`'*'` clears it back to unscoped; naming a slug also re-includes a previously
/// excluded one), then reconcile the placements two-phase.
///
/// # Errors
/// As [`exclude_agents`], minus the `'*'` refusal (here it is the documented clear).
pub(crate) fn set_scope(
    ctx: &Ctx<'_>,
    targets: &[String],
    agents: &[String],
    workspace: Option<&str>,
    yes: bool,
) -> Result<AgentScopeOutcome, ClientError> {
    let clear = agents.iter().any(|a| a == "*");
    if clear && agents.len() > 1 {
        return Err(ClientError::InvalidArgument(
            "`--agent '*'` clears the include-list back to unscoped — pass it alone, not with \
             other agents"
                .into(),
        ));
    }
    let next_agents: Vec<String> = if clear { Vec::new() } else { agents.to_vec() };
    let undetected = placement::validate_agent_slugs(ctx, &next_agents)?;
    let resolved = resolve_followed(ctx, targets, workspace, "follow")?;

    let mut items = Vec::with_capacity(resolved.len());
    for (sid, lock, ws) in &resolved {
        let map = sync_engine::read_map_required(ctx, &ctx.layout.published(sid))?;
        let (_, cur_excluded) = scope_state(ctx, sid.as_str())?;
        // Naming an agent re-includes it (the durable write folds the same way).
        let next_excluded: Vec<String> = cur_excluded
            .iter()
            .filter(|e| !next_agents.contains(e))
            .cloned()
            .collect();
        let mut item = plan_item(
            ctx,
            sid,
            lock,
            ws.clone(),
            &map,
            AgentScope {
                agents: &next_agents,
                excluded: &next_excluded,
            },
        );
        for slug in &undetected {
            item.notes.push(format!(
                "'{slug}' is not detected on this machine — placement engages when the agent is \
                 detected"
            ));
        }
        items.push(item);
    }

    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), "follow".to_owned()];
        yes_argv.extend(targets.iter().cloned());
        for a in agents {
            yes_argv.push("--agent".to_owned());
            yes_argv.push(a.clone());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(AgentScopeOutcome::Described {
            data: data(&next_agents, items, "scope", false),
            yes_argv,
        });
    }

    for (sid, lock, _) in &resolved {
        if super::builtin::is_builtin(sid.as_str()) {
            super::builtin::set_agents(ctx, &next_agents)?;
        } else {
            enroll::set_agent_scope(ctx.fs, &ctx.layout, sid.as_str(), &next_agents)?;
        }
        let (_, cur_excluded) = scope_state(ctx, sid.as_str())?;
        let next_excluded: Vec<String> = cur_excluded
            .iter()
            .filter(|e| !next_agents.contains(e))
            .cloned()
            .collect();
        apply_scope_change(
            ctx,
            sid,
            lock,
            AgentScope {
                agents: &next_agents,
                excluded: &next_excluded,
            },
        )?;
    }
    Ok(AgentScopeOutcome::Applied(data(
        &next_agents,
        items,
        "scope",
        true,
    )))
}

fn data(
    agents: &[String],
    items: Vec<AgentScopeItem>,
    action: &str,
    applied: bool,
) -> AgentScopeData {
    AgentScopeData {
        action: action.to_owned(),
        agents: agents.iter().filter(|a| *a != "*").cloned().collect(),
        items,
        subscription_note: "the subscription is untouched — you keep following the skill, other \
                            devices keep receiving it; this changes where THIS device places the \
                            bytes"
            .to_owned(),
        applied,
    }
}

/// One skill's device-local scope: the built-in's rides `state/builtin.json`; a followed skill's
/// rides its `follows.json` row.
fn scope_state(ctx: &Ctx<'_>, sid: &str) -> Result<(Vec<String>, Vec<String>), ClientError> {
    if super::builtin::is_builtin(sid) {
        super::builtin::current_scope(ctx)
    } else {
        Ok(placement::scope_of(ctx, sid))
    }
}

/// Resolve every target to a FOLLOWED tracked skill (all-or-none). The `--agent` verbs are placement
/// policy for a followed skill; an untracked agent-dir copy keeps `remove`'s classic `-a` semantics,
/// and a never-followed local skill has no delivery to scope. The BUILT-IN skill is the one
/// non-followed target the scope verbs accept — its placement is scoped exactly the same way.
fn resolve_followed(
    ctx: &Ctx<'_>,
    targets: &[String],
    workspace: Option<&str>,
    verb: &str,
) -> Result<Vec<(SkillId, Lock, Option<String>)>, ClientError> {
    if targets.is_empty() {
        return Err(ClientError::InvalidArgument(format!(
            "`{verb} --agent` needs a followed skill name"
        )));
    }
    let mut out = Vec::with_capacity(targets.len());
    for token in targets {
        if super::builtin::is_builtin(token) {
            let sid = SkillId::parse(token)?;
            let lock: Lock =
                doc::read_doc(ctx.fs, &ctx.layout.published(&sid).lock)?.ok_or_else(|| {
                    ClientError::InvalidArgument(
                        "the built-in topos skill is not on this machine — `topos follow topos` \
                         places it first"
                            .into(),
                    )
                })?;
            out.push((sid, lock, None));
            continue;
        }
        let (sid, lock) = super::resolve_skill_in_workspace(ctx, token, workspace)?;
        let ws = super::followed_workspace(ctx, sid.as_str());
        if ws.is_none() {
            return Err(ClientError::InvalidArgument(format!(
                "'{token}' is not a followed skill — `--agent` scopes where a FOLLOWED skill's \
                 bytes land; an untracked copy in one agent's dir is `topos remove \
                 {token}@<agent>`"
            )));
        }
        out.push((sid, lock, ws));
    }
    Ok(out)
}

/// The describe row for one skill under a hypothetical scope: the plan diff against the recorded
/// placements — what would be cleaned, added, kept — plus the shared-coverage disclosure.
fn plan_item(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    lock: &Lock,
    workspace_id: Option<String>,
    map: &PlacementMap,
    scope: AgentScope<'_>,
) -> AgentScopeItem {
    let slug = placement::workspace_slug(ctx, workspace_id.as_deref());
    let plan = placement::plan_targets(
        ctx,
        sid.as_str(),
        topos_harness::PlacementNaming {
            name: Some(&lock.name),
            workspace_slug: slug.as_deref(),
        },
        scope,
        Some(map),
    );
    let (cleaned_idx, _) = partition_leaving(ctx, map, &plan, scope);
    let cleaned: Vec<String> = cleaned_idx
        .iter()
        .map(|&i| map.placements[i].clone())
        .collect();
    // Each planned dir is labeled shared-vs-native so the describe names WHERE, not just a path.
    let label = |t: &placement::PlannedTarget| match (&t.kind, &t.agent) {
        (PlacementKind::Shared, _) => format!("{} (shared)", t.dir.display()),
        (_, Some(a)) => format!("{} (native: {a})", t.dir.display()),
        _ => t.dir.display().to_string(),
    };
    let added: Vec<String> = plan
        .targets
        .iter()
        .filter(|t| !map.placements.iter().any(|p| Path::new(p) == t.dir))
        .map(label)
        .collect();
    let kept: Vec<String> = plan
        .targets
        .iter()
        .filter(|t| map.placements.iter().any(|p| Path::new(p) == t.dir))
        .map(label)
        .collect();
    let mut notes = Vec::new();
    for c in &plan.shared_covers {
        if c.docs_level {
            // Honest provenance: this agent's shared-dir coverage rests on vendor docs, not a live probe.
            notes.push(format!(
                "the shared dir covers {} (per vendor docs — not yet verified against a live build)",
                c.slug
            ));
        }
    }
    AgentScopeItem {
        skill: lock.name.clone(),
        workspace_id,
        cleaned,
        added,
        kept,
        notes,
    }
}

/// The recorded placements a SCOPE CHANGE removes, vs the ones detection alone lost (kept frozen):
/// a placement not in the new plan is cleaned only when the scope explains its absence — the shared
/// copy under a narrowing scope, a detected agent the scope excludes/omits, or a detected agent the
/// shared dir now covers (its native copy is redundant under an unscoped return). Returns
/// `(cleaned indices, kept-frozen indices)`.
fn partition_leaving(
    ctx: &Ctx<'_>,
    map: &PlacementMap,
    plan: &placement::PlacementPlan,
    scope: AgentScope<'_>,
) -> (Vec<usize>, Vec<usize>) {
    let detected: Vec<&str> = match &ctx.roots {
        Some(roots) => {
            topos_harness::registry::detected_harnesses(&roots.home, roots.cwd.as_deref())
                .into_iter()
                .map(|h| h.slug)
                .collect()
        }
        None => Vec::new(),
    };
    let mut cleaned = Vec::new();
    let mut frozen = Vec::new();
    for (i, (dir, state)) in map.placements.iter().zip(&map.placement_state).enumerate() {
        if plan.targets.iter().any(|t| t.dir == Path::new(dir)) {
            continue; // still planned — neither leaves nor freezes
        }
        let scope_caused = match state.kind {
            PlacementKind::Shared => scope.narrows(),
            PlacementKind::Native => state.agent.as_deref().is_some_and(|a| {
                let out_by_scope = scope.excluded.iter().any(|e| e == a)
                    || (!scope.agents.is_empty() && !scope.agents.iter().any(|s| s == a));
                let covered_now = !scope.narrows() && coverage::shared_dir_support(a).covered();
                detected.contains(&a) && (out_by_scope || covered_now)
            }),
        };
        if scope_caused {
            cleaned.push(i);
        } else {
            frozen.push(i);
        }
    }
    (cleaned, frozen)
}

/// Reconcile one skill's placements after its durable scope changed: clean the placements the new
/// scope removes (snapshot-first — an edited copy is committed to the sidecar store before its dir
/// goes; an unscannable one fails closed), drop them from the record, append the new targets, and
/// converge them from the LOCAL store (no network — the placed version is already local).
pub(crate) fn apply_scope_change(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    lock: &Lock,
    scope: AgentScope<'_>,
) -> Result<(), ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    let sp = ctx.layout.published(sid);
    let map = sync_engine::read_map_required(ctx, &sp)?;
    let ws = super::followed_workspace(ctx, sid.as_str());
    let slug = placement::workspace_slug(ctx, ws.as_deref());
    let plan = placement::plan_targets(
        ctx,
        sid.as_str(),
        topos_harness::PlacementNaming {
            name: Some(&lock.name),
            workspace_slug: slug.as_deref(),
        },
        scope,
        Some(&map),
    );
    let (cleaned_idx, _) = partition_leaving(ctx, &map, &plan, scope);

    // Snapshot-first clean of every leaving dir, then drop it from the record. The scan runs against
    // the leaving dir's own recorded sha; an edit anywhere in it is committed to the store first.
    let scans = placement::scan_placements(ctx, &map)?;
    for &i in &cleaned_idx {
        match &scans[i].status {
            ScanStatus::Unscannable => {
                return Err(ClientError::PlacementUnsupported {
                    reason: format!(
                        "the placement {} cannot be read; refusing to remove it — inspect or move \
                         the directory by hand",
                        scans[i].dir.display()
                    ),
                });
            }
            ScanStatus::Modified { scanned } => {
                sync_engine::snapshot_draft(ctx, &sp, lock, scanned)?;
            }
            ScanStatus::Clean { .. } | ScanStatus::Absent | ScanStatus::Foreign => {}
        }
    }
    let mut next = map.clone();
    // Drop from the back so earlier indices stay valid.
    for &i in cleaned_idx.iter().rev() {
        // A foreign dir (recorded, never materialized, since occupied) is dropped from the RECORD
        // only — its bytes were never ours to delete.
        let ours = !matches!(scans[i].status, ScanStatus::Foreign);
        let dir = next.placements.remove(i);
        next.placement_state.remove(i);
        if ours && ctx.fs.exists(Path::new(&dir)) {
            ctx.fs.remove_dir_all(Path::new(&dir))?;
        }
    }
    let mut next = placement::reconcile_map(&next, &plan);
    materialize_missing(ctx, &sp, lock, &mut next, &plan)?;
    Ok(())
}

/// Land the placed version's bytes into every planned-but-empty target from the LOCAL store, and
/// persist the reconciled record. A never-received skill (all-zero base) just records its targets —
/// the first receive places them.
fn materialize_missing(
    ctx: &Ctx<'_>,
    sp: &crate::sidecar::SkillPaths,
    lock: &Lock,
    next: &mut PlacementMap,
    plan: &placement::PlacementPlan,
) -> Result<(), ClientError> {
    let sync: Option<topos_types::persisted::SyncState> = doc::read_doc(ctx.fs, &sp.sync)?;
    let base_is_zero = lock.base_commit.bytes().all(|b| b == b'0');
    if base_is_zero {
        return doc::write_map(ctx.fs, &sp.map, next);
    }
    let managed = placement::managed_indices(next, plan);
    let scans = placement::scan_placements(ctx, next)?;
    let missing: Vec<usize> = managed
        .into_iter()
        .filter(|&i| matches!(scans[i].status, ScanStatus::Absent))
        .collect();
    if missing.is_empty() {
        return doc::write_map(ctx.fs, &sp.map, next);
    }
    let base = super::parse_hex32(&lock.base_commit)?;
    let base_digest = super::parse_hex32(&lock.bundle_digest)?;
    let store = Store::open(&sp.store)?;
    let bundle = store.render_verified(base, base_digest)?;
    sync_engine::fsync_batch(ctx, &store.version_durability(&base)?)?;
    let sync = sync.ok_or_else(|| ClientError::Corrupt("missing sync state".into()))?;
    let next_map = PlacementMap {
        applied_commit: lock.base_commit.clone(),
        materialized_sha: to_hex(&bundle.bundle_digest),
        ..next.clone()
    };
    crate::materialize::materialize(
        ctx.fs,
        &crate::materialize::MaterializeReq {
            skill_id: lock.skill_id.as_str(),
            target_indices: &missing,
            bundle: &bundle,
            next_map,
            next_lock: lock,
            next_sync: &sync, // unchanged — the served target did not move
            sp,
            snapshot: Some(&|s: &crate::scan::ScannedBundle| {
                sync_engine::snapshot_draft(ctx, sp, lock, s).map(|_| ())
            }),
        },
    )?;
    Ok(())
}
