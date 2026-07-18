//! `log <skill>` — the local action log (this skill's `log.jsonl` events) + its embedded-git history,
//! merged with the PLANE's version/proposal history when the skill is followed and this device enrolled.
//!
//! The plane half (`GET /skills/{skill}/log`) contributes the team's versions (newest first, with purge
//! tombstones rendered "purged by `<who>` `<when>` — bytes gone"), the proposal events, and the archived
//! successor hint when the skill was resolved by a FREED base name. A channel-typed target is refused
//! toward the web (curation history is a web surface). Un-enrolled / local-only skills keep today's
//! purely-local log.

use std::collections::HashSet;

use serde_json::{Value, json};
use topos_gitstore::Store;

use super::follow::{DirectoryConnect, build_universe_via};
use super::{parse_hex32, resolve_skill};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::resolve::{self, Resolution, ResourceKind};
use crate::{enroll, identity, logfile, sidecar::Layout};
use topos_core::digest::to_hex;
use topos_types::requests::{WireLogProposal, WireLogVersion};
use topos_types::results::LogData;

/// The seam `log` needs — the directory connector reads the plane-side history.
pub(crate) struct LogConnectors<'a> {
    pub directory: &'a DirectoryConnect<'a>,
}

/// History for `skill`: the local action events + git versions, then (when followed + enrolled) the
/// plane's version/proposal history — row-capped by `page` (the `--json` default page / the
/// `--limit`/`--offset` flags), with the `truncated`/`total` markers when rows fall off the page.
/// A channel target is refused toward the web.
///
/// # Errors
/// Name-resolution errors; [`ClientError::InvalidArgument`] for a channel target; a store / transport failure.
pub(crate) fn log(
    ctx: &Ctx<'_>,
    connectors: &LogConnectors<'_>,
    skill: &str,
    page: super::RowPage,
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

    // ---- the local action log (`log.jsonl`) — non-version events (add / error / …) ----
    let local_actions: Vec<Value> = logfile::read_events(ctx.fs, &Layout::log_path(&ctx.layout))?
        .into_iter()
        .filter(|e| e.get("skill_id").and_then(|v| v.as_str()) == Some(id.as_str()))
        .collect();

    // ---- the local git version events, author-mapped ----
    // The git commit author is the raw device id (`d_<hex>`); THIS install's own id renders as "you", so a
    // local-only skill never prints a bare `d_…`. A `None` device id (no host identity yet) leaves the raw
    // author — the fallback for a display-less local version.
    let me_device = identity::read_device_id(ctx.fs, &ctx.layout)?;
    let store = Store::open(&ctx.layout.published(&id).store)?;
    let local_versions: Vec<(String, Value)> = store
        .log(parse_hex32(&lock.base_commit)?)?
        .into_iter()
        .map(|node| {
            let version_id = to_hex(&node.version_id);
            let event = json!({
                "action": "version",
                "version_id": version_id,
                "author": map_local_author(&node.author, me_device.as_deref()),
                "message": node.message,
                "parents": node.parents.iter().map(|p| to_hex(p)).collect::<Vec<_>>(),
            });
            (version_id, event)
        })
        .collect();

    // ---- the plane half (only for a followed skill on an enrolled install) ----
    let mut archived_successor = None;
    let mut plane_versions: Vec<Value> = Vec::new();
    let mut plane_proposals: Vec<Value> = Vec::new();
    let mut plane_version_ids: HashSet<String> = HashSet::new();
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
                plane_version_ids.insert(v.version_id.clone());
                plane_versions.push(plane_version_event(v));
            }
            for p in &plane.proposals {
                plane_proposals.push(plane_proposal_event(p));
            }
        }
    }

    // A version the PLANE reports (display attribution + `current`/purge marks) SUPERSEDES the local git
    // event for the same id — else a followed skill's versions print twice. Local-only versions stay.
    let mut events = assemble_log_events(
        local_actions,
        local_versions,
        plane_versions,
        plane_proposals,
        &plane_version_ids,
    );

    // The row page, applied AFTER assembly (the assembled order is deterministic, so consecutive
    // pages tile the same list). An inactive page keeps the exact prior shape (no marker fields).
    let (page_applied, truncated, total) = if page.is_active() {
        let (_, total) = page.apply(&mut events);
        let end = page.offset.saturating_add(events.len());
        (true, end < total, total)
    } else {
        (false, false, events.len())
    };

    Ok(LogData {
        events,
        team: None,
        archived_successor,
        truncated,
        total: page_applied.then_some(total as u64),
    })
}

/// Map a local git commit author to its display: THIS install's own device id renders as "you"; any
/// other author (or `None` local identity) passes through unchanged.
fn map_local_author(author: &str, me_device: Option<&str>) -> String {
    match me_device {
        Some(me) if me == author => "you".to_owned(),
        _ => author.to_owned(),
    }
}

/// Merge the local action log, the local git version events, and the plane's version/proposal events into
/// ONE ordered list, DEDUPED by version id: a local git version whose id the plane also reports is dropped
/// (the plane event wins — it carries the display author + the `current`/purge marks). Local-only versions
/// (this device's drafts, not on the plane) stay.
fn assemble_log_events(
    mut events: Vec<Value>,
    local_versions: Vec<(String, Value)>,
    plane_versions: Vec<Value>,
    plane_proposals: Vec<Value>,
    plane_version_ids: &HashSet<String>,
) -> Vec<Value> {
    for (version_id, event) in local_versions {
        if !plane_version_ids.contains(&version_id) {
            events.push(event);
        }
    }
    events.extend(plane_versions);
    events.extend(plane_proposals);
    events
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

#[cfg(test)]
mod tests {
    use super::*;

    fn version_event(id: &str, author: &str) -> (String, Value) {
        (
            id.to_owned(),
            json!({ "action": "version", "version_id": id, "author": author }),
        )
    }

    #[test]
    fn a_local_version_the_plane_also_reports_is_deduped_to_one() {
        // A followed skill's version is walked BOTH locally (git) and by the plane — the merged list must
        // carry each version_id ONCE, and the PLANE event (display author) wins.
        let vid = "a".repeat(64);
        let local = vec![version_event(&vid, "d_test")];
        let plane_versions = vec![json!({
            "action": "version", "source": "plane", "version_id": vid, "author": "Alice",
        })];
        let plane_ids: HashSet<String> = std::iter::once(vid.clone()).collect();

        let events = assemble_log_events(Vec::new(), local, plane_versions, Vec::new(), &plane_ids);
        let for_id: Vec<&Value> = events
            .iter()
            .filter(|e| e.get("version_id").and_then(Value::as_str) == Some(vid.as_str()))
            .collect();
        assert_eq!(for_id.len(), 1, "each version_id appears once");
        assert_eq!(
            for_id[0].get("source").and_then(Value::as_str),
            Some("plane"),
            "the plane event (display author) wins the dedupe"
        );
        assert_eq!(
            for_id[0].get("author").and_then(Value::as_str),
            Some("Alice")
        );
    }

    #[test]
    fn a_local_only_version_survives_the_merge() {
        // A version the plane does NOT report (a local draft) stays in the merged list.
        let vid = "b".repeat(64);
        let events = assemble_log_events(
            Vec::new(),
            vec![version_event(&vid, "you")],
            Vec::new(),
            Vec::new(),
            &HashSet::new(),
        );
        assert_eq!(events.len(), 1, "the local-only version survives");
        assert_eq!(events[0].get("author").and_then(Value::as_str), Some("you"));
    }

    #[test]
    fn this_installs_device_id_renders_as_you() {
        // A local-only skill's own device-authored versions render as "you", never a raw `d_…`; another
        // device's author (or no local identity) passes through unchanged.
        assert_eq!(map_local_author("d_self", Some("d_self")), "you");
        assert_eq!(map_local_author("d_other", Some("d_self")), "d_other");
        assert_eq!(map_local_author("d_self", None), "d_self");
    }
}
