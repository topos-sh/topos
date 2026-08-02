//! `status` — the offline orientation read AND the trust rail: the sessions, the resolved table
//! for "an agent here", each connected workspace's REGIME, the disclosure NOTES, and — with a
//! bundle argument — the deep single-bundle answer. Computed ENTIRELY from local state: the
//! parsed manifests (`manifest::scopes`), the sessions file, the offline delivery cache
//! (`sync_status`), and the per-scope sidecar stores. No network, no writes.
//!
//! SCOPES ARE UNBLENDED, so the table is SECTIONED, never merged: the NEAREST project manifest
//! (taken whole), then the person scope — the global manifest when it exists (a COMPLETE recipe:
//! only its rows deliver, and a workspace's feed flows iff a feed row says so), else the implicit
//! recipe of one feed row per connected workspace. Within a scope the delivered set is the union
//! of the rows (or the implicit feeds) minus the `"off"` switches, deduped by bundle identity —
//! an explicit row beats any set's or any feed's delivery of the same bundle.
//!
//! Per line: the winning reference, ONE source (which manifest row — or which workspace's feed —
//! asked), the delivery attribution (`assigned by <name>` / `picked by you`), and an honest state
//! (applied-as-of-the-last-sync / local edits / behind / off / not available / never delivered
//! from / unknown). What a
//! row cannot carry rides `notes`: the loud "the global manifest does not adopt these
//! assignments" line, feeds whose exchange landed with nothing assigned, rows that add nothing,
//! `"off"` switches for bundles nobody assigns any more, set collisions, declined-but-delivered
//! bundles, and cross-scope version splits.
//!
//! The bare `topos` invocation renders this same snapshot on a TTY, so a human's first keystroke
//! answers "what is this, and where am I" without dialing anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use topos_types::persisted::{Lock, SyncState};
use topos_types::results::{
    StatusData, StatusDetail, StatusItem, StatusItemState, StatusRegime, StatusSession,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::document::EntryValue;
use crate::manifest::keys::{self, KeyShape};
use crate::manifest::scopes::{self, ScopePlan};
use crate::sessions::Sessions;
use crate::sidecar::Layout;
use crate::sync_status::{DeliveredSkill, SyncStatus, WorkspaceSync};
use crate::{doc, placement, sessions, sync_status};

/// The all-zero sentinel a first-receive baseline carries — a delivered bundle whose sync doc
/// still holds it has never been applied here (the next `update` applies it).
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Assemble the offline status snapshot. `bundle` is the deep single-bundle answer's token (a row
/// reference, a leaf name, or a cached feed name); `None` is the bare table. `triggers` stays
/// empty here — the composition root fills it from the read-only probe (`ops::probe_detected`),
/// the same layering the arming receipts use.
///
/// # Errors
/// A read failure or a manifest the grammar refuses (typed, naming the fix); a `bundle` token no
/// scope delivers is the uniform [`ClientError::TargetNotFound`].
pub(crate) fn status_snapshot(
    ctx: &Ctx<'_>,
    bundle: Option<&str>,
) -> Result<StatusData, ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let signed_in = all.live().count() > 0;
    let cache = sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();

    // What the workspaces ASSIGN this person (the delivery cache's feed rows — a manifest-driven
    // row is demand, not an assignment), and how much of it has never been applied here. A
    // delivery whose sidecar sync doc still holds the all-zero baseline has never been applied;
    // any unreadable doc makes the count honestly absent, never a partial number.
    //
    // Only a workspace this installation still holds a LIVE session for can assign anything here:
    // the cache OUTLIVES a logout, so counting every cached entry would keep crediting a workspace
    // nothing dials any more — and the number would sit beside a table that never shows its rows.
    // The predicate is `live()`, the SAME one the feeds and the regimes resolve against, so the
    // count can only name what the table can show. A PENDING session counts (the workspace
    // assigns; only an owner's approval gates the bytes); an ENDED one does not (access is gone —
    // what it delivered is frozen in place, not assigned).
    //
    // The key is `(host, workspace id)`, never the id alone: ids are opaque strings each SERVER
    // mints on its own, so the same id on two hosts is two different workspaces — matching on the
    // id would let a live session on one server resurrect another server's stale cache entry. A
    // cache entry with NO recorded host (written before the session model) is not placeable on any
    // server, so it counts for nobody: that is the same rule the table already follows (`ws_entry`
    // resolves by host + address), and the count must not claim what no row can show.
    let live: BTreeSet<(&str, &str)> = all
        .live()
        .map(|s| (s.host.as_str(), s.workspace_id.as_str()))
        .collect();
    let mut assigned = 0u64;
    let mut awaiting = Some(0u64);
    for (workspace_id, entry) in &cache.workspaces {
        let Some(host) = entry.host.as_deref() else {
            continue;
        };
        if !live.contains(&(host, workspace_id.as_str())) {
            continue;
        }
        for (skill_id, d) in &entry.delivered {
            if d.withdrawn || d.via_manifest {
                continue;
            }
            assigned += 1;
            let Ok(sid) = crate::id::SkillId::parse(skill_id) else {
                awaiting = None;
                continue;
            };
            match doc::read_doc::<SyncState>(ctx.fs, &ctx.layout.published(&sid).sync) {
                Ok(Some(sync)) if sync.base_commit == ZERO_HEX => {
                    awaiting = awaiting.map(|n| n + 1);
                }
                Ok(None) => awaiting = awaiting.map(|n| n + 1),
                Ok(Some(_)) => {}
                Err(_) => awaiting = None,
            }
        }
    }

    // This installation's SESSIONS (one per logged-into workspace).
    let session_rows: Vec<StatusSession> = all
        .sessions
        .iter()
        .map(|s| StatusSession {
            workspace_id: s.workspace_id.clone(),
            name: s.workspace_name.clone(),
            display_name: s.display_name.clone(),
            host: s.host.clone(),
            session_status: (s.status != sessions::SESSION_ACTIVE).then(|| s.status.clone()),
        })
        .collect();

    let resolved = resolve(ctx, &all, &cache)?;
    let mut items: Vec<StatusItem> = resolved.rows.iter().map(|r| r.item.clone()).collect();
    let mut detail = None;
    if let Some(token) = bundle {
        let (one, matched) = deep_answer(&resolved.rows, &all, token)?;
        detail = Some(one);
        items = matched;
    }

    Ok(StatusData {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        server: all.sessions.first().map(|s| s.base_url.clone()),
        signed_in,
        profile_skills: assigned,
        awaiting_first_sync: awaiting,
        triggers: Vec::new(),
        items,
        regimes: resolved.regimes,
        notes: resolved.notes,
        detail,
        sessions: session_rows,
    })
}

// =================================================================================================
// The resolution — per scope, from the parsed recipes + the offline cache + the sidecar stores.
// =================================================================================================

/// One resolved line plus the provenance the deep answer spells out.
struct Row {
    item: StatusItem,
    /// The manifest FILE whose row delivers it (absent on a feed-delivered line).
    source_file: Option<String>,
    /// That row's spelled key (the joined reference).
    source_key: Option<String>,
    /// `<host>/<workspace>` when a feed delivers it.
    feed: Option<String>,
    /// The row's version pin, when one is spelled.
    pin: Option<String>,
    /// The placement dirs this machine holds for it.
    placements: Vec<String>,
    /// A BUNDLE line (not a set header, a feed header, or an `"off"` switch) — what the adopted
    /// count counts and what a cross-scope split compares.
    bundle: bool,
}

/// The whole resolution: the table (project scope first, then person), the per-workspace regimes,
/// and the disclosure notes in reading order.
struct Resolved {
    rows: Vec<Row>,
    regimes: Vec<StatusRegime>,
    notes: Vec<String>,
}

/// One scope's pass — its lines plus the notes only that pass can see.
#[derive(Default)]
struct ScopeOut {
    rows: Vec<Row>,
    /// Two SETS delivering one name at different versions — the winner named.
    collisions: Vec<String>,
    /// `"off"` switches whose bundle is not in the cached feed any more.
    stale_offs: Vec<String>,
    /// Feed ADDRESSES whose last exchange completed and assigned this person nothing. The fact is
    /// the WORKSPACE's, not a scope's, so `resolve` says it once however many recipes adopt it.
    empty_feeds: Vec<String>,
}

/// Resolve both scopes, the regimes, and the notes — the whole offline answer.
fn resolve(ctx: &Ctx<'_>, all: &Sessions, cache: &SyncStatus) -> Result<Resolved, ClientError> {
    let connected: Vec<(String, String)> = all
        .live()
        .map(|s| (s.host.clone(), s.workspace_name.clone()))
        .collect();
    let person_plan = scopes::person_plan(ctx.fs, &ctx.layout, &connected)?;
    let project = match ctx.roots.as_ref().and_then(|r| {
        r.cwd
            .as_deref()
            .map(|cwd| (cwd.to_path_buf(), r.home.clone()))
    }) {
        Some((cwd, home)) => scopes::nearest_project_plan(ctx.fs, &cwd, Some(&home))?,
        None => None,
    };

    // The NEAREST project manifest, whole — over the project's OWN store (never minted here).
    let mut project_out = ScopeOut::default();
    if let Some((dir, plan)) = &project {
        let layout = crate::sidecar::existing_project_store(ctx.fs, dir);
        project_out = scope_rows(ctx, plan, false, layout.as_ref(), cache, all);
    }
    let person_out = scope_rows(ctx, &person_plan, true, Some(&ctx.layout), cache, all);

    // The REGIMES — one sentence per connected workspace, over the person plan.
    let mut regimes = Vec::new();
    for (host, ws) in &connected {
        let unadopted = assigned_not_adopted(&person_plan, cache, host, ws);
        if let Some(regime) = person_plan.regime(host, ws, unadopted) {
            regimes.push(StatusRegime {
                host: host.clone(),
                workspace: ws.clone(),
                regime,
            });
        }
    }

    // The NOTES, in reading order: the loud one first, then the inert rows, then the frictions.
    let mut notes = Vec::new();
    if person_plan.file_backed() {
        let adopts = person_out.rows.iter().filter(|r| r.bundle).count();
        for (host, ws) in &connected {
            if person_plan.has_feed(host, ws) {
                continue;
            }
            let unadopted = assigned_not_adopted(&person_plan, cache, host, ws);
            if unadopted == 0 {
                continue;
            }
            notes.push(format!(
                "global manifest adopts {adopts} bundles; {unadopted} assigned bundles are not \
                 adopted here (no feed row) — `topos add -g @{ws}` restores them"
            ));
        }
    }
    // A feed that HAS exchanged and brought nothing: the table can only fall silent, so the fact
    // gets words. Said once per address — it is the workspace's answer, not each scope's.
    let mut said: BTreeSet<&str> = BTreeSet::new();
    for feed in person_out
        .empty_feeds
        .iter()
        .chain(&project_out.empty_feeds)
    {
        if said.insert(feed.as_str()) {
            notes.push(format!("{feed}: exchanged — nothing assigned to you yet"));
        }
    }
    // A bare `"*"` row for a bundle the flowing feed already delivers states nothing new.
    for row in &person_plan.things {
        let KeyShape::WorkspaceBundle {
            host,
            workspace,
            bundle,
        } = &row.shape
        else {
            continue;
        };
        if matches!(row.value, EntryValue::Star)
            && person_plan.has_feed(host, workspace)
            && feed_delivers(cache, host, workspace, bundle)
        {
            notes.push(format!(
                "{} adds nothing — the feed already delivers it",
                row.reference
            ));
        }
    }
    notes.extend(person_out.stale_offs.iter().cloned());
    notes.extend(project_out.stale_offs.iter().cloned());
    notes.extend(person_out.collisions.iter().cloned());
    notes.extend(project_out.collisions.iter().cloned());
    // Declined on the web, delivered here anyway: the file outranks the person's own web choice.
    for entry in cache.workspaces.values() {
        let (Some(host), Some(ws)) = (&entry.host, &entry.workspace_name) else {
            continue;
        };
        for name in entry.declined.values() {
            if person_plan.explicit_claims(host, ws, name) {
                notes.push(format!(
                    "{name}: declined on the web, delivered here by your global manifest"
                ));
            }
        }
    }
    // The same bundle at BOTH scopes on different versions — nothing blends, so both copies land.
    let claude = claude_code_detected(ctx);
    for prow in project_out.rows.iter().filter(|r| r.bundle) {
        let Some(person) = person_out
            .rows
            .iter()
            .find(|r| r.bundle && r.item.name == prow.item.name)
        else {
            continue;
        };
        let here = prow.pin.clone().or_else(|| prow.item.version.clone());
        let there = person.pin.clone().or_else(|| person.item.version.clone());
        if let (Some(a), Some(b)) = (here, there)
            && a != b
        {
            let mut note = format!(
                "{}: project pins {}, person delivers {} — the project copy is projected in-repo",
                prow.item.name,
                short(&a),
                short(&b)
            );
            if claude {
                note.push_str(
                    "; Claude Code resolves personal skills ahead of project ones, so the \
                     personal copy may win anyway",
                );
            }
            notes.push(note);
        }
    }

    let mut rows = project_out.rows;
    rows.extend(person_out.rows);
    Ok(Resolved {
        rows,
        regimes,
        notes,
    })
}

/// One scope's lines, in reading order: the explicit THING rows, each SET row with its cached
/// members itemized under it, the flowing FEEDS, then the `"off"` switches. Deduped by bundle
/// identity as it goes — the first statement to claim an identity wins, and things are read
/// before sets and feeds, which IS the "an explicit row beats a set" rule.
fn scope_rows(
    ctx: &Ctx<'_>,
    plan: &ScopePlan,
    person: bool,
    layout: Option<&Layout>,
    cache: &SyncStatus,
    all: &Sessions,
) -> ScopeOut {
    let scope = if person { "person" } else { "project" };
    let file = plan.file.as_deref().map(|p| pretty(ctx, p));
    let base = plan
        .file
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let mut out = ScopeOut::default();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut by_name: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut index: Option<BTreeMap<String, String>> = None;

    // 1. The explicit THING rows — one bundle each, the row's own fields winning.
    for row in &plan.things {
        let identity = row.shape.canonical();
        if !claimed.insert(identity.clone()) {
            continue;
        }
        let name = row.display_name();
        let mut via = None;
        let mut attribution = None;
        let applied = match &row.shape {
            KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            } => match cache_lookup(cache, host, workspace, bundle) {
                Some(hit) => {
                    via = hit.ds.via_channels.first().cloned();
                    attribution = attribution_of(hit.ds);
                    // A PINNED row's target is its pin, never the served current: the pin is what
                    // `update` delivers here, so measuring against `current` would report a row
                    // sitting exactly where it was asked to sit as "behind — `topos update` lands
                    // the newer version", advice the next `update` correctly refuses to take.
                    let target = row.pin().unwrap_or_else(|| hit.ds.served_version.clone());
                    applied_for_id(
                        ctx,
                        layout,
                        hit.skill_id,
                        &target,
                        hit.entry.last_delivery_at,
                    )
                }
                None => stored_by_name(ctx, layout, &mut index, &name)
                    .unwrap_or_else(|| Applied::plain(session_state(all, host, workspace))),
            },
            // A local folder: its presence IS the delivery (adopted in place — there is no
            // upstream to be behind or ahead of).
            KeyShape::LocalPath { raw } => {
                if ctx.fs.exists(&local_dir(ctx, base.as_deref(), raw)) {
                    stored_by_name(ctx, layout, &mut index, &name)
                        .unwrap_or_else(|| Applied::plain(StatusItemState::Applied))
                } else {
                    Applied::plain(StatusItemState::Unknown)
                }
            }
            // A repo skill: only this scope's store can answer it offline.
            _ => stored_by_name(ctx, layout, &mut index, &name).unwrap_or_else(Applied::unknown),
        };
        out.rows.push(Row {
            item: StatusItem {
                name,
                reference: identity,
                source: file.clone().unwrap_or_default(),
                scope: scope.to_owned(),
                via,
                attribution,
                version: applied.version,
                applied_as_of: applied.as_of,
                state: applied.state,
                shadows: Vec::new(),
            },
            source_file: file.clone(),
            source_key: Some(row.reference.clone()),
            feed: None,
            pin: row.pin(),
            placements: applied.placements,
            bundle: true,
        });
    }

    // 2. The SET rows — the line itself, then the members the last delivery cached under it.
    for row in &plan.sets {
        let identity = row.shape.canonical();
        let state = match &row.shape {
            KeyShape::Channel {
                host, workspace, ..
            } => session_state(all, host, workspace),
            _ => StatusItemState::Unknown,
        };
        out.rows.push(Row {
            item: StatusItem {
                name: row.display_name(),
                reference: identity.clone(),
                source: file.clone().unwrap_or_default(),
                scope: scope.to_owned(),
                via: None,
                attribution: None,
                version: None,
                applied_as_of: None,
                state,
                shadows: Vec::new(),
            },
            source_file: file.clone(),
            source_key: Some(row.reference.clone()),
            feed: None,
            pin: row.pin(),
            placements: Vec::new(),
            bundle: false,
        });
        let KeyShape::Channel {
            host,
            workspace,
            channel,
        } = &row.shape
        else {
            continue;
        };
        let Some(entry) = ws_entry(cache, host, workspace) else {
            continue;
        };
        for (skill_id, ds) in &entry.delivered {
            if ds.withdrawn
                || ds.name.is_empty()
                || !ds.via_channels.iter().any(|c| c == channel)
                || plan.off_for(host, workspace, &ds.name).is_some()
                || plan.explicit_claims(host, workspace, &ds.name)
            {
                continue;
            }
            let member = format!("{host}/{workspace}/{}", ds.name);
            if !claimed.insert(member.clone()) {
                continue;
            }
            collide(
                &mut by_name,
                &mut out.collisions,
                &ds.name,
                &identity,
                &ds.served_version,
            );
            let applied = applied_for_id(
                ctx,
                layout,
                skill_id,
                &ds.served_version,
                entry.last_delivery_at,
            );
            out.rows.push(Row {
                item: StatusItem {
                    name: ds.name.clone(),
                    reference: member,
                    source: file.clone().unwrap_or_default(),
                    scope: scope.to_owned(),
                    via: Some(channel.clone()),
                    attribution: attribution_of(ds),
                    version: applied.version,
                    applied_as_of: applied.as_of,
                    state: applied.state,
                    shadows: Vec::new(),
                },
                source_file: file.clone(),
                source_key: Some(row.reference.clone()),
                feed: None,
                pin: None,
                placements: applied.placements,
                bundle: true,
            });
        }
    }

    // 3. The FEEDS that flow here (the global file's feed rows, or the implicit recipe's).
    for (host, workspace) in &plan.feeds {
        let feed = format!("{host}/{workspace}");
        let entry = ws_entry(cache, host, workspace);
        let mut itemized = 0usize;
        for (skill_id, ds) in entry.into_iter().flat_map(|e| &e.delivered) {
            if ds.withdrawn
                || ds.via_manifest
                || ds.name.is_empty()
                || plan.off_for(host, workspace, &ds.name).is_some()
                || plan.explicit_claims(host, workspace, &ds.name)
            {
                continue;
            }
            let identity = format!("{feed}/{}", ds.name);
            if !claimed.insert(identity.clone()) {
                continue;
            }
            collide(
                &mut by_name,
                &mut out.collisions,
                &ds.name,
                &feed,
                &ds.served_version,
            );
            let applied = applied_for_id(
                ctx,
                layout,
                skill_id,
                &ds.served_version,
                entry.and_then(|e| e.last_delivery_at),
            );
            out.rows.push(Row {
                item: StatusItem {
                    name: ds.name.clone(),
                    reference: identity,
                    source: format!("the {feed} feed"),
                    scope: scope.to_owned(),
                    via: ds.via_channels.first().cloned(),
                    attribution: attribution_of(ds),
                    version: applied.version,
                    applied_as_of: applied.as_of,
                    state: applied.state,
                    shadows: Vec::new(),
                },
                source_file: None,
                source_key: None,
                feed: Some(feed.clone()),
                pin: None,
                placements: applied.placements,
                bundle: true,
            });
            itemized += 1;
        }
        // A feed that has itemized NOTHING and has no completed exchange behind it: ONE honest
        // line for the workspace itself. The test is delivery PROVENANCE, never the existence of a
        // cache entry — a landed publish SEEDS its workspace's entry (host, address, its own
        // manifest-driven row) without any delivery having answered, and only the sweep ever
        // stamps `last_delivery_at`. Reading the seed as an exchange would delete this line and
        // drop the adopted feed out of the table entirely, since a `via_manifest` row itemizes
        // nothing here.
        //
        // A live session that has never delivered here is missing the EXCHANGE, not an apply — the
        // ordinary unknown state would promise `update` lands something, and this row cannot know
        // that the workspace assigns anything at all. Every other verdict (`session_state`'s
        // no-session and pending answers) is about access and stands.
        let exchanged = entry.is_some_and(|e| e.last_delivery_at.is_some());
        if !exchanged && itemized == 0 {
            let state = match session_state(all, host, workspace) {
                StatusItemState::Unknown => StatusItemState::NoDeliveryYet,
                settled => settled,
            };
            out.rows.push(Row {
                item: StatusItem {
                    name: workspace.clone(),
                    reference: feed.clone(),
                    source: format!("the {feed} feed"),
                    scope: scope.to_owned(),
                    via: None,
                    attribution: None,
                    version: None,
                    applied_as_of: None,
                    state,
                    shadows: Vec::new(),
                },
                source_file: file.clone(),
                source_key: file.as_ref().map(|_| feed.clone()),
                feed: Some(feed.clone()),
                pin: None,
                placements: Vec::new(),
                bundle: false,
            });
        } else if exchanged && !entry.is_some_and(assigns_anything) {
            // The other side of the same fact: the exchange DID complete and the workspace has
            // nothing for this person. No row can say that — the table just falls silent — so it
            // is said in words, the same words the sweep's receipt uses.
            out.empty_feeds.push(feed.clone());
        }
    }

    // 4. The `"off"` switches — their own rows (the file says them), and a note when the bundle
    // one withholds is not assigned to this person any more.
    for row in &plan.offs {
        let KeyShape::WorkspaceBundle {
            host,
            workspace,
            bundle,
        } = &row.shape
        else {
            continue;
        };
        if !feed_delivers(cache, host, workspace, bundle) {
            out.stale_offs
                .push(format!("off — not currently assigned: {}", row.reference));
        }
        let identity = row.shape.canonical();
        if !claimed.insert(identity.clone()) {
            continue;
        }
        out.rows.push(Row {
            item: StatusItem {
                name: row.display_name(),
                reference: identity,
                source: file.clone().unwrap_or_default(),
                scope: scope.to_owned(),
                via: None,
                attribution: None,
                version: None,
                applied_as_of: None,
                state: StatusItemState::Off,
                shadows: Vec::new(),
            },
            source_file: file.clone(),
            source_key: Some(row.reference.clone()),
            feed: None,
            pin: None,
            placements: Vec::new(),
            bundle: false,
        });
    }
    out
}

/// The deep single-bundle answer: the token resolved against the SAME lines the table renders (a
/// row reference, a leaf name, or a cached feed name; `@ws/…` and the canonical spellings ride
/// [`keys::parse_input`] with the one connected host as the default). The NEAREST scope's line
/// spells the detail; every matching line rides `items`.
fn deep_answer(
    rows: &[Row],
    all: &Sessions,
    token: &str,
) -> Result<(StatusDetail, Vec<StatusItem>), ClientError> {
    let hosts: BTreeSet<&str> = all.live().map(|s| s.host.as_str()).collect();
    let default_host = if hosts.len() == 1 {
        hosts.iter().copied().next()
    } else {
        None
    };
    let canonical = keys::parse_input(token, default_host)
        .ok()
        .map(|r| r.shape.canonical());
    let matched: Vec<&Row> = rows
        .iter()
        .filter(|r| canonical.as_deref() == Some(r.item.reference.as_str()) || r.item.name == token)
        .collect();
    let Some(first) = matched.first() else {
        return Err(ClientError::TargetNotFound {
            target: token.to_owned(),
        });
    };
    let detail = StatusDetail {
        name: first.item.name.clone(),
        source_file: first.source_file.clone(),
        source_key: first.source_key.clone(),
        feed: first.feed.clone(),
        attribution: first.item.attribution.clone(),
        version: first.item.version.clone(),
        pin: first.pin.clone(),
        placements: first.placements.clone(),
        state: first.item.state,
    };
    Ok((
        detail,
        matched.into_iter().map(|r| r.item.clone()).collect(),
    ))
}

// =================================================================================================
// The offline readers — the delivery cache, the per-scope stores, the sessions file.
// =================================================================================================

/// The honest local state of one bundle, read from one scope's store alone.
struct Applied {
    state: StatusItemState,
    version: Option<String>,
    as_of: Option<String>,
    placements: Vec<String>,
}

impl Applied {
    fn plain(state: StatusItemState) -> Self {
        Applied {
            state,
            version: None,
            as_of: None,
            placements: Vec::new(),
        }
    }

    fn unknown() -> Self {
        Applied::plain(StatusItemState::Unknown)
    }
}

/// The APPLIED state for one bundle in one scope's store: never applied → `Unknown` ("not applied
/// here yet"); a scanned draft → `LocalEdits`; a served version past the applied one → `Behind`;
/// else `Applied`, stamped "as of" the cache's last delivery — an offline fact, never a live
/// claim. Any unreadable doc degrades to `Unknown`.
fn applied_for_id(
    ctx: &Ctx<'_>,
    layout: Option<&Layout>,
    skill_id: &str,
    served_version: &str,
    last_delivery_at: Option<i64>,
) -> Applied {
    let Some(layout) = layout else {
        return Applied::unknown();
    };
    let Ok(sid) = crate::id::SkillId::parse(skill_id) else {
        return Applied::unknown();
    };
    if !ctx.fs.exists(&layout.skill_dir(&sid)) {
        return Applied::unknown();
    }
    let sp = layout.published(&sid);
    let Ok(Some(sync)) = doc::read_doc::<SyncState>(ctx.fs, &sp.sync) else {
        return Applied::unknown();
    };
    if sync.base_commit == ZERO_HEX {
        return Applied::unknown();
    }
    let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
        return Applied::unknown();
    };
    let version = Some(lock.base_commit.clone());
    let mut placements = Vec::new();
    let mut edited = false;
    if let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) {
        placements.clone_from(&map.placements);
        let scoped = super::pull::ctx_with_layout(ctx, layout);
        if let Ok(scans) = placement::scan_placements(&scoped, &map) {
            edited = scans
                .iter()
                .any(|s| matches!(s.status, placement::ScanStatus::Modified { .. }));
        }
    }
    if edited {
        return Applied {
            state: StatusItemState::LocalEdits,
            version,
            as_of: None,
            placements,
        };
    }
    if !served_version.is_empty() && served_version != lock.base_commit {
        return Applied {
            state: StatusItemState::Behind,
            version,
            as_of: None,
            placements,
        };
    }
    Applied {
        state: StatusItemState::Applied,
        version,
        as_of: last_delivery_at.map(super::connect::fmt_rfc3339_millis),
        placements,
    }
}

/// The scope store's `name → skill id` index, built once per scope on first need — the offline
/// answer for a row the delivery cache does not name (a repo skill, a local folder, a bundle
/// applied before the cache was written).
fn store_index(ctx: &Ctx<'_>, layout: &Layout) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(entries) = ctx.fs.read_dir(&layout.skills_dir()) else {
        return out;
    };
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(sid) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &layout.published(&sid).lock) else {
            continue;
        };
        out.insert(lock.name, id.to_owned());
    }
    out
}

/// The stored state for a NAME in this scope, when the store holds one (the index is built at most
/// once per scope). No served version is known this way, so a stored copy never reads `behind` —
/// only applied, edited, or unknown.
fn stored_by_name(
    ctx: &Ctx<'_>,
    layout: Option<&Layout>,
    index: &mut Option<BTreeMap<String, String>>,
    name: &str,
) -> Option<Applied> {
    let layout = layout?;
    let id = index
        .get_or_insert_with(|| store_index(ctx, layout))
        .get(name)?
        .clone();
    Some(applied_for_id(ctx, Some(layout), &id, "", None))
}

/// One cached delivery a line resolves against.
struct CacheHit<'a> {
    entry: &'a WorkspaceSync,
    skill_id: &'a str,
    ds: &'a DeliveredSkill,
}

/// The cache entry of one workspace, by its manifest-grammar address.
fn ws_entry<'a>(cache: &'a SyncStatus, host: &str, workspace: &str) -> Option<&'a WorkspaceSync> {
    cache
        .workspaces
        .values()
        .find(|e| e.host.as_deref() == Some(host) && e.workspace_name.as_deref() == Some(workspace))
}

/// The cached delivery of `<host>/<workspace>/<name>`, whatever asked for it.
fn cache_lookup<'a>(
    cache: &'a SyncStatus,
    host: &str,
    workspace: &str,
    name: &str,
) -> Option<CacheHit<'a>> {
    let entry = ws_entry(cache, host, workspace)?;
    let (skill_id, ds) = entry
        .delivered
        .iter()
        .find(|(_, d)| !d.withdrawn && d.name == name)?;
    Some(CacheHit {
        entry,
        skill_id,
        ds,
    })
}

/// Whether the workspace's last delivery ASSIGNED this bundle to the person (a feed row — a
/// manifest-driven delivery is demand, not an assignment).
fn feed_delivers(cache: &SyncStatus, host: &str, workspace: &str, bundle: &str) -> bool {
    ws_entry(cache, host, workspace).is_some_and(|e| {
        e.delivered
            .values()
            .any(|d| !d.withdrawn && !d.via_manifest && d.name == bundle)
    })
}

/// Whether the workspace's last delivery assigned this person ANYTHING at all — the same feed-row
/// test [`feed_delivers`] applies, over every name. What a local `"off"` row then withholds is a
/// separate, local fact: the workspace still assigned it.
fn assigns_anything(entry: &WorkspaceSync) -> bool {
    entry
        .delivered
        .values()
        .any(|d| !d.withdrawn && !d.via_manifest && !d.name.is_empty())
}

/// The assigned bundles of one workspace a FILE-backed person plan does not adopt: its feed does
/// not flow here, and no explicit row claims them.
fn assigned_not_adopted(
    plan: &ScopePlan,
    cache: &SyncStatus,
    host: &str,
    workspace: &str,
) -> usize {
    if plan.has_feed(host, workspace) {
        return 0;
    }
    let Some(entry) = ws_entry(cache, host, workspace) else {
        return 0;
    };
    entry
        .delivered
        .values()
        .filter(|d| {
            !d.withdrawn
                && !d.via_manifest
                && !d.name.is_empty()
                && !plan.explicit_claims(host, workspace, &d.name)
        })
        .count()
}

/// The delivery attribution, when the cache carries one.
fn attribution_of(ds: &DeliveredSkill) -> Option<String> {
    if let Some(who) = &ds.assigned_by {
        return Some(format!("assigned by {who}"));
    }
    ds.picked.then(|| "picked by you".to_owned())
}

/// The honest line for a workspace reference with NO cached delivery — phrased from the LOCAL
/// sessions file, never a server answer.
fn session_state(all: &Sessions, host: &str, workspace: &str) -> StatusItemState {
    match all.find_on_host(host, workspace) {
        None => StatusItemState::NotAvailable,
        Some(s) if s.status == sessions::SESSION_PENDING => StatusItemState::PendingSession,
        Some(s) if s.status == sessions::SESSION_ENDED => StatusItemState::NotAvailable,
        // Connected but never delivered here — the next `update` answers.
        Some(_) => StatusItemState::Unknown,
    }
}

/// Record one SET/feed delivery of `name`; a second set delivering the same name at a DIFFERENT
/// version is a real collision on disk — name the winner rather than let one copy vanish quietly.
fn collide(
    by_name: &mut BTreeMap<String, (String, String)>,
    notes: &mut Vec<String>,
    name: &str,
    reference: &str,
    version: &str,
) {
    match by_name.get(name) {
        Some((winner, won_at)) => {
            if !version.is_empty() && !won_at.is_empty() && won_at != version {
                notes.push(format!(
                    "{name}: `{winner}` and `{reference}` both deliver it — `{winner}` wins here"
                ));
            }
        }
        None => {
            by_name.insert(name.to_owned(), (reference.to_owned(), version.to_owned()));
        }
    }
}

/// A local-folder row's directory: absolute as spelled, `~/…` against the machine home, else
/// relative to the manifest's own folder.
fn local_dir(ctx: &Ctx<'_>, base: Option<&Path>, raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(roots) = &ctx.roots
    {
        return roots.home.join(rest);
    }
    if Path::new(raw).is_absolute() {
        return PathBuf::from(raw);
    }
    match base {
        Some(b) => b.join(raw.trim_start_matches("./")),
        None => PathBuf::from(raw),
    }
}

/// A path as a person reads it — the machine home abbreviated to `~`.
fn pretty(ctx: &Ctx<'_>, path: &Path) -> String {
    if let Some(roots) = &ctx.roots
        && let Ok(rest) = path.strip_prefix(&roots.home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Whether Claude Code is detected on this machine — its own precedence resolves personal skills
/// over project ones, which a cross-scope version split must disclose. A read-only probe.
fn claude_code_detected(ctx: &Ctx<'_>) -> bool {
    ctx.roots.as_ref().is_some_and(|r| {
        topos_harness::registry::detected_harnesses(&r.home, r.cwd.as_deref())
            .iter()
            .any(|h| h.slug == "claude-code")
    })
}

/// A version/commit as a note spells it.
fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::ctx::Ctx;
    use crate::enroll;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::manifest::MANIFEST_FILE;
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;
    use topos_types::PERSISTED_SCHEMA_VERSION;

    /// A self-cleaning temp MACHINE home (RAII): `<home>/.topos` is the sidecar, `<home>` the
    /// root the manifest walk and the placement probes resolve against.
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-status-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn layout(&self) -> Layout {
            Layout::new(&self.0.join(".topos"))
        }

        /// Write the GLOBAL manifest (`~/.topos/topos.toml`).
        fn global(&self, text: &str) {
            let dir = self.0.join(".topos");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(MANIFEST_FILE), text).unwrap();
        }

        /// A session for `<host>/<ws>`.
        fn session(&self, host: &str, ws_id: &str, ws: &str, status: &str) {
            crate::sessions::upsert_session(
                &RealFs,
                &self.layout(),
                crate::sessions::Session {
                    host: host.to_owned(),
                    base_url: format!("https://{host}/api"),
                    workspace_id: ws_id.to_owned(),
                    workspace_name: ws.to_owned(),
                    display_name: ws.to_uppercase(),
                    session_id: format!("sn_{ws}"),
                    credential: "c".to_owned(),
                    status: status.to_owned(),
                    logged_in_at: 1,
                },
            )
            .unwrap();
        }

        /// Record one workspace's last delivery in the offline cache.
        fn cache(
            &self,
            ws_id: &str,
            host: &str,
            ws: &str,
            delivered: Vec<(String, DeliveredSkill)>,
            declined: Vec<(String, String)>,
        ) {
            crate::sync_status::record(
                &RealFs,
                &self.layout(),
                &[(
                    ws_id.to_owned(),
                    WorkspaceSync {
                        host: Some(host.to_owned()),
                        workspace_name: Some(ws.to_owned()),
                        last_delivery_at: Some(1_700_000_000_000),
                        last_report_at: None,
                        staleness_window_ms: 0,
                        delivered: delivered.into_iter().collect(),
                        declined: declined.into_iter().collect(),
                    },
                )],
            )
            .unwrap();
        }

        /// A PRE-SESSION-MODEL cache entry: one the document still tolerates, with no host and no
        /// address recorded.
        fn hostless_cache(&self, ws_id: &str, delivered: Vec<(String, DeliveredSkill)>) {
            crate::sync_status::record(
                &RealFs,
                &self.layout(),
                &[(
                    ws_id.to_owned(),
                    WorkspaceSync {
                        host: None,
                        workspace_name: None,
                        last_delivery_at: Some(1_700_000_000_000),
                        last_report_at: None,
                        staleness_window_ms: 0,
                        delivered: delivered.into_iter().collect(),
                        declined: BTreeMap::new(),
                    },
                )],
            )
            .unwrap();
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A cached FEED row (assigned to the person; never manifest-driven). `by = None` reads as the
    /// person's own pick.
    fn assigned(name: &str, by: Option<&str>) -> (String, DeliveredSkill) {
        let tag = name.chars().next().unwrap_or('x');
        (
            format!("topos_{}", std::iter::repeat_n(tag, 32).collect::<String>()),
            DeliveredSkill {
                name: name.to_owned(),
                review_required: false,
                served_version: "d".repeat(64),
                withdrawn: false,
                via_channels: vec!["everyone".to_owned()],
                via_manifest: false,
                assigned_by: by.map(str::to_owned),
                picked: by.is_none(),
            },
        )
    }

    fn snapshot_at(home: &TempHome, cwd: &Path) -> StatusData {
        run(home, cwd, None).expect("status snapshot")
    }

    fn run(home: &TempHome, cwd: &Path, bundle: Option<&str>) -> Result<StatusData, ClientError> {
        let fs = RealFs;
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &RealIds,
            clock: &RealClock,
            device_id: String::new(),
            layout: home.layout(),
            harness: &harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: home.0.clone(),
                cwd: Some(cwd.to_path_buf()),
            }),
        };
        status_snapshot(&ctx, bundle)
    }

    /// NO global file: the machine behaves exactly as if one feed row per connected workspace were
    /// written — the cached assignments itemize, attributed, sourced to the feed itself.
    #[test]
    fn the_implicit_recipe_itemizes_each_workspace_feed() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", Some("Dana")), assigned("notes", None)],
            Vec::new(),
        );

        let d = snapshot_at(&home, &cwd);
        let row = |n: &str| d.items.iter().find(|i| i.name == n).expect("a row");
        assert_eq!(row("deploy").source, "the topos.sh/acme feed");
        assert_eq!(row("deploy").scope, "person");
        assert_eq!(row("deploy").reference, "topos.sh/acme/deploy");
        assert_eq!(
            row("deploy").attribution.as_deref(),
            Some("assigned by Dana")
        );
        assert_eq!(row("notes").attribution.as_deref(), Some("picked by you"));
        assert_eq!(row("deploy").via.as_deref(), Some("everyone"));
        // Never applied here (no store): the honest not-applied-yet state, no false claim.
        assert!(matches!(row("deploy").state, StatusItemState::Unknown));
        assert_eq!(d.profile_skills, 2);
        assert_eq!(d.awaiting_first_sync, Some(2));
        // The implicit recipe adopts everything the workspace assigns; nothing to disclose.
        assert_eq!(d.regimes.len(), 1);
        assert_eq!(d.regimes[0].regime, "adopting all assigned");
        assert!(d.notes.is_empty(), "{:?}", d.notes);
    }

    /// The delivery cache OUTLIVES a logout, so the count must be session-scoped or a workspace
    /// nobody dials any more keeps claiming to assign bundles — beside a table that shows none of
    /// them, next to the feed the person actually has.
    #[test]
    fn a_cached_workspace_without_a_session_assigns_nothing_here() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        // acme is logged in and assigns nothing; the logged-OUT workspace's cache entry survives.
        home.cache("w_acme", "topos.sh", "acme", Vec::new(), Vec::new());
        home.cache(
            "w_gone",
            "topos.sh",
            "gone",
            vec![assigned("ghost", Some("Dana"))],
            Vec::new(),
        );

        let d = snapshot_at(&home, &cwd);
        assert_eq!(d.profile_skills, 0);
        assert_eq!(d.awaiting_first_sync, Some(0));
        assert!(!d.items.iter().any(|i| i.name == "ghost"), "{:?}", d.items);

        // A session for it — however it is minted — makes the same rows count again.
        home.session(
            "topos.sh",
            "w_gone",
            "gone",
            crate::sessions::SESSION_ACTIVE,
        );
        let d = snapshot_at(&home, &cwd);
        assert_eq!(d.profile_skills, 1);
        assert_eq!(d.awaiting_first_sync, Some(1));
        assert!(d.items.iter().any(|i| i.name == "ghost"));
    }

    /// PENDING counts, ENDED does not: a pending session's workspace still assigns (only an
    /// owner's approval gates the bytes), while an ended one has taken its access away — what it
    /// delivered is frozen in place, not assigned.
    #[test]
    fn a_pending_session_counts_and_an_ended_one_does_not() {
        for (status, expected) in [
            (crate::sessions::SESSION_PENDING, 1),
            (crate::sessions::SESSION_ENDED, 0),
        ] {
            let home = TempHome::new();
            let cwd = home.0.join("plain");
            std::fs::create_dir_all(&cwd).unwrap();
            home.session("topos.sh", "w_acme", "acme", status);
            home.cache(
                "w_acme",
                "topos.sh",
                "acme",
                vec![assigned("deploy", None)],
                Vec::new(),
            );

            let d = snapshot_at(&home, &cwd);
            assert_eq!(d.profile_skills, expected, "status {status}");
            assert_eq!(d.awaiting_first_sync, Some(expected), "status {status}");
        }
    }

    /// A workspace id is an OPAQUE string each server mints on its own, so it is not an address.
    /// Matching the live set on the id alone would let a session on one host resurrect another
    /// host's stale cache entry — a count with no row anywhere in the table to back it.
    #[test]
    fn the_live_test_is_keyed_by_host_and_id_not_the_id_alone() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        // Logged into `topos.sh`; the SAME id is cached from a different server.
        home.session(
            "topos.sh",
            "w_shared",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_shared",
            "other.test",
            "acme",
            vec![assigned("ghost", Some("Dana"))],
            Vec::new(),
        );

        let d = snapshot_at(&home, &cwd);
        assert_eq!(d.profile_skills, 0);
        assert_eq!(d.awaiting_first_sync, Some(0));
        assert!(!d.items.iter().any(|i| i.name == "ghost"), "{:?}", d.items);

        // The HOST is what decides: a session on the server that cached it counts the same rows.
        home.session(
            "other.test",
            "w_shared",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        let d = snapshot_at(&home, &cwd);
        assert_eq!(d.profile_skills, 1);
        assert!(d.items.iter().any(|i| i.name == "ghost"), "{:?}", d.items);
    }

    /// A cache entry with no recorded host cannot be placed on any server — and `ws_entry` (the
    /// table's own reader) needs a host, so no row can ever show it. It counts for nobody.
    #[test]
    fn a_cache_entry_with_no_recorded_host_counts_for_nobody() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.hostless_cache("w_acme", vec![assigned("ghost", Some("Dana"))]);

        let d = snapshot_at(&home, &cwd);
        assert_eq!(d.profile_skills, 0);
        assert_eq!(d.awaiting_first_sync, Some(0));
        assert!(!d.items.iter().any(|i| i.name == "ghost"), "{:?}", d.items);
    }

    /// A feed that has never delivered here is missing the EXCHANGE, not an apply — its one
    /// placeholder line must not promise `update` lands something it cannot know exists.
    #[test]
    fn a_feed_with_no_delivery_yet_promises_only_the_exchange() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );

        let d = snapshot_at(&home, &cwd);
        let row = d.items.iter().find(|i| i.name == "acme").expect("the feed");
        assert_eq!(row.reference, "topos.sh/acme");
        assert_eq!(row.source, "the topos.sh/acme feed");
        assert!(
            matches!(row.state, StatusItemState::NoDeliveryYet),
            "{:?}",
            row.state
        );
        // The access verdicts are a different fact and still stand.
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_PENDING,
        );
        let d = snapshot_at(&home, &cwd);
        let row = d.items.iter().find(|i| i.name == "acme").expect("the feed");
        assert!(
            matches!(row.state, StatusItemState::PendingSession),
            "{:?}",
            row.state
        );
    }

    /// A cache ENTRY is not an exchange. A landed publish seeds its workspace's entry — host,
    /// address, and its own manifest-driven row — with no delivery having answered; only the sweep
    /// stamps `last_delivery_at`. A `via_manifest` row itemizes nothing under a feed, so reading
    /// the seed as an exchange would drop the adopted feed out of the table with nothing said.
    #[test]
    fn a_publish_seeded_entry_keeps_the_never_delivered_placeholder() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        crate::sync_status::merge_delivered(
            &RealFs,
            &home.layout(),
            "w_acme",
            "topos.sh",
            "acme",
            "topos_pppppppppppppppppppppppppppppppp",
            DeliveredSkill {
                name: "deploy".to_owned(),
                served_version: "d".repeat(64),
                via_manifest: true,
                ..DeliveredSkill::default()
            },
        )
        .unwrap();
        // The seed's shape is the whole point: an entry exists, no exchange is recorded.
        let cache = crate::sync_status::read(&RealFs, &home.layout()).unwrap();
        assert_eq!(cache.workspaces["w_acme"].last_delivery_at, None);

        let d = snapshot_at(&home, &cwd);
        let row = d.items.iter().find(|i| i.name == "acme").expect("the feed");
        assert_eq!(row.reference, "topos.sh/acme");
        assert_eq!(row.source, "the topos.sh/acme feed");
        assert!(
            matches!(row.state, StatusItemState::NoDeliveryYet),
            "{:?}",
            row.state
        );
        // A manifest-driven row is demand, not an assignment — it counts for nothing here, and
        // nothing claims an exchange that never happened.
        assert_eq!(d.profile_skills, 0);
        assert!(
            !d.notes.iter().any(|n| n.contains("exchanged")),
            "{:?}",
            d.notes
        );
    }

    /// The other side of that fact: the exchange COMPLETED and the workspace had nothing for this
    /// person. No row can carry that — the table simply falls silent — so it is said in words,
    /// once, in the sweep receipt's own vocabulary.
    #[test]
    fn a_completed_exchange_that_brought_nothing_says_so() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache("w_acme", "topos.sh", "acme", Vec::new(), Vec::new());

        let d = snapshot_at(&home, &cwd);
        assert_eq!(
            d.notes,
            vec!["topos.sh/acme: exchanged — nothing assigned to you yet"]
        );
        // An exchanged feed never wears the never-delivered placeholder.
        assert!(d.items.is_empty(), "{:?}", d.items);

        // A workspace that DID assign something says nothing of the sort.
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            Vec::new(),
        );
        let d = snapshot_at(&home, &cwd);
        assert!(
            !d.notes.iter().any(|n| n.contains("exchanged")),
            "{:?}",
            d.notes
        );
    }

    /// A global file is COMPLETE: with no feed row the assignments do NOT flow — only the file's
    /// own rows deliver, and the withheld ones are disclosed loudly, with the way back.
    #[test]
    fn a_file_backed_plan_withholds_the_feed_and_says_so() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![
                assigned("deploy", Some("Dana")),
                assigned("notes", None),
                assigned("triage", None),
            ],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n");

        let d = snapshot_at(&home, &cwd);
        // The file's own row delivers, sourced to the file — the other assignments do not flow.
        let deploy = d.items.iter().find(|i| i.name == "deploy").expect("a row");
        assert!(deploy.source.ends_with("topos.toml"), "{}", deploy.source);
        assert_eq!(deploy.attribution.as_deref(), Some("assigned by Dana"));
        assert!(!d.items.iter().any(|i| i.name == "notes"));
        assert!(!d.items.iter().any(|i| i.name == "triage"));
        // The LOUD line names both counts and the exact way back.
        let loud = d
            .notes
            .iter()
            .find(|n| n.contains("not adopted here"))
            .expect("the loud note");
        assert!(loud.contains("global manifest adopts 1 bundles"), "{loud}");
        assert!(loud.contains("2 assigned bundles"), "{loud}");
        assert!(loud.contains("`topos add -g @acme`"), "{loud}");
        // The regime says the same thing in one phrase.
        assert_eq!(
            d.regimes[0].regime,
            "explicit: 1 bundles; 2 assigned not adopted here"
        );
    }

    /// `"off"` is a statement like any other: it renders as its own row. An off switch for a
    /// bundle nobody assigns any more is inert — that gets a note, never a silent line.
    #[test]
    fn off_rows_render_and_a_stale_one_is_disclosed() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("noisy", None), assigned("deploy", None)],
            Vec::new(),
        );
        home.global(
            "[bundles]\n\
             \"topos.sh/acme\" = \"*\"\n\
             \"topos.sh/acme/noisy\" = \"off\"\n\
             \"topos.sh/acme/gone\" = \"off\"\n",
        );

        let d = snapshot_at(&home, &cwd);
        let noisy = d
            .items
            .iter()
            .find(|i| i.name == "noisy")
            .expect("the off row");
        assert!(matches!(noisy.state, StatusItemState::Off));
        // The off bundle is withheld from the feed's itemization (one row, not two).
        assert_eq!(d.items.iter().filter(|i| i.name == "noisy").count(), 1);
        // The still-assigned bundle keeps flowing.
        assert!(d.items.iter().any(|i| i.name == "deploy"));
        // The stale switch is called out by reference; the live one needs no note.
        assert!(
            d.notes
                .iter()
                .any(|n| n == "off — not currently assigned: topos.sh/acme/gone"),
            "{:?}",
            d.notes
        );
        assert!(!d.notes.iter().any(|n| n.contains("acme/noisy")));
        assert_eq!(d.regimes[0].regime, "adopting all assigned, 2 off");
    }

    /// A bare `"*"` row for a bundle the flowing feed already delivers is harmless — and inert.
    #[test]
    fn a_redundant_row_is_named_as_adding_nothing() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/deploy\" = \"*\"\n");

        let d = snapshot_at(&home, &cwd);
        assert!(
            d.notes
                .iter()
                .any(|n| n == "topos.sh/acme/deploy adds nothing — the feed already delivers it"),
            "{:?}",
            d.notes
        );
        // Deduped by identity: the explicit row wins, the feed does not repeat it.
        assert_eq!(d.items.iter().filter(|i| i.name == "deploy").count(), 1);
    }

    /// Declined on the web, delivered here anyway: the file outranks the person's own web choice,
    /// so the disagreement is stated rather than left a mystery.
    #[test]
    fn a_declined_bundle_a_row_still_delivers_is_disclosed() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            vec![(
                "topos_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
                "legacy".to_owned(),
            )],
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/legacy\" = \"*\"\n");

        let d = snapshot_at(&home, &cwd);
        assert!(
            d.notes
                .iter()
                .any(|n| n
                    == "legacy: declined on the web, delivered here by your global manifest"),
            "{:?}",
            d.notes
        );
    }

    /// The regime sentence per workspace phrases BOTH shapes from the same plan.
    #[test]
    fn regimes_phrase_the_adopting_and_the_explicit_shapes() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.session(
            "topos.sh",
            "w_beta",
            "beta",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            Vec::new(),
        );
        home.cache(
            "w_beta",
            "topos.sh",
            "beta",
            vec![assigned("triage", None), assigned("notes", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/beta/triage\" = \"*\"\n");

        let d = snapshot_at(&home, &cwd);
        let regime = |ws: &str| {
            d.regimes
                .iter()
                .find(|r| r.workspace == ws)
                .map(|r| r.regime.clone())
                .unwrap_or_default()
        };
        assert_eq!(regime("acme"), "adopting all assigned");
        assert_eq!(
            regime("beta"),
            "explicit: 1 bundles; 1 assigned not adopted here"
        );
    }

    /// The NEAREST project manifest governs its subtree WHOLE — an ancestor's rows never blend in.
    #[test]
    fn only_the_nearest_project_manifest_governs_the_project_scope() {
        let home = TempHome::new();
        let repo = home.0.join("repo");
        let nested = repo.join("services/api");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            repo.join(MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/repo-wide\" = \"*\"\n",
        )
        .unwrap();
        std::fs::write(
            nested.join(MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/api-only\" = \"*\"\n",
        )
        .unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );

        let d = snapshot_at(&home, &nested);
        let project: Vec<&StatusItem> = d.items.iter().filter(|i| i.scope == "project").collect();
        assert_eq!(project.len(), 1, "{project:?}");
        assert_eq!(project[0].name, "api-only");
        assert!(
            project[0].source.ends_with("services/api/topos.toml"),
            "{}",
            project[0].source
        );
        // Connected but never delivered here — the honest local answer, nothing dialed.
        assert!(matches!(project[0].state, StatusItemState::Unknown));
    }

    /// `status <bundle>` names WHERE it comes from: a row spells its file and its key; a feed
    /// delivery spells the feed and who aimed it. A token nothing delivers refuses uniformly.
    #[test]
    fn the_deep_answer_names_the_row_or_the_feed() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", Some("Dana")), assigned("notes", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/deploy\" = \"*\"\n");

        // The row-delivered bundle: the file + the spelled key.
        let d = run(&home, &cwd, Some("deploy")).expect("the deep answer");
        let detail = d.detail.expect("a detail");
        assert_eq!(detail.name, "deploy");
        assert!(
            detail
                .source_file
                .as_deref()
                .is_some_and(|f| f.ends_with("topos.toml")),
            "{:?}",
            detail.source_file
        );
        assert_eq!(detail.source_key.as_deref(), Some("topos.sh/acme/deploy"));
        assert_eq!(detail.feed, None);
        assert_eq!(detail.attribution.as_deref(), Some("assigned by Dana"));
        // The table narrows to the answered bundle.
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].name, "deploy");

        // The feed-delivered one: the feed, not a file. The `@ws/name` spelling resolves too.
        let d = run(&home, &cwd, Some("@acme/notes")).expect("the deep answer");
        let detail = d.detail.expect("a detail");
        assert_eq!(detail.name, "notes");
        assert_eq!(detail.source_file, None);
        assert_eq!(detail.source_key, None);
        assert_eq!(detail.feed.as_deref(), Some("topos.sh/acme"));
        assert_eq!(detail.attribution.as_deref(), Some("picked by you"));

        // A token no scope delivers is the uniform not-found.
        let err = run(&home, &cwd, Some("nowhere")).expect_err("an unknown token");
        assert!(matches!(err, ClientError::TargetNotFound { .. }), "{err:?}");
    }

    /// Every file under `dir`, as `relative path → bytes` — the byte-identity oracle.
    fn tree_bytes(dir: &PathBuf) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &PathBuf, dir: &PathBuf, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap_or_default());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    /// The read-only promise, proven byte-for-byte: a status run over a sidecar holding a
    /// PENDING-RECOVERY fixture (an expired enrollment WAL the ordinary start-of-command sweep
    /// would reap) leaves every byte in place — the snapshot AND the trigger probe (the exact
    /// pre-recovery pair the composition root's fast path runs) write nothing, and the same
    /// fixture is then shown POTENT: the recovery sweep the fast path skips does mutate it.
    #[test]
    fn status_leaves_a_pending_recovery_sidecar_byte_identical() {
        let home = TempHome::new();
        let fs = RealFs;
        let layout = Layout::new(&home.0);
        enroll::write_wal(
            &fs,
            &layout,
            &enroll::PendingEnrollment {
                schema_version: PERSISTED_SCHEMA_VERSION,
                host: String::new(),
                base_url: "https://topos.sh/api".to_owned(),
                preselect: "acme".to_owned(),
                intent: enroll::EnrollIntentDoc::Session,
                device_code: "dc_expired".to_owned(),
                user_code: "XXXX-YYYY".to_owned(),
                verification_uri: "https://topos.sh/verify".to_owned(),
                interval_secs: 5,
                // Long expired — recovery would reap this WAL on any ordinary command.
                loopback: false,
                expires_at_millis: 1_000,
            },
        )
        .unwrap();

        let before = tree_bytes(&home.0);
        assert!(!before.is_empty(), "the fixture is on disk");

        // The exact pair the composition root's pre-recovery fast path runs.
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &RealIds,
            clock: &RealClock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: &harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: None,
        };
        let data = status_snapshot(&ctx, None).expect("status snapshot");
        assert!(data.sessions.is_empty());
        let _ = crate::ops::probe_detected(&home.0, None, &harness, &fs);
        assert_eq!(
            before,
            tree_bytes(&home.0),
            "a status run must leave the sidecar byte-identical"
        );

        // The fixture is potent: the sweep the fast path skips DOES mutate it (the WAL is reaped).
        crate::sidecar::recover(&fs, &layout, i64::MAX, &mut Vec::new()).unwrap();
        assert_ne!(
            before,
            tree_bytes(&home.0),
            "the recovery sweep reaps the expired WAL — proving status really skipped it"
        );
    }

    #[test]
    fn a_fresh_install_reads_not_connected_with_nothing_delivered() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let data = snapshot_at(&home, &cwd);
        assert!(data.sessions.is_empty() && !data.signed_in);
        assert_eq!(data.server, None);
        assert_eq!(data.profile_skills, 0);
        assert_eq!(data.awaiting_first_sync, Some(0));
        assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
        assert!(data.triggers.is_empty());
        assert!(data.items.is_empty() && data.regimes.is_empty() && data.notes.is_empty());
        assert!(data.detail.is_none());
    }
}
