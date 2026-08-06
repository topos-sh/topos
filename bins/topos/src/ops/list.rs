//! `list` — the INVENTORY, offline by default: what is installed where you stand, per scope. The
//! default body is the here-scope's rows in full (the nearest `topos.toml` covering the cwd, else
//! the machine); `-g` is the machine scope alone, `--all` both. Whatever the invocation does not
//! show is never invisible and never dumped — the machine scope and the untracked discoveries
//! each ride ONE summary line ending in the exact command that expands them, and a signed-in TTY
//! points at `list --remote` for what the workspaces offer.
//!
//! The optional views: `list <name>` is the one-skill deep dive (which file and line-key — or
//! which feed — delivers it) over the SAME scopes the invocation selects, falling back to the
//! store records no row claims, so it answers for exactly what the listing shows and no more;
//! `-a <slug>` is the agent-eye view (each skills dir that harness
//! reads from this folder, entries marked managed or untracked — deliberately spanning both
//! scopes); `--untracked` is the full discovery listing; `--remote` (the one networked arm) reads
//! each live session's channel index + catalog and annotates every skill with this machine's
//! adoption state. `--footprint` reports every topos-owned path outside skill dirs.
//!
//! Rows come from the SAME per-scope resolution `status` reads ([`super::inventory`]): project
//! rows from the checkout's own store, machine rows from `~/.topos/` — one resolution, two views.
//! The offline states are cache facts ("as of last sync"), never live claims.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use topos_harness::coverage;
use topos_harness::registry::{self, SkillScope};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{
    AgentView, AgentViewDir, AgentViewEntry, BucketTruncation, DetachCause, ListData, ListScope,
    ListScopeSummary, RemoteAdoption, RemoteChannel, RemoteSkill, RemoteWorkspace, SkillEntry,
    SkillStatus, StatusItemState, UntrackedEntry, UntrackedSummary,
};

use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;
use crate::plane::DirectorySource;
use crate::sessions::{Session, Sessions};
use crate::sidecar;
use crate::sync_status::{DeliveredSkill, SyncStatus};

use super::inventory::{self, Resolved, Row, ScopeResolution, ScopeView, ZERO_HEX};

/// The filesystem roots `list` probes for **untracked** skills: the user home (every harness's
/// global skill dir resolves under it) and, optionally, the current project dir (for repo-scoped
/// skills). `None` (no `$HOME`) degrades to no discovery — the untracked summary honestly
/// disappears rather than lying a zero.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveryRoots {
    pub home: PathBuf,
    pub cwd: Option<PathBuf>,
}

/// One `list` invocation, parsed: which scope(s) show in full, which optional view (at most one —
/// clap's conflicts enforce it), and the row filters.
#[derive(Debug, Default)]
pub(crate) struct ListRequest {
    /// The scope selection (`-g` / `--all`; default = where you stand).
    pub view: ScopeView,
    /// `--untracked` — the full discovery listing plus one summary per tracked scope.
    pub untracked: bool,
    /// `list <name>` — the one-skill deep dive.
    pub name: Option<String>,
    /// `-a <slug>` — the agent-eye view.
    pub agent: Option<String>,
    /// `--remote` — the live per-workspace catalog view.
    pub remote: bool,
    /// `--footprint` — topos-owned paths outside skill dirs.
    pub footprint: bool,
    /// `--channel` selectors (rows delivered via that channel), ALL-OR-NONE.
    pub channels: Vec<String>,
    /// `--skill` selectors (rows by name), ALL-OR-NONE.
    pub skills: Vec<String>,
    /// The global `--workspace` filter, canonicalized to an id — narrows the `--remote` reads.
    pub workspace: Option<String>,
}

/// A `list` run's typed result: the schema-pinned envelope payload plus the isolated per-workspace
/// `--remote` read failures (one stable-shape line each — a transport fault reading one
/// workspace's catalog skips it with a warning rather than failing the whole `list`).
#[derive(Debug)]
pub(crate) struct ListOutcome {
    pub data: ListData,
    pub warnings: Vec<String>,
    /// TTY-only: this run was `--untracked`, so the tracked scopes render as one summary line
    /// each (the wire carries them in full either way).
    pub untracked_view: bool,
}

/// The per-session directory connector `--remote` dials (one credentialed client per live
/// session) — a seam so the tests inject a fake.
pub(crate) type SessionDirectory<'a> = &'a dyn Fn(&Session) -> Box<dyn DirectorySource>;

/// Inventory the machine under a full [`ListRequest`], row-capped by `page` (applied PER BUCKET
/// with a [`BucketTruncation`] marker per capped bucket; the buckets are the scope names plus
/// `untracked`, `remote` and `agent`). A bucket that NESTS rows is paged at its unbounded axis —
/// `--remote` at each workspace's skills, `-a` at each dir's entries — with one summed marker, so
/// no view can answer with an unbounded row count.
///
/// # Errors
/// [`ClientError::TargetNotFound`] when a deep-dive name resolves nowhere;
/// [`ClientError::InvalidArgument`] for an unknown `-a` slug; [`ClientError::SessionRequired`]
/// for `--remote` with no live session; [`ClientError::NoSuchSkill`] / the uniform not-found for
/// a filter selector matching nothing; otherwise a read failure.
pub(crate) fn list_with(
    ctx: &Ctx<'_>,
    req: &ListRequest,
    discover: Option<DiscoveryRoots>,
    remote: Option<SessionDirectory<'_>>,
    page: super::RowPage,
) -> Result<ListOutcome, ClientError> {
    let (all, cache) = inventory::read_sources(ctx)?;
    let signed_in = all.live().count() > 0;
    let resolved = inventory::resolve(ctx, &all, &cache)?;
    let mut warnings: Vec<String> = Vec::new();
    let mut data = ListData {
        signed_in,
        ..ListData::default()
    };
    let mut truncated: Vec<BucketTruncation> = Vec::new();
    fn mark(bucket: &str, (shown, total): (usize, usize), out: &mut Vec<BucketTruncation>) {
        if shown < total {
            out.push(BucketTruncation {
                bucket: bucket.to_owned(),
                shown: shown as u64,
                total: total as u64,
            });
        }
    }

    // The scope sections shown in full. Under `--untracked` every tracked scope covering the cwd
    // rides along (the TTY renders each as one summary line), so nothing is invisible beside the
    // discovery listing.
    let in_project = resolved.project().is_some();
    let sections: Vec<&ScopeResolution> = if req.untracked {
        resolved.scopes.iter().collect()
    } else {
        match req.view {
            ScopeView::Here => vec![resolved.project().unwrap_or_else(|| resolved.machine())],
            ScopeView::Machine => vec![resolved.machine()],
            ScopeView::All => resolved.scopes.iter().collect(),
        }
    };

    // The external sources' health, over the sections THIS INVOCATION SHOWS — computed BEFORE any
    // of the focused views, every one of which returns early. `--agent`, `--remote` and the
    // one-skill deep dive all answer about machines that track external sources too, and the
    // shared tail renders this block for all of them; leaving it empty there would make the
    // contract true only of the view that happens to fall through.
    data.forge = inventory::forge_sources(ctx, &sections);

    if req.footprint {
        // The `~/.topos/` walk PLUS any harness config path topos holds a managed entry in
        // (disclosed, never deleted) — every topos-owned path outside skill dirs.
        let mut paths = sidecar::footprint(ctx.fs, &ctx.layout)?;
        paths.extend(
            ctx.harness
                .uninstall_footprint()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        paths.sort();
        data.footprint = Some(paths);
    }

    // The agent-eye view — deliberately spans both scopes.
    if let Some(slug) = &req.agent {
        let mut view = agent_view(ctx, &resolved, discover.as_ref(), slug)?;
        // The page applies to each DIR's entries (the unbounded axis — a skills dir holds however
        // many folders the person put there); the dirs themselves are a handful and ride whole. One
        // `agent` marker sums the drop across them, so a paged view never silently shortens a dir.
        if page.is_active() {
            let mut shown = 0;
            let mut total = 0;
            for dir in &mut view.dirs {
                let (s, t) = page.apply(&mut dir.entries);
                shown += s;
                total += t;
            }
            mark("agent", (shown, total), &mut truncated);
        }
        data.agent_view = Some(view);
        data.truncated = truncated;
        return Ok(ListOutcome {
            data,
            warnings,
            untracked_view: req.untracked,
        });
    }

    // The live per-workspace catalog view — the one networked arm.
    if req.remote {
        let connect = remote.expect("the composition root passes the --remote connector");
        data.remote = remote_view(
            &resolved,
            &all,
            req.workspace.as_deref(),
            connect,
            &mut warnings,
        )?;
        // The page applies to each workspace's SKILLS — the unbounded axis. Paging the workspace
        // WRAPPERS instead left every nested catalog whole, so a `--limit 5` over one workspace
        // holding 400 skills emitted all 400. The channels ride whole: a workspace has a handful,
        // and they are the index a person pages the skills WITH. One `remote` marker sums the drop
        // across workspaces.
        if page.is_active() {
            let mut shown = 0;
            let mut total = 0;
            for ws in &mut data.remote {
                let (s, t) = page.apply(&mut ws.skills);
                shown += s;
                total += t;
            }
            mark("remote", (shown, total), &mut truncated);
        }
        data.truncated = truncated;
        return Ok(ListOutcome {
            data,
            warnings,
            untracked_view: req.untracked,
        });
    }

    // The one-skill deep dive, over the sections THIS INVOCATION SELECTS — the scope flags mean
    // the same thing here as on the listing, so `-g` answers from the machine scope alone even
    // inside a project (the default and `--all` read project-then-machine, precedence order).
    // A name no resolved row claims falls back to those sections' GHOSTS: the built-in and every
    // detached copy are shown by a plain `list`, so `list <name>` must answer for them too — a
    // deep dive that says "not found" about a row the listing prints is just wrong.
    if let Some(name) = &req.name {
        let dive = dive_sections(&resolved, req.view);
        data.detail = Some(match inventory::detail_for(&dive, &all, name) {
            Ok(detail) => detail,
            Err(miss) => {
                let dive_ghosts: Vec<Vec<Ghost>> = dive
                    .iter()
                    .map(|section| store_ghosts(ctx, section, &cache, &all, signed_in))
                    .collect();
                ghost_detail(&dive, &dive_ghosts, name).ok_or(miss)?
            }
        });
        return Ok(ListOutcome {
            data,
            warnings,
            untracked_view: req.untracked,
        });
    }

    // Each shown section's GHOST rows (store records no resolved row claims — the built-in
    // meta-skill, detached/frozen copies): installed is installed, so they are never invisible.
    let ghosts: Vec<Vec<Ghost>> = sections
        .iter()
        .map(|section| store_ghosts(ctx, section, &cache, &all, signed_in))
        .collect();

    // The `--channel`/`--skill` filters, ALL-OR-NONE across the SHOWN sections (ghosts count as
    // matchable rows — a detached copy is still findable by its name or its cached channel).
    let narrowed = !(req.channels.is_empty() && req.skills.is_empty());
    if narrowed {
        validate_filters(&sections, &ghosts, &req.skills, &req.channels)?;
    }
    let keeps = |name: &str, via: &[String]| -> bool {
        if !narrowed {
            return true;
        }
        req.skills.iter().any(|s| s == name)
            || req.channels.iter().any(|c| via.iter().any(|v| v == c))
    };
    for (section, section_ghosts) in sections.iter().zip(&ghosts) {
        let mut rows: Vec<SkillEntry> = section
            .inventory_rows()
            .filter(|r| keeps(&r.name, &r.via_channels))
            .map(skill_entry)
            .collect();
        rows.extend(
            section_ghosts
                .iter()
                .filter(|g| keeps(&g.entry.skill, &g.via_channels))
                .map(|g| g.entry.clone()),
        );
        if page.is_active() {
            mark(section.scope, page.apply(&mut rows), &mut truncated);
        }
        data.scopes.push(ListScope {
            scope: section.scope.to_owned(),
            manifest: section.manifest.clone(),
            rows,
        });
    }

    if req.untracked {
        // The FULL discovery listing (grouped by folder on the TTY).
        if let Some(roots) = &discover {
            data.untracked = discover_untracked(ctx, roots)?;
            if page.is_active() {
                mark("untracked", page.apply(&mut data.untracked), &mut truncated);
            }
        }
    } else if !narrowed {
        // The quiet rule's summaries: the machine scope when the body shows only the project, and
        // the untracked discoveries (absent when nothing was found — nothing is being withheld).
        if in_project && matches!(req.view, ScopeView::Here) {
            let machine = resolved.machine();
            // The ghost rows count into the summary's `skills` too — a summary that undercounts
            // what `-g` would show breaks the summary's promise. Their pending state does NOT
            // ride `updates_pending`: no manifest row means `topos update -g` has nothing to act
            // on there, and the count's command must stay true.
            let machine_ghosts = store_ghosts(ctx, machine, &cache, &all, signed_in);
            data.machine_summary = Some(ListScopeSummary {
                skills: machine.skills() + machine_ghosts.len() as u64,
                updates_pending: machine.updates_pending(),
                command: "topos list -g".to_owned(),
            });
        }
        if let Some(roots) = &discover {
            let found = discover_untracked(ctx, roots)?;
            if !found.is_empty() {
                let folders: HashSet<&str> = found
                    .iter()
                    .filter_map(|u| Path::new(&u.path).parent().and_then(Path::to_str))
                    .collect();
                data.untracked_summary = Some(UntrackedSummary {
                    skills: found.len() as u64,
                    folders: folders.len() as u64,
                    command: "topos list --untracked".to_owned(),
                });
            }
        }
    }

    data.truncated = truncated;
    Ok(ListOutcome {
        data,
        warnings,
        untracked_view: req.untracked,
    })
}

/// One resolved row as the inventory prints it. A row never applied here carries the all-zero
/// baseline as its identity (the system's own "never applied" sentinel — the honest reading of a
/// required field with nothing to put in it); an `"off"` switch reads detached/excluded-here.
fn skill_entry(row: &Row) -> SkillEntry {
    let (status, cause) = match row.state {
        StatusItemState::Applied => (Some(SkillStatus::Current), None),
        StatusItemState::Behind => (Some(SkillStatus::Behind), None),
        StatusItemState::LocalEdits => (Some(SkillStatus::Draft), None),
        StatusItemState::Off => (Some(SkillStatus::Detached), Some(DetachCause::ExcludedHere)),
        StatusItemState::NotAvailable => {
            (Some(SkillStatus::Detached), Some(DetachCause::SignedOut))
        }
        // Never applied / pending / no delivery yet: no update status is honestly claimable.
        _ => (None, None),
    };
    SkillEntry {
        skill: row.name.clone(),
        workspace_id: row.workspace_id.clone(),
        version_id: row.version.clone().unwrap_or_else(|| ZERO_HEX.to_owned()),
        bundle_digest: row.digest.clone().unwrap_or_else(|| ZERO_HEX.to_owned()),
        draft: matches!(row.state, StatusItemState::LocalEdits),
        pending_proposals: Vec::new(),
        source: (!row.source.is_empty()).then(|| row.source.clone()),
        status,
        cause,
        kind: row.kind.clone(),
    }
}

/// The ALL-OR-NONE filter gate: every `--skill` selector must match at least one shown row's name
/// (else the typed no-such-skill), every `--channel` selector at least one row's cached delivery
/// channel (else the uniform not-found). Ghost rows count — a detached copy is still findable.
fn validate_filters(
    sections: &[&ScopeResolution],
    ghosts: &[Vec<Ghost>],
    skills: &[String],
    channels: &[String],
) -> Result<(), ClientError> {
    for s in skills {
        let hit = sections
            .iter()
            .any(|sec| sec.rows.iter().any(|r| r.name == *s))
            || ghosts.iter().flatten().any(|g| g.entry.skill == *s);
        if !hit {
            return Err(ClientError::NoSuchSkill { name: s.clone() });
        }
    }
    for c in channels {
        let hit = sections.iter().any(|sec| {
            sec.rows
                .iter()
                .any(|r| r.via_channels.iter().any(|v| v == c))
        }) || ghosts
            .iter()
            .flatten()
            .any(|g| g.via_channels.iter().any(|v| v == c));
        if !hit {
            return Err(crate::resolve::not_found(c));
        }
    }
    Ok(())
}

/// One store record no resolved row claims, with the cached channels the `--channel` filter
/// matches against and the placement dirs its store map records (what the deep dive answers with —
/// read from the same map the draft scan already opened, so it costs nothing extra).
struct Ghost {
    entry: SkillEntry,
    via_channels: Vec<String>,
    placements: Vec<String>,
}

/// Which scope sections the DEEP DIVE (`list <name>`) resolves against, in precedence order: the
/// machine scope alone under `-g` (the flag means the same thing here as on the listing — from
/// inside a project it must not answer with the project copy), project-then-machine otherwise.
/// The here-view deliberately keeps BOTH: a bare `list <name>` is a lookup, not a section, and a
/// machine-wide skill is a true answer to "where does this come from".
fn dive_sections(resolved: &Resolved, view: ScopeView) -> Vec<&ScopeResolution> {
    match view {
        ScopeView::Machine => vec![resolved.machine()],
        ScopeView::Here | ScopeView::All => resolved.scopes.iter().collect(),
    }
}

/// The deep answer for a name only a GHOST record carries (the built-in, a detached/frozen copy) —
/// the store record IS the answer: no manifest row names it, so `source_file`/`source_key`/`feed`
/// and the attribution are honestly absent, and the version, placements and state come from the
/// scope store the ghost was read out of. `ghosts` is per `sections` entry in precedence order, so
/// the first hit follows the same order the resolved rows do — and the section it came from is the
/// answer's scope. `None` when no shown section holds such a record (the caller keeps its uniform
/// not-found).
fn ghost_detail(
    sections: &[&ScopeResolution],
    ghosts: &[Vec<Ghost>],
    token: &str,
) -> Option<topos_types::results::ListDetail> {
    let (scope, ghost) = sections.iter().zip(ghosts).find_map(|(section, gs)| {
        gs.iter()
            .find(|g| g.entry.skill == token)
            .map(|g| (section.scope, g))
    })?;
    let version = ghost.entry.version_id.clone();
    Some(topos_types::results::ListDetail {
        name: ghost.entry.skill.clone(),
        scope: Some(scope.to_owned()),
        source_file: None,
        source_key: None,
        feed: None,
        attribution: None,
        version: (!version.bytes().all(|b| b == b'0')).then_some(version),
        pin: None,
        placements: ghost.placements.clone(),
        state: ghost_state(&ghost.entry),
        kind: ghost.entry.kind.clone(),
        harnesses: Vec::new(),
    })
}

/// A ghost's row status as the deep dive's state vocabulary: a copy delivery no longer claims is
/// `detached` (the new state — the bytes stay, the delivery ended); everything else keeps what the
/// row column says, so the built-in reads `applied` (or `local-edits` under a hand edit) exactly
/// as its row does. A purely local record claims no status at all, and reads by its draft alone.
fn ghost_state(entry: &SkillEntry) -> StatusItemState {
    match entry.status {
        Some(SkillStatus::Detached) => StatusItemState::Detached,
        Some(SkillStatus::Draft) => StatusItemState::LocalEdits,
        Some(SkillStatus::Behind) => StatusItemState::Behind,
        Some(SkillStatus::Current) => StatusItemState::Applied,
        None if entry.draft => StatusItemState::LocalEdits,
        None => StatusItemState::Applied,
    }
}

/// The scope's unclaimed STORE records — the built-in `topos` meta-skill and detached/frozen
/// copies (an unfollowed skill's retained bytes, a removed row's kept custody). The inventory
/// shows what is INSTALLED, and these are installed: the manifest resolution just does not claim
/// them, and hiding them would make `remove topos`'s own target invisible. Read from THIS scope's
/// own store only (never cross-scope), with the classic column semantics: `built-in` for the
/// meta-skill; otherwise DETACHED (no row demands an unclaimed record, so the bytes are a kept
/// leftover), with the cause from the delivery cache (withdrawn upstream / signed out / the row
/// left this scope's list), the source from the recorded origin or the workspace label, and the
/// draft flag from the lock + placements.
fn store_ghosts(
    ctx: &Ctx<'_>,
    section: &ScopeResolution,
    cache: &SyncStatus,
    all: &Sessions,
    signed_in: bool,
) -> Vec<Ghost> {
    let Some(layout) = &section.store else {
        return Vec::new();
    };
    let claimed: HashSet<&str> = section.rows.iter().map(|r| r.name.as_str()).collect();
    let Ok(entries) = ctx.fs.read_dir(&layout.skills_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<Ghost> = Vec::new();
    for dir in entries {
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(sid) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let sp = layout.published(&sid);
        let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
            continue;
        };
        if claimed.contains(lock.name.as_str()) {
            continue;
        }
        let (draft, placements) = ghost_scan(ctx, &sp.map, &lock);
        // The cached delivery for this id, across workspaces — withdrawn entries included (the
        // withdrawal IS the cause).
        let delivered: Option<(&String, &DeliveredSkill)> = cache
            .workspaces
            .iter()
            .find_map(|(ws, e)| e.delivered.get(id).map(|d| (ws, d)));
        let via_channels = delivered
            .map(|(_, d)| d.via_channels.clone())
            .unwrap_or_default();
        let entry = if super::builtin::is_builtin(id) {
            // The built-in: shipped by the CLI, force-synced to the binary. A hand edit shows
            // `draft` honestly until the next sweep overwrites it (snapshot-first).
            SkillEntry {
                skill: lock.name.clone(),
                workspace_id: None,
                version_id: lock.base_commit.clone(),
                bundle_digest: lock.bundle_digest.clone(),
                draft,
                pending_proposals: Vec::new(),
                source: Some("built-in".to_owned()),
                status: Some(if draft {
                    SkillStatus::Draft
                } else {
                    SkillStatus::Current
                }),
                cause: None,
                kind: None,
            }
        } else {
            let origin = doc::read_doc::<super::add::OriginDoc>(ctx.fs, &sp.origin)
                .ok()
                .flatten()
                .and_then(|o| origin_host(&o.origin.source));
            // An unclaimed record is never live: no row demands it, so the bytes are a kept
            // leftover whatever their drift, and the cause names which act ended the demand —
            // withdrawn upstream, signed out, or (otherwise) the row left this scope's list.
            let (source, cause) = if delivered.is_none() && origin.is_none() {
                // A purely local record: its absent workspace already says "local".
                (None, DetachCause::Unfollowed)
            } else {
                let label = delivered.and_then(|(ws, _)| {
                    all.sessions
                        .iter()
                        .find(|s| s.workspace_id == *ws)
                        .map(|s| s.display_name.clone())
                });
                let source = origin.or(label).unwrap_or_else(|| "local".to_owned());
                let cause = match delivered {
                    Some((_, d)) if d.withdrawn => DetachCause::RemovedUpstream,
                    Some(_) if !signed_in => DetachCause::SignedOut,
                    _ => DetachCause::Unfollowed,
                };
                (Some(source), cause)
            };
            let (status, cause) = (Some(SkillStatus::Detached), Some(cause));
            SkillEntry {
                skill: lock.name.clone(),
                workspace_id: delivered.map(|(ws, _)| ws.clone()),
                version_id: lock.base_commit.clone(),
                bundle_digest: lock.bundle_digest.clone(),
                draft,
                pending_proposals: Vec::new(),
                source,
                status,
                cause,
                kind: delivered.and_then(|(_, d)| d.kind.clone()),
            }
        };
        out.push(Ghost {
            entry,
            via_channels,
            placements,
        });
    }
    // Deterministic order: name (ids are opaque; names are the scope identity).
    out.sort_by(|a, b| a.entry.skill.cmp(&b.entry.skill));
    out
}

/// One ghost's `(draft, placements)` from its store map: a draft iff ANY placement holds bytes
/// hashing to a different digest than the lock pins, and the recorded placement dirs (what the
/// deep dive prints — the same set a resolved row reports). Both come off ONE map read. A
/// missing/unscannable source is no-draft (nothing to compare) and no placements, never an error.
fn ghost_scan(ctx: &Ctx<'_>, map_path: &Path, lock: &Lock) -> (bool, Vec<String>) {
    let Ok(Some(map)) = doc::read_map(ctx.fs, map_path) else {
        return (false, Vec::new());
    };
    let mut draft = false;
    for placement in &map.placements {
        let source = Path::new(placement);
        if !source.exists() {
            continue;
        }
        if let Ok(scanned) = crate::scan::scan(source)
            && topos_core::digest::to_hex(&scanned.bundle_digest) != lock.bundle_digest
        {
            draft = true;
            break;
        }
    }
    (draft, map.placements)
}

/// The host of a recorded import source (`github.com/owner/repo` → `github.com`), or `None` for
/// an empty source.
fn origin_host(source: &str) -> Option<String> {
    source
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
}

/// The `--remote` view: one channel-index + catalog read per live session (narrowed by the
/// `--workspace` filter), each skill annotated with this machine's adoption state from the SAME
/// resolution the local sections print. A per-workspace transport fault DEGRADES to a warning —
/// the successfully-read workspaces still land.
fn remote_view(
    resolved: &Resolved,
    all: &crate::sessions::Sessions,
    only: Option<&str>,
    connect: SessionDirectory<'_>,
    warnings: &mut Vec<String>,
) -> Result<Vec<RemoteWorkspace>, ClientError> {
    let live: Vec<&Session> = all.live().collect();
    if live.is_empty() {
        return Err(ClientError::SessionRequired {
            address: "<workspace-address>".to_owned(),
            message: "not connected to a workspace — run `topos login <workspace-address>` first"
                .into(),
        });
    }
    let mut out = Vec::new();
    for s in live {
        if only.is_some_and(|w| w != s.workspace_id) {
            continue;
        }
        let dir = connect(s);
        let channels = match dir.channels_index(&s.workspace_id) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(remote_skip_line(s, &e));
                continue;
            }
        };
        let skills = match dir.skills_index(&s.workspace_id) {
            Ok(k) => k,
            Err(e) => {
                warnings.push(remote_skip_line(s, &e));
                continue;
            }
        };
        out.push(RemoteWorkspace {
            host: s.host.clone(),
            workspace: s.workspace_name.clone(),
            workspace_id: s.workspace_id.clone(),
            channels: channels
                .channels
                .iter()
                .map(|c| RemoteChannel {
                    name: c.name.clone(),
                    skills: c.skills.len() as u64,
                    adopted_in: channel_adopted_in(resolved, &s.host, &s.workspace_name, &c.name),
                })
                .collect(),
            skills: skills
                .skills
                .iter()
                .map(|e| RemoteSkill {
                    name: e.name.clone(),
                    kind: e.kind.clone(),
                    version_id: e.version_id.clone(),
                    open_proposals: e.open_proposals,
                    state: adoption(resolved, &s.host, &s.workspace_name, &e.name, &e.version_id),
                })
                .collect(),
        });
    }
    // Deterministic order however the sessions file lists them.
    out.sort_by(|a, b| {
        a.host
            .cmp(&b.host)
            .then_with(|| a.workspace.cmp(&b.workspace))
    });
    Ok(out)
}

/// A short, leak-free line for one skipped `--remote` workspace.
fn remote_skip_line(s: &Session, e: &ClientError) -> String {
    let label = match e {
        ClientError::TargetNotFound { .. } => "not visible to you",
        _ => "the server did not answer",
    };
    format!(
        "could not read the catalog for workspace {} ({label}) — skipped",
        s.workspace_name
    )
}

/// The manifest file whose channel row adopts `<host>/<ws>/channels/<name>` here, when one does.
fn channel_adopted_in(
    resolved: &Resolved,
    host: &str,
    workspace: &str,
    channel: &str,
) -> Option<String> {
    let canonical = format!("{host}/{workspace}/channels/{channel}");
    resolved
        .scopes
        .iter()
        .find(|s| s.sets.contains(&canonical))
        .and_then(|s| s.manifest.clone())
}

/// One catalog skill's adoption marker: adopted by the here-scope, only by the machine (seen from
/// inside a project), not at all — or adopted with the catalog `current` past the UNPINNED
/// applied version (a pinned row sits where it was asked to sit, never "update available").
fn adoption(
    resolved: &Resolved,
    host: &str,
    workspace: &str,
    name: &str,
    catalog_version: &str,
) -> RemoteAdoption {
    let reference = format!("{host}/{workspace}/{name}");
    fn find<'a>(s: &'a ScopeResolution, reference: &str) -> Option<&'a Row> {
        s.rows.iter().find(|r| r.bundle && r.reference == reference)
    }
    let here = &resolved.scopes[0];
    let (row, adopted) = if let Some(r) = find(here, &reference) {
        (r, RemoteAdoption::AdoptedHere)
    } else if here.scope != "machine"
        && let Some(r) = find(resolved.machine(), &reference)
    {
        (r, RemoteAdoption::AdoptedOnMachine)
    } else {
        return RemoteAdoption::NotAdopted;
    };
    match &row.version {
        Some(v) if row.pin.is_none() && v != catalog_version => RemoteAdoption::UpdateAvailable,
        _ => adopted,
    }
}

/// The agent-eye view for one harness: each skills dir it reads from this folder — its canonical
/// home dir, the shared `.agents/skills` dirs when the harness is covered by them, and its
/// project dir — with every entry marked managed (by which manifest row or feed, or `built-in`
/// for the CLI's own placed meta-skill) or untracked.
fn agent_view(
    ctx: &Ctx<'_>,
    resolved: &Resolved,
    roots: Option<&DiscoveryRoots>,
    slug: &str,
) -> Result<AgentView, ClientError> {
    let known = registry::known_harnesses();
    let Some(harness) = known.iter().find(|h| h.slug == slug) else {
        let slugs: Vec<&str> = known.iter().map(|h| h.slug).collect();
        return Err(ClientError::InvalidArgument(format!(
            "'{slug}' is not a known agent — known agents: {}",
            slugs.join(", ")
        )));
    };

    // The dirs, home scope first, deduped (a harness whose native dir IS the shared dir).
    let mut dirs: Vec<(PathBuf, &'static str)> = Vec::new();
    if let Some(r) = roots {
        let cwd = r.cwd.as_deref();
        if let Some(d) = registry::skills_root(slug, SkillScope::User, &r.home, cwd) {
            dirs.push((d, "user"));
        }
        let covered = coverage::shared_dir_support(slug).covered();
        if covered {
            dirs.push((coverage::shared_skills_dir(&r.home), "user"));
        }
        if let Some(d) = registry::skills_root(slug, SkillScope::Project, &r.home, cwd) {
            dirs.push((d, "project"));
        }
        if covered && let Some(c) = cwd {
            dirs.push((c.join(".agents/skills"), "project"));
        }
    }
    let mut seen: HashSet<PathBuf> = HashSet::new();
    dirs.retain(|(d, _)| seen.insert(d.clone()));

    // Every managed placement across BOTH scopes, canonicalized → its managing label.
    let mut managed: Vec<(PathBuf, String)> = Vec::new();
    for scope in &resolved.scopes {
        for row in &scope.rows {
            let label = match (&row.source_file, &row.source_key, &row.feed) {
                (Some(file), Some(key), _) => format!("{file}:{key}"),
                (_, _, Some(feed)) => format!("feed {feed}"),
                _ => continue,
            };
            for p in &row.placements {
                if let Ok(canon) = Path::new(p).canonicalize() {
                    managed.push((canon, label.clone()));
                }
            }
        }
    }
    // The placed BUILT-IN meta-skill is topos-managed with no manifest row (force-synced
    // custody), so its recorded dirs are seeded by hand — labeled the way the inventory's ghost
    // row names it. An unreadable record marks nothing (the view stays a best-effort read).
    for dir in super::builtin::placement_dirs(ctx).unwrap_or_default() {
        if let Ok(canon) = Path::new(&dir).canonicalize() {
            managed.push((canon, "built-in".to_owned()));
        }
    }

    let mut out = Vec::new();
    for (dir, scope) in dirs {
        let mut entries: Vec<AgentViewEntry> = Vec::new();
        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.') || !path.is_dir() {
                    continue;
                }
                if !path.join("SKILL.md").is_file() {
                    continue;
                }
                let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                entries.push(AgentViewEntry {
                    name: name.to_owned(),
                    managed: managed
                        .iter()
                        .find(|(p, _)| *p == canon)
                        .map(|(_, label)| label.clone()),
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        out.push(AgentViewDir {
            path: dir.to_string_lossy().into_owned(),
            scope: scope.to_owned(),
            entries,
        });
    }
    Ok(AgentView {
        agent: harness.slug.to_owned(),
        agent_name: harness.display_name.to_owned(),
        dirs: out,
    })
}

/// Discover skills sitting in a known harness's skill dir (across the baked registry) that no
/// tracked skill already records — the `add`-able inventory. Dedups a physically-shared dir (e.g.
/// `.agents/skills`) to one row by canonical path. Real-fs (like the adapters' own `discover`),
/// so a per-dir scan failure is silently skipped, never an error. `pub(crate)` so `add <skill>`
/// name resolution shares the SAME discovered inventory `list` prints (one source of truth for
/// what a name can resolve to).
pub(crate) fn discover_untracked(
    ctx: &Ctx<'_>,
    roots: &DiscoveryRoots,
) -> Result<Vec<UntrackedEntry>, ClientError> {
    let tracked = tracked_placement_paths(ctx)?;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<UntrackedEntry> = Vec::new();
    for d in registry::discover_all(&roots.home, roots.cwd.as_deref()) {
        let canon = d.path.canonicalize().unwrap_or_else(|_| d.path.clone());
        if tracked.contains(&canon) {
            continue; // already adopted or delivered — not "untracked"
        }
        if !seen.insert(canon) {
            continue; // one physical dir once (a dir shared across harnesses, e.g. .agents/skills)
        }
        let name = d
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| d.path.to_string_lossy().into_owned());
        out.push(UntrackedEntry {
            name,
            path: d.path.to_string_lossy().into_owned(),
            harness: d.harness_slug,
            harness_name: d.harness_name,
            adapter_supported: d.adapter_supported,
            scope: match d.scope {
                SkillScope::User => "user",
                SkillScope::Project => "project",
            }
            .to_owned(),
        });
    }
    // Deterministic order: name, then path.
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// Every tracked skill's placement paths in the MACHINE store, canonicalized (a placement that no
/// longer resolves on disk is dropped — it can't shadow a real discovery). The same dedup key
/// `add`'s `reject_already_tracked` uses.
fn tracked_placement_paths(ctx: &Ctx<'_>) -> Result<Vec<PathBuf>, ClientError> {
    let mut paths = Vec::new();
    for entry in ctx.fs.read_dir(&ctx.layout.skills_dir())? {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || !entry.is_dir() {
            continue;
        }
        let Ok(id) = crate::id::SkillId::parse(id) else {
            continue;
        };
        let Some(map): Option<PlacementMap> =
            doc::read_map(ctx.fs, &ctx.layout.published(&id).map)?
        else {
            continue;
        };
        for p in &map.placements {
            if let Ok(canon) = Path::new(p).canonicalize() {
                paths.push(canon);
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use topos_types::requests::{
        WireChannelEntry, WireChannelIndex, WireMe, WireProposalIndex, WireReach, WireSkillIndex,
        WireSkillIndexEntry, WireSkillLog,
    };

    use super::*;
    use crate::ops::inventory::testkit::{TempHome, assigned, skill_id_of, with_ctx};

    fn request() -> ListRequest {
        ListRequest::default()
    }

    fn run(home: &TempHome, cwd: &Path, req: &ListRequest) -> Result<ListOutcome, ClientError> {
        run_discovering(home, cwd, req, None)
    }

    fn run_discovering(
        home: &TempHome,
        cwd: &Path,
        req: &ListRequest,
        discover: Option<DiscoveryRoots>,
    ) -> Result<ListOutcome, ClientError> {
        with_ctx(home, Some(cwd), |ctx| {
            list_with(ctx, req, discover, None, super::super::RowPage::unlimited())
        })
    }

    fn scope<'a>(out: &'a ListOutcome, name: &str) -> &'a ListScope {
        out.data
            .scopes
            .iter()
            .find(|s| s.scope == name)
            .unwrap_or_else(|| panic!("no {name} scope in {:?}", out.data.scopes))
    }

    /// A project layout: a checkout with its own `topos.toml` demanding one workspace bundle and
    /// one adopted-in-place local folder recorded in the PROJECT's own store.
    fn lay_project(home: &TempHome) -> PathBuf {
        let repo = home.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(crate::manifest::MANIFEST_FILE),
            "[bundles]\n\
             \"topos.sh/acme/deploy\" = \"*\"\n\
             \"./tools/repo-helper\" = \"*\"\n",
        )
        .unwrap();
        let tool = repo.join("tools/repo-helper");
        std::fs::create_dir_all(&tool).unwrap();
        std::fs::write(tool.join("SKILL.md"), b"# helper\n").unwrap();
        // The PROJECT store holds the adopted skill — the custody the machine store never sees.
        let layout = crate::sidecar::project_store_layout(&repo);
        std::fs::create_dir_all(layout.home()).unwrap();
        let id = skill_id_of("repo-helper");
        let sid = crate::id::SkillId::parse(&id).unwrap();
        std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();
        let sp = layout.published(&sid);
        crate::doc::write_doc(
            &crate::fs_seam::RealFs,
            &sp.sync,
            &topos_types::persisted::SyncState {
                schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
                observed: 1,
                observed_version_id: "b".repeat(64),
                applied: 1,
                base_commit: "b".repeat(64),
                work_hash: "e".repeat(64),
                held: false,
                draft_observed: None,
            },
        )
        .unwrap();
        crate::doc::write_doc(
            &crate::fs_seam::RealFs,
            &sp.lock,
            &topos_types::persisted::Lock {
                schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
                skill_id: id,
                name: "repo-helper".to_owned(),
                base_commit: "b".repeat(64),
                bundle_digest: "f".repeat(64),
                files: Vec::new(),
            },
        )
        .unwrap();
        repo
    }

    /// The default view in a project: the PROJECT rows in full — including the skill adopted
    /// into the PROJECT's own store (invisible to a home-store-only read) — plus the machine
    /// summary with its counts and exact command, and the signed-in flag the remote pointer
    /// renders from.
    #[test]
    fn default_in_a_project_shows_project_rows_and_summarizes_the_machine() {
        let home = TempHome::new();
        let repo = lay_project(&home);
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
            vec![assigned("notes", None), assigned("triage", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");
        // One machine row BEHIND: applied at an older version than served.
        home.store_applied(&skill_id_of("notes"), "notes", &"c".repeat(64), &[]);

        let out = run(&home, &repo, &request()).unwrap();
        assert_eq!(out.data.scopes.len(), 1, "{:?}", out.data.scopes);
        let project = scope(&out, "project");
        assert!(
            project
                .manifest
                .as_deref()
                .is_some_and(|m| m.ends_with("topos.toml"))
        );
        let names: Vec<&str> = project.rows.iter().map(|r| r.skill.as_str()).collect();
        assert!(names.contains(&"deploy"), "{names:?}");
        // The PROJECT-STORE adopted skill shows with its stored identity — the custody leg made
        // these invisible to the old home-store-only list.
        let helper = project
            .rows
            .iter()
            .find(|r| r.skill == "repo-helper")
            .expect("the project-store skill rides the project section");
        assert_eq!(helper.version_id, "b".repeat(64));
        assert_eq!(helper.status, Some(SkillStatus::Current));

        // The machine summary: 2 skills, 1 update pending, the exact expanding command.
        let summary = out.data.machine_summary.expect("a machine summary");
        assert_eq!(summary.skills, 2);
        assert_eq!(summary.updates_pending, 1);
        assert_eq!(summary.command, "topos list -g");
        assert!(out.data.signed_in, "the remote pointer renders from this");
    }

    /// Outside a project: machine rows in full, NO machine summary (machine IS the shown scope).
    #[test]
    fn outside_a_project_the_machine_rows_show_in_full() {
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
            vec![assigned("notes", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");

        let out = run(&home, &cwd, &request()).unwrap();
        assert_eq!(out.data.scopes.len(), 1);
        let machine = scope(&out, "machine");
        assert!(
            machine
                .manifest
                .as_deref()
                .is_some_and(|m| m.ends_with("topos.toml")),
            "the feed row's file governs"
        );
        assert_eq!(machine.rows.len(), 1);
        assert_eq!(machine.rows[0].skill, "notes");
        // Never applied: the all-zero baseline, no false version claim, no status claim.
        assert_eq!(machine.rows[0].version_id, super::ZERO_HEX);
        assert_eq!(machine.rows[0].status, None);
        assert!(out.data.machine_summary.is_none());
    }

    /// `-g` inside a project: machine rows only — scope-blind like `update -g`.
    #[test]
    fn dash_g_shows_machine_rows_even_inside_a_project() {
        let home = TempHome::new();
        let repo = lay_project(&home);
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
            vec![assigned("notes", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");

        let out = run(
            &home,
            &repo,
            &ListRequest {
                view: ScopeView::Machine,
                ..request()
            },
        )
        .unwrap();
        assert_eq!(out.data.scopes.len(), 1);
        let machine = scope(&out, "machine");
        assert!(machine.rows.iter().any(|r| r.skill == "notes"));
        assert!(
            !machine.rows.iter().any(|r| r.skill == "deploy"),
            "no project rows under -g"
        );
        assert!(out.data.machine_summary.is_none());
    }

    /// `--all`: both scope sections in full.
    #[test]
    fn all_shows_both_scope_sections() {
        let home = TempHome::new();
        let repo = lay_project(&home);
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
            vec![assigned("notes", None)],
            Vec::new(),
        );

        let out = run(
            &home,
            &repo,
            &ListRequest {
                view: ScopeView::All,
                ..request()
            },
        )
        .unwrap();
        let names: Vec<&str> = out.data.scopes.iter().map(|s| s.scope.as_str()).collect();
        assert_eq!(names, vec!["project", "machine"]);
        assert!(out.data.machine_summary.is_none());
    }

    /// `--untracked`: the full discovery listing PLUS every tracked covering scope (the TTY
    /// renders each as one summary line, so nothing is invisible).
    #[test]
    fn untracked_lists_discoveries_and_keeps_the_tracked_scopes_visible() {
        let home = TempHome::new();
        let repo = lay_project(&home);
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        // A discoverable untracked skill in a harness dir under an ISOLATED probe home.
        let probe_home = TempHome::new();
        let skill_dir = probe_home.0.join(".cursor/skills/improbable-zebra");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            b"---\nname: improbable-zebra\n---\n# z\n",
        )
        .unwrap();
        let discover = DiscoveryRoots {
            home: probe_home.0.clone(),
            cwd: None,
        };

        let out = run_discovering(
            &home,
            &repo,
            &ListRequest {
                untracked: true,
                ..request()
            },
            Some(discover),
        )
        .unwrap();
        assert!(
            out.data
                .untracked
                .iter()
                .any(|u| u.name == "improbable-zebra" && u.harness == "cursor"),
            "{:?}",
            out.data.untracked
        );
        let names: Vec<&str> = out.data.scopes.iter().map(|s| s.scope.as_str()).collect();
        assert_eq!(
            names,
            vec!["project", "machine"],
            "tracked scopes ride along"
        );
        assert!(out.data.untracked_summary.is_none(), "the listing IS shown");
    }

    /// The untracked SUMMARY on a bare list: counts + the exact expanding command; absent when
    /// nothing untracked exists (nothing is being withheld).
    #[test]
    fn the_untracked_summary_counts_skills_and_folders() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let probe_home = TempHome::new();
        for name in ["improbable-zebra", "unlikely-yak"] {
            let dir = probe_home.0.join(".cursor/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), b"# s\n").unwrap();
        }
        let discover = DiscoveryRoots {
            home: probe_home.0.clone(),
            cwd: None,
        };

        let out = run_discovering(&home, &cwd, &request(), Some(discover)).unwrap();
        let summary = out.data.untracked_summary.expect("a summary");
        assert_eq!(summary.skills, 2);
        assert_eq!(summary.folders, 1, "both live in one folder");
        assert_eq!(summary.command, "topos list --untracked");
        assert!(
            out.data.untracked.is_empty(),
            "the full listing waits for the flag"
        );

        // Nothing discovered → no summary at all.
        let empty_home = TempHome::new();
        let out = run_discovering(
            &home,
            &cwd,
            &request(),
            Some(DiscoveryRoots {
                home: empty_home.0.clone(),
                cwd: None,
            }),
        )
        .unwrap();
        assert!(out.data.untracked_summary.is_none());
    }

    /// `list <name>`: the deep dive — file + row-key for a manifest-delivered skill, the feed
    /// spelling for a feed-delivered one, and the uniform not-found on a miss.
    #[test]
    fn the_deep_dive_names_the_file_or_the_feed() {
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
        // deploy applied with a placement — the detail names where the bytes are.
        let placed = home.0.join("placed-deploy");
        home.store_applied(
            &skill_id_of("deploy"),
            "deploy",
            &"d".repeat(64),
            &[placed.to_string_lossy().as_ref()],
        );

        let deep = |name: &str| {
            run(
                &home,
                &cwd,
                &ListRequest {
                    name: Some(name.to_owned()),
                    ..request()
                },
            )
        };
        let detail = deep("deploy").unwrap().data.detail.expect("a detail");
        assert!(
            detail
                .source_file
                .as_deref()
                .is_some_and(|f| f.ends_with("topos.toml"))
        );
        assert_eq!(detail.source_key.as_deref(), Some("topos.sh/acme/deploy"));
        assert_eq!(detail.feed, None);
        assert_eq!(
            detail.placements,
            vec![placed.to_string_lossy().into_owned()]
        );
        assert!(matches!(detail.state, StatusItemState::Applied));

        let detail = deep("notes").unwrap().data.detail.expect("a detail");
        assert_eq!(detail.source_file, None);
        assert_eq!(detail.feed.as_deref(), Some("topos.sh/acme"));

        let err = deep("nowhere").unwrap_err();
        assert!(matches!(err, ClientError::TargetNotFound { .. }), "{err:?}");
    }

    /// `list -a <slug>`: the agent-eye view — the harness's dirs across home and project, each
    /// entry marked managed (`<file>:<row-key>`) or untracked; an unknown slug refuses typed,
    /// naming the known slugs.
    #[test]
    fn the_agent_view_marks_managed_and_untracked_entries() {
        let home = TempHome::new();
        let cwd = home.0.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            cwd.join(crate::manifest::MANIFEST_FILE),
            "[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n",
        )
        .unwrap();
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
        // The managed placement: the machine store records deploy placed into the probe home's
        // claude-code skills dir.
        let probe_home = TempHome::new();
        let managed_dir = probe_home.0.join(".claude/skills/deploy");
        std::fs::create_dir_all(&managed_dir).unwrap();
        std::fs::write(managed_dir.join("SKILL.md"), b"# deploy\n").unwrap();
        // An untracked neighbour in the same dir.
        let stray = probe_home.0.join(".claude/skills/stray-helper");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("SKILL.md"), b"# stray\n").unwrap();
        home.global("[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n");
        home.store_applied(
            &skill_id_of("deploy"),
            "deploy",
            &"d".repeat(64),
            &[managed_dir.to_string_lossy().as_ref()],
        );
        // The placed BUILT-IN meta-skill: a store record with no manifest row anywhere — the view
        // must mark it managed (`built-in`), never untracked.
        let builtin_dir = probe_home.0.join(".claude/skills/topos");
        std::fs::create_dir_all(&builtin_dir).unwrap();
        std::fs::write(builtin_dir.join("SKILL.md"), b"# topos\n").unwrap();
        home.store_applied(
            "topos",
            "topos",
            &"a".repeat(64),
            &[builtin_dir.to_string_lossy().as_ref()],
        );

        let out = run_discovering(
            &home,
            &cwd,
            &ListRequest {
                agent: Some("claude-code".to_owned()),
                ..request()
            },
            Some(DiscoveryRoots {
                home: probe_home.0.clone(),
                cwd: Some(cwd.clone()),
            }),
        )
        .unwrap();
        let view = out.data.agent_view.expect("an agent view");
        assert_eq!(view.agent, "claude-code");
        assert_eq!(view.agent_name, "Claude Code");
        let user_dir = view
            .dirs
            .iter()
            .find(|d| d.scope == "user" && d.path.ends_with(".claude/skills"))
            .expect("the claude-code user dir");
        let entry = |n: &str| {
            user_dir
                .entries
                .iter()
                .find(|e| e.name == n)
                .unwrap_or_else(|| panic!("no entry {n} in {:?}", user_dir.entries))
        };
        let managed = entry("deploy").managed.as_deref().expect("managed");
        assert!(
            managed.ends_with("topos.toml:topos.sh/acme/deploy"),
            "{managed}"
        );
        assert_eq!(entry("stray-helper").managed, None);
        // The built-in carries its own honest marker — the inventory's ghost-row label.
        assert_eq!(entry("topos").managed.as_deref(), Some("built-in"));
        // The view spans both scopes: a project dir rides along.
        assert!(
            view.dirs.iter().any(|d| d.scope == "project"),
            "{:?}",
            view.dirs
        );

        // The unknown slug refusal names the known ones.
        let err = run_discovering(
            &home,
            &cwd,
            &ListRequest {
                agent: Some("not-a-harness".to_owned()),
                ..request()
            },
            None,
        )
        .unwrap_err();
        match err {
            ClientError::InvalidArgument(msg) => {
                assert!(msg.contains("claude-code"), "{msg}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// Store records no resolved row claims stay IN the inventory: the built-in meta-skill
    /// (source `built-in`) and detached/frozen copies ride their scope's section with the classic
    /// column semantics — installed is installed, and `remove topos`'s own target must be
    /// findable. The deep dive stays resolution-only (the old `status <bundle>` missed
    /// store-only records the same way).
    #[test]
    fn unclaimed_store_records_ride_their_scope_as_ghost_rows() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        // A withdrawn delivery whose bytes are retained: the cache says withdrawn, so the feed
        // itemizes nothing and no row claims the record.
        let ghost_id = skill_id_of("ghosty");
        home.store_applied(&ghost_id, "ghosty", &"c".repeat(64), &[]);
        let mut withdrawn = assigned("ghosty", None).1;
        withdrawn.withdrawn = true;
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![(ghost_id, withdrawn)],
            Vec::new(),
        );
        // The built-in meta-skill's record (force-synced custody, never a manifest row).
        home.store_applied("topos", "topos", &"a".repeat(64), &[]);
        // A purely local frozen copy (row removed, bytes kept): no cache entry, no origin.
        home.store_applied(&skill_id_of("frozen"), "frozen", &"b".repeat(64), &[]);

        let out = run(&home, &cwd, &request()).unwrap();
        let machine = scope(&out, "machine");
        let row = |n: &str| {
            machine
                .rows
                .iter()
                .find(|r| r.skill == n)
                .unwrap_or_else(|| panic!("no {n} in {:?}", machine.rows))
        };
        assert_eq!(row("topos").source.as_deref(), Some("built-in"));
        assert_eq!(row("topos").status, Some(SkillStatus::Current));
        assert_eq!(row("ghosty").status, Some(SkillStatus::Detached));
        assert_eq!(row("ghosty").cause, Some(DetachCause::RemovedUpstream));
        assert_eq!(row("ghosty").workspace_id.as_deref(), Some("w_acme"));
        // The frozen local copy is a leftover like any other ghost: detached, its row gone — its
        // absent workspace already says "local".
        assert_eq!(row("frozen").status, Some(SkillStatus::Detached));
        assert_eq!(row("frozen").cause, Some(DetachCause::Unfollowed));
        assert_eq!(row("frozen").version_id, "b".repeat(64));

        // A name only a ghost carries still answers — the deep dive falls back to the same store
        // records the listing prints, so `list <name>` never denies a row `list` just showed.
        let miss = run(
            &home,
            &cwd,
            &ListRequest {
                name: Some("nowhere".to_owned()),
                ..request()
            },
        )
        .unwrap_err();
        assert!(
            matches!(miss, ClientError::TargetNotFound { .. }),
            "{miss:?}"
        );
    }

    /// A REMOVED row's retained record (the delivery cache still lists it, nothing withdrew it)
    /// reads detached with the removed-from-the-list cause — never like a live row. This is the
    /// remove-then-clean leftover: the bytes deliberately stay, and the row must say so.
    #[test]
    fn a_removed_rows_retained_record_reads_detached_not_current() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        // The machine manifest exists but no longer carries the row (the remove dropped it); the
        // cache still lists the delivery un-withdrawn, and the store record stands.
        home.global("[bundles]\n");
        let id = skill_id_of("lingery");
        home.store_applied(&id, "lingery", &"c".repeat(64), &[]);
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("lingery", None)],
            Vec::new(),
        );

        let out = run(&home, &cwd, &request()).unwrap();
        let machine = scope(&out, "machine");
        let row = machine
            .rows
            .iter()
            .find(|r| r.skill == "lingery")
            .unwrap_or_else(|| panic!("no lingery in {:?}", machine.rows));
        assert_eq!(row.status, Some(SkillStatus::Detached));
        assert_eq!(row.cause, Some(DetachCause::Unfollowed));
        // The source is the session's DISPLAY name (the testkit keeps it unequal to the slug).
        assert_eq!(row.source.as_deref(), Some("ACME"));
    }

    /// `list <name>` answers for a GHOST too: the built-in (no manifest row anywhere) reports its
    /// placements and reads `applied`, and a detached copy reads the `detached` state. Before
    /// this, both answered NOT_FOUND while a plain `list` printed them — a deep dive that denies a
    /// row the listing shows is simply wrong.
    #[test]
    fn the_deep_dive_answers_from_the_ghost_records_too() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        // The built-in, placed: a clean placement (its recorded sha never matches real bytes, so
        // an EXISTING dir would scan as a draft — leave it absent for the clean reading).
        let placed = home.0.join("placed-topos");
        home.store_applied(
            "topos",
            "topos",
            &"a".repeat(64),
            &[placed.to_string_lossy().as_ref()],
        );
        // A withdrawn delivery whose bytes are retained — the detached ghost.
        let ghost_id = skill_id_of("ghosty");
        home.store_applied(&ghost_id, "ghosty", &"c".repeat(64), &[]);
        let mut withdrawn = assigned("ghosty", None).1;
        withdrawn.withdrawn = true;
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![(ghost_id, withdrawn)],
            Vec::new(),
        );

        let deep = |name: &str| {
            run(
                &home,
                &cwd,
                &ListRequest {
                    name: Some(name.to_owned()),
                    ..request()
                },
            )
        };

        let detail = deep("topos").unwrap().data.detail.expect("a detail");
        assert_eq!(detail.name, "topos");
        assert_eq!(
            detail.scope.as_deref(),
            Some("machine"),
            "a ghost answer carries the scope of the store it was read out of"
        );
        assert_eq!(detail.version.as_deref(), Some(&*"a".repeat(64)));
        assert_eq!(
            detail.placements,
            vec![placed.to_string_lossy().into_owned()],
            "the ghost answers with the placements its store map records"
        );
        // No row names it, so no file, no key, no feed — the record IS the answer.
        assert_eq!(detail.source_file, None);
        assert_eq!(detail.source_key, None);
        assert_eq!(detail.feed, None);
        assert!(
            matches!(detail.state, StatusItemState::Applied),
            "{detail:?}"
        );

        let detail = deep("ghosty").unwrap().data.detail.expect("a detail");
        assert!(
            matches!(detail.state, StatusItemState::Detached),
            "{detail:?}"
        );
    }

    /// `list <name> -g` inside a project answers from the MACHINE scope alone — the flag means the
    /// same thing on the deep dive as on the listing. A name only the project delivers is a miss
    /// under `-g` (it would otherwise silently answer with the project copy).
    #[test]
    fn the_deep_dive_honors_the_scope_view() {
        let home = TempHome::new();
        let repo = lay_project(&home);
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
            vec![assigned("notes", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");

        let deep = |name: &str, view: ScopeView| {
            run(
                &home,
                &repo,
                &ListRequest {
                    name: Some(name.to_owned()),
                    view,
                    ..request()
                },
            )
        };

        // The default view still reads project-then-machine: both names answer, each carrying
        // the scope it was answered from (the spelling every suggested command rides on).
        let detail = deep("repo-helper", ScopeView::Here)
            .unwrap()
            .data
            .detail
            .expect("a detail");
        assert_eq!(detail.scope.as_deref(), Some("project"));
        let detail = deep("notes", ScopeView::Here)
            .unwrap()
            .data
            .detail
            .expect("a detail");
        assert_eq!(detail.scope.as_deref(), Some("machine"));

        // `-g`: the machine's own row answers, the project-only one does not.
        let detail = deep("notes", ScopeView::Machine)
            .unwrap()
            .data
            .detail
            .expect("a detail");
        assert_eq!(detail.name, "notes");
        assert_eq!(detail.scope.as_deref(), Some("machine"));
        let miss = deep("repo-helper", ScopeView::Machine).unwrap_err();
        assert!(
            matches!(miss, ClientError::TargetNotFound { .. }),
            "the machine holds nothing of that name — {miss:?}"
        );
    }

    /// The machine summary's `skills` count includes the ghost rows — a summary that undercounts
    /// what `-g` would show breaks the summary's promise. Their pending state deliberately stays
    /// out of `updates pending`: no manifest row means `topos update -g` has nothing to act on.
    #[test]
    fn the_machine_summary_counts_ghost_rows() {
        let home = TempHome::new();
        let repo = lay_project(&home);
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
            vec![assigned("notes", None)],
            Vec::new(),
        );
        // One CLAIMED machine record (the feed delivers notes) + the built-in ghost.
        home.store_applied(&skill_id_of("notes"), "notes", &"d".repeat(64), &[]);
        home.store_applied("topos", "topos", &"a".repeat(64), &[]);

        let out = run(&home, &repo, &request()).unwrap();
        let summary = out.data.machine_summary.expect("a machine summary");
        assert_eq!(summary.skills, 2, "the delivered row + the built-in ghost");
        assert_eq!(
            summary.updates_pending, 0,
            "a ghost never claims `topos update -g`"
        );

        // `-g` shows exactly what the summary counted.
        let out = run(
            &home,
            &repo,
            &ListRequest {
                view: ScopeView::Machine,
                ..request()
            },
        )
        .unwrap();
        assert_eq!(scope(&out, "machine").rows.len(), 2);
    }

    /// A fake per-session directory: canned channel + skill indexes, or a fault. `Clone` so the
    /// connector closure can mint an owned (`'static`) copy per session.
    #[derive(Clone)]
    struct FakeDirectory {
        channels: HashMap<String, WireChannelIndex>,
        skills: HashMap<String, WireSkillIndex>,
        fail: bool,
    }
    impl DirectorySource for FakeDirectory {
        fn me(&self, _w: &str) -> Result<WireMe, ClientError> {
            unreachable!("list --remote reads channels + skills only")
        }
        fn channels_index(&self, w: &str) -> Result<WireChannelIndex, ClientError> {
            if self.fail {
                return Err(ClientError::TargetNotFound { target: w.into() });
            }
            Ok(self.channels.get(w).cloned().unwrap_or(WireChannelIndex {
                channels: Vec::new(),
            }))
        }
        fn skills_index(&self, w: &str) -> Result<WireSkillIndex, ClientError> {
            Ok(self
                .skills
                .get(w)
                .cloned()
                .unwrap_or(WireSkillIndex { skills: Vec::new() }))
        }
        fn proposals_index(&self, _w: &str) -> Result<WireProposalIndex, ClientError> {
            unreachable!()
        }
        fn skill_log(&self, _w: &str, _s: &str) -> Result<WireSkillLog, ClientError> {
            unreachable!()
        }
        fn reach(&self, _w: &str, _s: &str) -> Result<WireReach, ClientError> {
            unreachable!()
        }
        fn channel_place(&self, _w: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn channel_unplace(&self, _w: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn protect_skill(&self, _w: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn protect_channel(&self, _w: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn ack_notices(&self, _w: &str, _ids: &[String]) -> Result<(), ClientError> {
            unreachable!()
        }
    }

    fn catalog_entry(name: &str, version: &str) -> WireSkillIndexEntry {
        WireSkillIndexEntry {
            skill_id: format!("s_{name}"),
            name: name.to_owned(),
            kind: "skill".to_owned(),
            status: "active".to_owned(),
            version_id: version.to_owned(),
            bundle_digest: "e".repeat(64),
            generation: 1,
            display_name: None,
            updated_at: 1,
            open_proposals: 2,
            upstream_host: None,
            upstream_repo: None,
            upstream_path: None,
            mcp_server_name: None,
        }
    }

    /// `--remote` with no live session refuses typed; with sessions it reads each workspace's
    /// channels (adopted_in naming the adopting manifest) and skills (the four adoption markers).
    #[test]
    fn remote_reads_channels_and_skills_with_adoption_markers() {
        let home = TempHome::new();
        let repo = lay_project(&home);

        // No session → the typed refusal, before anything could dial.
        let refused = with_ctx(&home, Some(&repo), |ctx| {
            list_with(
                ctx,
                &ListRequest {
                    remote: true,
                    ..request()
                },
                None,
                Some(&|_s: &Session| -> Box<dyn DirectorySource> {
                    unreachable!("no session → no dial")
                }),
                super::super::RowPage::unlimited(),
            )
        })
        .unwrap_err();
        assert_eq!(refused.code(), "SESSION_REQUIRED");

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
            vec![assigned("deploy", None), assigned("notes", None)],
            Vec::new(),
        );
        // machine adopts `notes` explicitly and is BEHIND the catalog current; the project (the
        // here-scope) adopts `deploy` (never applied → adopted-here, no update claim).
        home.global(
            "[bundles]\n\
             \"topos.sh/acme/notes\" = \"*\"\n\
             \"topos.sh/acme/channels/backend\" = \"*\"\n",
        );
        home.store_applied(&skill_id_of("notes"), "notes", &"c".repeat(64), &[]);

        let mut channels = HashMap::new();
        channels.insert(
            "w_acme".to_owned(),
            WireChannelIndex {
                channels: vec![
                    WireChannelEntry {
                        name: "backend".to_owned(),
                        mode: "open".to_owned(),
                        builtin: false,
                        included: true,
                        skills: vec![],
                    },
                    WireChannelEntry {
                        name: "everyone".to_owned(),
                        mode: "open".to_owned(),
                        builtin: true,
                        included: true,
                        skills: vec![],
                    },
                ],
            },
        );
        let mut skills = HashMap::new();
        skills.insert(
            "w_acme".to_owned(),
            WireSkillIndex {
                skills: vec![
                    catalog_entry("deploy", &"d".repeat(64)),
                    catalog_entry("notes", &"d".repeat(64)),
                    catalog_entry("triage", &"d".repeat(64)),
                ],
            },
        );
        let fake = FakeDirectory {
            channels,
            skills,
            fail: false,
        };

        let out = with_ctx(&home, Some(&repo), |ctx| {
            list_with(
                ctx,
                &ListRequest {
                    remote: true,
                    ..request()
                },
                None,
                Some(&|_s: &Session| -> Box<dyn DirectorySource> { Box::new(fake.clone()) }),
                super::super::RowPage::unlimited(),
            )
        })
        .unwrap();
        assert_eq!(out.data.remote.len(), 1);
        let ws = &out.data.remote[0];
        assert_eq!(
            (ws.host.as_str(), ws.workspace.as_str()),
            ("topos.sh", "acme")
        );
        let backend = ws.channels.iter().find(|c| c.name == "backend").unwrap();
        assert!(
            backend
                .adopted_in
                .as_deref()
                .is_some_and(|f| f.ends_with("topos.toml")),
            "{:?}",
            backend.adopted_in
        );
        let everyone = ws.channels.iter().find(|c| c.name == "everyone").unwrap();
        assert_eq!(everyone.adopted_in, None);
        let state = |n: &str| ws.skills.iter().find(|s| s.name == n).unwrap().state;
        // `deploy` is adopted by the PROJECT manifest — the here-scope.
        assert_eq!(state("deploy"), RemoteAdoption::AdoptedHere);
        // `notes` is adopted machine-side only, and its applied version is behind the catalog.
        assert_eq!(state("notes"), RemoteAdoption::UpdateAvailable);
        assert_eq!(state("triage"), RemoteAdoption::NotAdopted);
        assert_eq!(
            ws.skills
                .iter()
                .find(|s| s.name == "deploy")
                .unwrap()
                .open_proposals,
            2
        );
    }

    /// A machine-only adoption seen from inside a project reads adopted-on-machine (when not
    /// behind), and a per-workspace fault degrades to a warning, never failing the list.
    #[test]
    fn remote_marks_machine_adoption_and_degrades_per_workspace_faults() {
        let home = TempHome::new();
        let repo = lay_project(&home);
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
            vec![assigned("notes", None)],
            Vec::new(),
        );
        home.global("[bundles]\n\"topos.sh/acme/notes\" = \"*\"\n");
        // Applied AT the catalog current — adopted-on-machine, not update-available.
        home.store_applied(&skill_id_of("notes"), "notes", &"d".repeat(64), &[]);

        let mut skills = HashMap::new();
        skills.insert(
            "w_acme".to_owned(),
            WireSkillIndex {
                skills: vec![catalog_entry("notes", &"d".repeat(64))],
            },
        );
        let fake = FakeDirectory {
            channels: HashMap::new(),
            skills,
            fail: false,
        };
        let out = with_ctx(&home, Some(&repo), |ctx| {
            list_with(
                ctx,
                &ListRequest {
                    remote: true,
                    ..request()
                },
                None,
                Some(&|_s: &Session| -> Box<dyn DirectorySource> { Box::new(fake.clone()) }),
                super::super::RowPage::unlimited(),
            )
        })
        .unwrap();
        assert_eq!(
            out.data.remote[0].skills[0].state,
            RemoteAdoption::AdoptedOnMachine
        );

        // The degrade: a faulting workspace is skipped with a stable warning line.
        let failing = FakeDirectory {
            channels: HashMap::new(),
            skills: HashMap::new(),
            fail: true,
        };
        let out = with_ctx(&home, Some(&repo), |ctx| {
            list_with(
                ctx,
                &ListRequest {
                    remote: true,
                    ..request()
                },
                None,
                Some(&|_s: &Session| -> Box<dyn DirectorySource> { Box::new(failing.clone()) }),
                super::super::RowPage::unlimited(),
            )
        })
        .unwrap();
        assert!(out.data.remote.is_empty());
        assert_eq!(out.warnings.len(), 1);
        assert!(
            out.warnings[0].contains("workspace acme") && out.warnings[0].contains("skipped"),
            "{}",
            out.warnings[0]
        );
    }

    /// The page reaches the rows a nested view actually holds. `--remote` used to page the
    /// per-workspace WRAPPERS — one row per workspace, so a `--limit` over a single workspace
    /// capped nothing and the nested catalog came back whole. The cap now lands on each
    /// workspace's skills, with ONE summed marker; the channels (a handful, and the index the
    /// skills are read WITH) stay whole.
    #[test]
    fn remote_pages_catalog_rows_not_workspace_wrappers() {
        let home = TempHome::new();
        let repo = lay_project(&home);
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.cache("w_acme", "topos.sh", "acme", Vec::new(), Vec::new());
        let mut channels = HashMap::new();
        channels.insert(
            "w_acme".to_owned(),
            WireChannelIndex {
                channels: vec![
                    WireChannelEntry {
                        name: "backend".to_owned(),
                        mode: "open".to_owned(),
                        builtin: false,
                        included: true,
                        skills: vec![],
                    },
                    WireChannelEntry {
                        name: "everyone".to_owned(),
                        mode: "open".to_owned(),
                        builtin: true,
                        included: true,
                        skills: vec![],
                    },
                ],
            },
        );
        let mut skills = HashMap::new();
        skills.insert(
            "w_acme".to_owned(),
            WireSkillIndex {
                skills: vec![
                    catalog_entry("deploy", &"d".repeat(64)),
                    catalog_entry("notes", &"d".repeat(64)),
                    catalog_entry("triage", &"d".repeat(64)),
                ],
            },
        );
        let fake = FakeDirectory {
            channels,
            skills,
            fail: false,
        };
        let out = with_ctx(&home, Some(&repo), |ctx| {
            list_with(
                ctx,
                &ListRequest {
                    remote: true,
                    ..request()
                },
                None,
                Some(&|_s: &Session| -> Box<dyn DirectorySource> { Box::new(fake.clone()) }),
                super::super::RowPage {
                    offset: 0,
                    limit: Some(2),
                },
            )
        })
        .unwrap();
        let ws = &out.data.remote[0];
        assert_eq!(ws.skills.len(), 2, "the catalog rows are capped");
        assert_eq!(ws.channels.len(), 2, "the channels ride whole");
        assert_eq!(out.data.truncated.len(), 1);
        let t = &out.data.truncated[0];
        assert_eq!((t.bucket.as_str(), t.shown, t.total), ("remote", 2, 3));
    }

    /// The agent view is paged at each DIR's entries, with one summed `agent` marker — the same
    /// promise every other bucket makes. (Its row SELECTORS are refused at the argv boundary; see
    /// the cli suite.)
    #[test]
    fn the_agent_view_pages_dir_entries() {
        let home = TempHome::new();
        let cwd = home.0.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
        // Three folders in the agent's own skills dir — more than the page allows.
        let probe_home = TempHome::new();
        for name in ["alpha", "beta", "gamma"] {
            let dir = probe_home.0.join(".claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), b"# s\n").unwrap();
        }
        let out = with_ctx(&home, Some(&cwd), |ctx| {
            list_with(
                ctx,
                &ListRequest {
                    agent: Some("claude-code".to_owned()),
                    ..request()
                },
                Some(DiscoveryRoots {
                    home: probe_home.0.clone(),
                    cwd: Some(cwd.clone()),
                }),
                None,
                super::super::RowPage {
                    offset: 0,
                    limit: Some(2),
                },
            )
        })
        .unwrap();
        let view = out.data.agent_view.expect("an agent view");
        let user_dir = view
            .dirs
            .iter()
            .find(|d| d.scope == "user" && d.path.ends_with(".claude/skills"))
            .expect("the claude-code user dir");
        assert_eq!(user_dir.entries.len(), 2, "the dir's entries are capped");
        assert_eq!(out.data.truncated.len(), 1);
        let t = &out.data.truncated[0];
        assert_eq!((t.bucket.as_str(), t.shown, t.total), ("agent", 2, 3));
    }

    /// A FOCUSED view still honors `--footprint` on the TTY. Both focused arms used to return
    /// before the shared tail, so `list <name> --footprint` printed no footprint at all while the
    /// `--json` payload carried it — a flag whose answer depended on which view asked.
    #[test]
    fn the_focused_tty_views_still_print_the_footprint() {
        use topos_types::results::{AgentView, AgentViewDir, ListDetail};

        let footprint = Some(vec!["/home/me/.topos/state".to_owned()]);
        let dive = ListOutcome {
            data: ListData {
                detail: Some(ListDetail {
                    name: "deploy".to_owned(),
                    scope: None,
                    source_file: None,
                    source_key: None,
                    feed: None,
                    attribution: None,
                    version: None,
                    pin: None,
                    placements: Vec::new(),
                    state: StatusItemState::Applied,
                    kind: None,
                    harnesses: Vec::new(),
                }),
                footprint: footprint.clone(),
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let rendered = crate::render::list_tty(&dive);
        assert!(rendered.contains("deploy"), "{rendered}");
        assert!(
            rendered.contains("Footprint: 1 paths") && rendered.contains("/home/me/.topos/state"),
            "the deep dive prints the footprint: {rendered}"
        );

        let agent = ListOutcome {
            data: ListData {
                agent_view: Some(AgentView {
                    agent: "claude-code".to_owned(),
                    agent_name: "Claude Code".to_owned(),
                    dirs: vec![AgentViewDir {
                        path: "/home/me/.claude/skills".to_owned(),
                        scope: "user".to_owned(),
                        entries: Vec::new(),
                    }],
                }),
                footprint,
                ..ListData::default()
            },
            warnings: Vec::new(),
            untracked_view: false,
        };
        let rendered = crate::render::list_tty(&agent);
        assert!(
            rendered.contains("Footprint: 1 paths") && rendered.contains("/home/me/.topos/state"),
            "the agent view prints the footprint: {rendered}"
        );
    }
}
