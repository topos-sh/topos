//! `log <skill>` — the local action log (this skill's `log.jsonl` events) + its embedded-git history,
//! merged with the PLANE's version/proposal history when the skill is followed and this device enrolled.
//!
//! The plane half (`GET /skills/{skill}/log`) contributes the team's versions (newest first, with purge
//! tombstones rendered "purged by <who> <when> — bytes gone"), the proposal events, and the archived
//! successor hint when the skill was resolved by a FREED base name. A channel-typed target is refused
//! toward the web (curation history is a web surface). Un-enrolled / local-only skills keep today's
//! purely-local log.

use serde_json::json;
use topos_gitstore::Store;

use super::follow::{DirectoryConnect, build_universe_via};
use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::resolve::{self, Resolution, ResourceKind};
use crate::{enroll, logfile, sidecar::Layout};
use topos_core::digest::to_hex;
use topos_types::requests::{WireLogProposal, WireLogVersion};
use topos_types::results::LogData;

/// The seam `log` needs — the directory connector reads the plane-side history.
pub(crate) struct LogConnectors<'a> {
    pub directory: &'a DirectoryConnect<'a>,
}

/// History for `skill`: the local action events + git versions, then (when followed + enrolled) the
/// plane's version/proposal history. A channel target is refused toward the web.
///
/// # Errors
/// Name-resolution errors; [`ClientError::InvalidArgument`] for a channel target; a store / transport failure.
pub(crate) fn log(
    ctx: &Ctx<'_>,
    connectors: &LogConnectors<'_>,
    skill: &str,
) -> Result<LogData, ClientError> {
    // A CHANNEL target (resolved through the grammar) is a web surface — refuse it toward the web before
    // the local skill resolution swallows it as a not-found.
    if let Ok((base_url, universe)) = build_universe_via(ctx, connectors.directory)
        && let Ok(parsed) = resolve::parse_target(skill)
        && let Ok(Some(Resolution::Resource {
            kind: ResourceKind::Channel,
            name,
            ..
        })) = resolve::resolve_one(&universe, &parsed, resolve::KindScope::CHANNELS)
    {
        let _ = base_url;
        return Err(ClientError::InvalidArgument(format!(
            "'{name}' is a channel — a channel's curation history lives on the web, not in `topos \
             log` (which shows a skill's version history)"
        )));
    }

    let (id, lock) = resolve_skill(ctx, skill)?;

    // ---- the local half (unchanged) ----
    let mut events: Vec<serde_json::Value> =
        logfile::read_events(ctx.fs, &Layout::log_path(&ctx.layout))?
            .into_iter()
            .filter(|e| e.get("skill_id").and_then(|v| v.as_str()) == Some(id.as_str()))
            .collect();

    let store = Store::open(&ctx.layout.published(&id).store)?;
    for node in store.log(parse_hex32(&lock.base_commit)?)? {
        events.push(json!({
            "action": "version",
            "version_id": to_hex(&node.version_id),
            "author": node.author,
            "message": node.message,
            "parents": node.parents.iter().map(|p| to_hex(p)).collect::<Vec<_>>(),
        }));
    }

    // ---- the plane half (only for a followed skill on an enrolled install) ----
    let mut archived_successor = None;
    if let Some(workspace_id) = super::followed_workspace(ctx, id.as_str())
        && let Some(instance) = enroll::read_instance(ctx.fs, &ctx.layout)?
    {
        let directory = (connectors.directory)(&instance.base_url);
        // Best-effort: a transport fault or a not-found leaves the local log intact.
        if let Ok(plane) = directory.skill_log(&workspace_id, id.as_str()) {
            // The archived-successor hint when the skill was resolved by its FREED base name.
            if let Some(base) = &plane.base_name {
                archived_successor = Some(format!("{base} is archived as {}", plane.name));
            }
            for v in &plane.versions {
                events.push(plane_version_event(v));
            }
            for p in &plane.proposals {
                events.push(plane_proposal_event(p));
            }
        }
    }

    Ok(LogData {
        events,
        team: None,
        archived_successor,
    })
}

/// A plane version as a log event — a purged version carries its tombstone (`purged_at` / `purged_by`).
fn plane_version_event(v: &WireLogVersion) -> serde_json::Value {
    json!({
        "action": "version",
        "source": "plane",
        "version_id": v.version_id,
        "author": v.author,
        "message": v.message,
        "current": v.current,
        "purged_at": v.purged_at,
        "purged_by": v.purged_by,
    })
}

/// A plane proposal event (open + every resolution).
fn plane_proposal_event(p: &WireLogProposal) -> serde_json::Value {
    json!({
        "action": "proposal",
        "source": "plane",
        "version_id": p.version_id,
        "proposer": p.proposer,
        "status": p.status,
        "resolved_by": p.resolved_by,
        "resolved_reason": p.resolved_reason,
        "resolved_at": p.resolved_at,
        "message": p.resolved_reason,
        "created_at": p.created_at,
    })
}
