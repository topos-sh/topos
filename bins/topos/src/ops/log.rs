//! `log <skill>` — the local action log (this skill's `log.jsonl` events) + its embedded-git history,
//! merged with the PLANE's version/proposal history when the skill is followed and this device enrolled.
//!
//! The plane half (`GET /skills/{skill}/log`) contributes the team's versions (newest first, with purge
//! tombstones rendered "purged by `<who>` `<when>` — bytes gone"), the proposal events, and the archived
//! successor hint when the skill was resolved by a FREED base name. A channel-typed target is refused
//! toward the web (curation history is a web surface). Un-enrolled / local-only skills keep today's
//! purely-local log.
//!
//! The halves are merged into ONE history, newest first: each source arrives in its own order, so the
//! merged list is re-ordered by the `at` stamp every dated event carries. An UNDATED event (a local git
//! version, a proposal) keeps its place beside the neighbour it arrived next to — nothing is given a
//! time it does not have.

use std::collections::HashSet;

use serde_json::{Value, json};
use topos_gitstore::Store;

use super::parse_hex32;
use super::reconcile::SessionConnect;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::resolve::{self, Resolution, ResourceKind};
use crate::{identity, logfile, sessions, sidecar::Layout, sync_status};
use topos_core::digest::to_hex;
use topos_types::requests::{WireLogProposal, WireLogVersion};
use topos_types::results::{LogData, SyncFault};

/// The seam `log` needs — the per-session transports read the plane-side history.
pub(crate) struct LogConnectors<'a> {
    /// The per-session transports (each read rides its session's own credential).
    pub session: &'a SessionConnect<'a>,
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
    workspace: Option<&str>,
    page: super::RowPage,
) -> Result<LogData, ClientError> {
    // Resolve WHERE YOU STAND, FIRST: a bundle a project `topos.toml` delivers keeps its custody
    // (and its action log) in the checkout's own store, so a `log` run from inside that checkout
    // reads THAT store — not a same-named machine twin, and not a not-found. A bundle this
    // machine already tracks is also UNAMBIGUOUS about its workspace (its own row names the one it
    // came from), so this arm resolves nothing and reads no other login.
    let (layout, id, lock) = match super::resolve_skill_here(ctx, skill, None) {
        Ok(found) => found,
        // Nothing here answers to that name. It may be a CHANNEL — whose curation history is a web
        // surface — in the workspace this machine acts on: ONE workspace, never a scan of every
        // login.
        Err(absent @ ClientError::NoSuchSkill { .. }) => {
            return Err(channel_refusal(ctx, connectors, skill, workspace)?.unwrap_or(absent));
        }
        Err(e) => return Err(e),
    };
    let sctx = super::pull::ctx_with_layout(ctx, &layout);
    // The KIND decides whether this verb applies at all, asked before a single event is read.
    let placements: Vec<String> = crate::doc::read_map(sctx.fs, &layout.published(&id).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    if let Some(refusal) = crate::bundle_kind::refuse_version_verb(
        crate::bundle_kind::VersionVerb::Log,
        &lock.name,
        crate::bundle_kind::classify(&sctx, id.as_str(), &placements).or_skill(),
    ) {
        return Err(refusal);
    }

    // ---- the local action log (`log.jsonl`) — non-version events (add / error / …) ----
    // Each store keeps its OWN `log.jsonl` (the layout resolves it under that store's root), and an
    // apply event is written by the operation running against the store it applied into — so the
    // OWNING store's log is this copy's history. Deliberately not merged with the machine log: the
    // same bundle can be held at both scopes under one skill id, and merging would attribute the
    // other copy's applies to this one. A machine-scope skill resolves to the machine layout here,
    // so nothing about the machine case changes.
    let local_actions: Vec<Value> = logfile::read_events(ctx.fs, &Layout::log_path(&layout))?
        .into_iter()
        .filter(|e| e.get("skill_id").and_then(|v| v.as_str()) == Some(id.as_str()))
        .collect();

    // ---- the local git version events, author-mapped ----
    // The git commit author is the raw device id (`d_<hex>`); THIS install's own id renders as "you", so a
    // local-only skill never prints a bare `d_…`. A `None` device id (no host identity yet) leaves the raw
    // author — the fallback for a display-less local version.
    // The device id is a MACHINE fact (one host identity per install), so it is read from the
    // machine layout however the skill's custody is scoped; the version walk is the owning store's.
    let me_device = identity::read_device_id(ctx.fs, &ctx.layout)?;
    let store = Store::open(&layout.published(&id).store)?;
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
        && let Ok(Some((session, transports))) =
            super::connect::workspace_transports(ctx, connectors.session, &workspace_id)
    {
        // The ONE workspace this bundle came from, named once. Reading every login to find a lane
        // for a workspace already known announced `reading <host>/<ws>` for workspaces that hold
        // no copy of this bundle at all.
        let _phase = crate::progress::phase(
            ctx.progress,
            &format!("reading {}/{}", session.host, session.workspace_name),
        );
        // Best-effort: a transport fault or a not-found leaves the local log intact.
        if let Ok(plane) = transports.directory.skill_log(&workspace_id, id.as_str()) {
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

    // Whether the workspace this copy is delivered from landed its LAST exchange with this machine.
    // The freshness cache is a MACHINE fact, like the device id above — one per install, whatever
    // scope holds the copy — so a project copy reads it from `ctx.layout` too.
    let sync_fault = super::followed_workspace(ctx, id.as_str())
        .and_then(|workspace_id| recorded_fault(ctx, &workspace_id));

    Ok(LogData {
        events,
        team: None,
        archived_successor,
        truncated,
        total: page_applied.then_some(total as u64),
        sync_fault,
    })
}

/// The refusal a name that is a CHANNEL earns: a channel's curation history lives on the web, not
/// in `topos log`. Asked of ONE workspace — the one the machine acts on (`--workspace` →
/// `TOPOS_WORKSPACE` → the starred default → the sole login) — because a lookup that scanned every
/// login dialed workspaces that hold nothing by that name, and narrated each one.
///
/// `Ok(None)` = not a channel there (or this machine is signed into nothing), which leaves the
/// caller's own "no such bundle" answer standing.
///
/// # Errors
/// The typed workspace-selection refusal when several logins are joined and none is the default.
fn channel_refusal(
    ctx: &Ctx<'_>,
    connectors: &LogConnectors<'_>,
    skill: &str,
    workspace: Option<&str>,
) -> Result<Option<ClientError>, ClientError> {
    let su = match super::connect::acting_universe(ctx, connectors.session, workspace) {
        Ok(Some(su)) => su,
        Ok(None) => return Ok(None),
        // The PICK is the person's to make and has to be said out loud. A server that did not
        // answer is a different thing entirely: the probe is best-effort, so the local
        // "no bundle by that name" stands, exactly as it did before this probe existed.
        Err(e @ (ClientError::WorkspaceSelection(_) | ClientError::SessionRequired { .. })) => {
            return Err(e);
        }
        Err(_) => return Ok(None),
    };
    let Ok(parsed) = resolve::parse_target(skill) else {
        return Ok(None);
    };
    let Ok(Some(Resolution::Resource {
        kind: ResourceKind::Channel,
        name,
        ..
    })) = resolve::resolve_one(&su.universe, &parsed, resolve::KindScope::CHANNELS)
    else {
        return Ok(None);
    };
    Ok(Some(ClientError::InvalidArgument(format!(
        "'{name}' is a channel — a channel's curation history lives on the web, not in `topos \
         log` (which shows a skill's version history)"
    ))))
}

/// The fault recorded for `workspace_id`'s last exchange, named the way a person addresses the
/// workspace. Best-effort: an unreadable freshness cache says nothing rather than failing a history
/// read. The cache is KEYED by the opaque id and the id is never what a person is shown — the cache
/// row and the session record both spell the address name, so the fallback cannot switch vocabulary
/// mid-line; the id is the last resort, for a row written before either carried a name.
fn recorded_fault(ctx: &Ctx<'_>, workspace_id: &str) -> Option<SyncFault> {
    let status = sync_status::read(ctx.fs, &ctx.layout).ok()?;
    let entry = status.workspaces.get(workspace_id)?;
    let kind = entry.last_exchange_fault?;
    let named = entry.workspace_name.clone().or_else(|| {
        sessions::read_sessions(ctx.fs, &ctx.layout)
            .ok()?
            .sessions
            .iter()
            .find(|s| s.workspace_id == workspace_id)
            .map(|s| s.workspace_name.clone())
    });
    Some(SyncFault {
        workspace: named.unwrap_or_else(|| workspace_id.to_owned()),
        kind,
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
/// ONE list ordered NEWEST FIRST, DEDUPED by version id: a local git version whose id the plane also
/// reports is dropped (the plane event wins — it carries the display author + the `current`/purge marks).
/// Local-only versions (this device's drafts, not on the plane) stay.
///
/// The three sources arrive in their own orders — the action log oldest-first, the plane's versions
/// newest-first — so concatenating them read as three lists stapled together the moment every version
/// started printing its own date. [`order_newest_first`] makes it ONE history.
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
    order_newest_first(events)
}

/// When an event happened, as the merged list records it: the `at` stamp, epoch milliseconds — the one
/// field every dated event carries, and the one the TTY renderer reads. An event without it has no time,
/// and none is guessed from anything else it holds.
fn event_time(event: &Value) -> Option<u64> {
    event.get("at").and_then(Value::as_u64)
}

/// Order the merged events NEWEST FIRST — without inventing a time for anything.
///
/// Not every event is dated: a local git version carries no stamp, and neither does a proposal. Such an
/// event is sorted by the time of the neighbour it ALREADY sits next to — the nearest dated event before
/// it, or, for events ahead of the first dated one, the nearest after — used as a sort key and never
/// written onto the event. So an undated event travels WITH the event it arrived beside instead of
/// drifting to one end of the list, and the renderer still prints it with a blank stamp.
///
/// The sort is STABLE, so events sharing a time — and whole undated runs — keep the order they arrived
/// in. A list where nothing carries a time is returned exactly as it was.
fn order_newest_first(events: Vec<Value>) -> Vec<Value> {
    let own: Vec<Option<u64>> = events.iter().map(event_time).collect();
    // Nothing to order by: leave the list alone rather than shuffle it under a made-up key.
    let Some(first_dated) = own.iter().position(Option::is_some) else {
        return events;
    };
    let lead = own[first_dated].unwrap_or_default();

    let mut keys: Vec<u64> = Vec::with_capacity(own.len());
    let mut carried = lead;
    for time in &own {
        // Before the first dated event `carried` is still `lead` — the nearest time AFTER, which is
        // the only neighbour those events have.
        if let Some(time) = *time {
            carried = time;
        }
        keys.push(carried);
    }

    let mut keyed: Vec<(u64, Value)> = keys.into_iter().zip(events).collect();
    // `sort_by_key` is STABLE, and `Reverse` is what makes it newest first without losing that.
    keyed.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    keyed.into_iter().map(|(_, event)| event).collect()
}

/// A plane version as a log event — a purged version carries its tombstone (`purged_at` / `purged_by`).
/// `at` is the commit time the server recorded, in the same field and units a `pull` event carries, so
/// every history line stamps itself instead of borrowing the date of whatever printed above it.
fn plane_version_event(v: &WireLogVersion) -> serde_json::Value {
    json!({
        "action": "version",
        "source": "plane",
        "version_id": v.version_id,
        "at": v.at,
        "author": v.author,
        "message": v.message,
        "current": v.current,
        "purged_at": v.purged_at,
        "purged_by": v.purged_by,
    })
}

/// A plane proposal event (open + every resolution).
///
/// `at` is WHEN THE PROPOSAL WAS OPENED — the one instant the row records for itself, and the one it
/// always has (a resolution is rendered in the line's own words, and moving the row to the day it was
/// rejected would file "proposed by X" away from when X did it). The server stamps it as a string, so
/// it is parsed here; a spelling this client cannot read leaves the row undated rather than dated
/// wrong, and the merge keeps it beside the neighbour it arrived next to.
fn plane_proposal_event(p: &WireLogProposal) -> serde_json::Value {
    json!({
        "action": "proposal",
        "source": "plane",
        "at": super::parse_rfc3339_utc_millis(&p.created_at),
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

    /// A dated event: `at` epoch milliseconds, the field every dated event carries.
    fn dated(tag: &str, at: u64) -> Value {
        json!({ "action": "add", "name": tag, "at": at })
    }

    /// An event with NO time — a local git version, a proposal.
    fn undated(tag: &str) -> Value {
        json!({ "action": "version", "version_id": tag })
    }

    /// The tags of a merged list, in the order it reads.
    fn tags(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|e| {
                e.get("name")
                    .or_else(|| e.get("version_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_merged_history_reads_newest_first_across_every_source() {
        // The action log arrives OLDEST first and the plane's versions NEWEST first, so before this
        // the two read as separate lists stapled together — with dates on the rows to prove it.
        let events = assemble_log_events(
            vec![dated("add", 100), dated("error", 300)],
            Vec::new(),
            vec![dated("v3", 400), dated("v2", 200)],
            Vec::new(),
            &HashSet::new(),
        );
        assert_eq!(tags(&events), ["v3", "error", "v2", "add"]);
    }

    #[test]
    fn an_undated_event_travels_with_the_neighbour_it_arrived_beside() {
        // A local git version carries no stamp. It must stay where it was relative to its neighbours
        // rather than sink to one end — and it must not acquire a time on the way.
        let events =
            order_newest_first(vec![dated("old", 100), undated("draft"), dated("new", 900)]);
        assert_eq!(tags(&events), ["new", "old", "draft"]);
        assert!(
            events[2].get("at").is_none(),
            "no time is written onto an undated event: {}",
            events[2]
        );
    }

    #[test]
    fn undated_events_ahead_of_every_date_stay_ahead_of_the_one_they_lead() {
        // Nothing precedes them, so their only neighbour is the event AFTER — they ride with it.
        let events = order_newest_first(vec![
            undated("lead-a"),
            undated("lead-b"),
            dated("old", 100),
            dated("new", 900),
        ]);
        assert_eq!(tags(&events), ["new", "lead-a", "lead-b", "old"]);
    }

    #[test]
    fn events_sharing_a_time_keep_the_order_they_arrived_in() {
        let events = order_newest_first(vec![
            dated("first", 500),
            dated("second", 500),
            dated("third", 500),
        ]);
        assert_eq!(tags(&events), ["first", "second", "third"]);
    }

    #[test]
    fn a_history_where_nothing_carries_a_time_is_left_exactly_as_it_was() {
        let events = order_newest_first(vec![undated("a"), undated("b"), undated("c")]);
        assert_eq!(tags(&events), ["a", "b", "c"]);
    }

    fn proposal(created_at: &str) -> WireLogProposal {
        WireLogProposal {
            version_id: "f".repeat(64),
            proposer: "Mia <mia@topos.sh>".to_owned(),
            status: "open".to_owned(),
            resolved_by: None,
            resolved_reason: None,
            resolved_at: None,
            created_at: created_at.to_owned(),
        }
    }

    #[test]
    fn a_proposal_is_dated_by_when_it_was_opened() {
        // The server stamps `created_at` as a string; the merge orders by `at`, so a proposal that
        // stayed undated could sort ahead of an action that happened after it.
        let event = plane_proposal_event(&proposal("2023-11-15T22:13:20Z"));
        assert_eq!(event["at"].as_i64(), Some(1_700_086_400_000));
    }

    #[test]
    fn a_stamp_this_client_cannot_read_leaves_the_proposal_undated() {
        // Undated beats dated-wrong: the merge then keeps the row beside the neighbour it arrived
        // next to, rather than filing it under a guess.
        for unreadable in [
            "",
            "yesterday",
            "2023-11-15T22:13:20.500Z",
            "2023-11-15 22:13:20Z",
        ] {
            let event = plane_proposal_event(&proposal(unreadable));
            assert!(event["at"].is_null(), "{unreadable}: {event}");
        }
    }

    #[test]
    fn an_old_proposal_sinks_below_the_action_that_happened_after_it() {
        // The exact symptom the stamp fixes. Proposals arrive LAST in the merge, so an undated one
        // inherited the key of the newest thing above it — the plane version here — and rode up
        // past a local action it predates by a day. Its own date puts it where it belongs.
        let events = assemble_log_events(
            vec![dated("add", 1_700_000_000_000)],
            Vec::new(),
            vec![dated("v", 1_700_086_400_000)],
            vec![plane_proposal_event(&proposal("2023-11-13T22:13:20Z"))],
            &HashSet::new(),
        );
        let order: Vec<&str> = events
            .iter()
            .map(|e| {
                e.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| e.get("action").and_then(Value::as_str).unwrap_or("?"))
            })
            .collect();
        assert_eq!(order, ["v", "add", "proposal"], "{events:?}");
    }

    #[test]
    fn a_plane_version_event_carries_the_time_the_server_recorded() {
        // `--json` version entries stamp themselves the same way a `pull` event does (`at`, epoch
        // milliseconds), so a caller never has to infer a version's date from the line above it.
        let event = plane_version_event(&WireLogVersion {
            version_id: "a".repeat(64),
            at: Some(1_700_086_400_000),
            author: Some("Robert <robert@topos.sh>".to_owned()),
            message: Some("topos: publish".to_owned()),
            current: true,
            purged_at: None,
            purged_by: None,
        });
        assert_eq!(event["at"].as_i64(), Some(1_700_086_400_000));
    }

    #[test]
    fn a_version_the_server_recorded_no_time_for_omits_at_rather_than_faking_one() {
        let event = plane_version_event(&WireLogVersion {
            version_id: "a".repeat(64),
            at: None,
            author: None,
            message: None,
            current: false,
            purged_at: None,
            purged_by: None,
        });
        assert!(event["at"].is_null(), "{event}");
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
