//! `list` — the INVENTORY, offline by default: what is installed where you stand, per scope. The
//! default body is the here-scope's rows in full (the nearest `topos.toml` covering the cwd, else
//! the machine); `-g` is the machine scope alone, `--all` both. Whatever the invocation does not
//! show is never invisible and never dumped — the machine scope and the untracked discoveries
//! each ride ONE summary line ending in the exact command that expands them, and a signed-in TTY
//! points at `list --remote` for what the workspaces offer.
//!
//! The optional views: `list <name>` is the one-skill deep dive (which file and line-key — or
//! which feed — delivers it) over the SAME scopes the invocation selects; a name no row manages
//! (the placed built-in aside) answers the NOT-MANAGED headline plus any unmanaged copies found
//! on disk — a success, never an error, because "nothing manages it" is the whole answer;
//! `-a <slug>` is the agent-eye view (each skills dir that harness
//! reads from this folder, entries marked managed or untracked — deliberately spanning both
//! scopes); `--untracked` is the full discovery listing; `--remote` (the one networked arm) reads
//! each live session's channel index + catalog and annotates every skill with this machine's
//! adoption state. `--footprint` reports every topos-owned path outside skill dirs.
//!
//! Rows come from the SAME per-scope resolution `status` reads ([`super::inventory`]): project
//! rows from the checkout's own store, machine rows from `~/.topos/` — one resolution, two views.
//! The offline states are cache facts ("as of last sync"), never live claims.
//!
//! ONE list: every bundle the invocation shows is a row under a scope section, and each row names
//! its own ORIGIN — the workspace address, the repository, the folder. A source is where a bundle
//! comes from, never a second kind of thing to enumerate below the list, so nothing an origin
//! covers gets a trailing block of its own here. What no row can say still does: an external
//! source that has stopped answering rides the tail, because a row keeps reading `current` while
//! its repository goes unreachable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use topos_harness::coverage;
use topos_harness::registry::{self, SkillScope};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{
    AgentView, AgentViewDir, AgentViewEntry, BucketTruncation, ListData, ListDetail, ListScope,
    ListScopeSummary, RemoteAdoption, RemoteChannel, RemoteSkill, RemoteWorkspace, SkillEntry,
    SkillStatus, StatusItemState, UntrackedEntry, UntrackedSummary,
};

use crate::ctx::Ctx;
use crate::doc;
use crate::error::ClientError;
use crate::manifest::keys::{self, KeyShape};
use crate::plane::DirectorySource;
use crate::sessions::Session;
use crate::sidecar;

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
/// [`ClientError::InvalidArgument`] for an unknown `-a` slug; [`ClientError::SessionRequired`]
/// for `--remote` with no live session; [`ClientError::NoSuchSkill`] / the uniform not-found for
/// a filter selector matching nothing; otherwise a read failure. A deep-dive name resolving
/// nowhere is a SUCCESS (the not-managed answer), never an error.
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
    // of the focused views, every one of which returns early. `--agent` and `--remote` answer
    // about machines that track external sources too; leaving the field empty there would make the
    // wire contract true only of the view that happens to fall through. The one-skill deep dive is
    // the exception, and narrows it below. What the TTY does with the field differs by view: the
    // dive prints its one source in full, every other view prints only the sources in trouble
    // (each row already names its own origin).
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
    // A name no resolved row claims has exactly two honest answers left: the placed BUILT-IN
    // (engine-managed with no manifest row — its record and placements are the answer), and the
    // NOT-MANAGED headline plus any unmanaged copies discovery finds on disk. A success either
    // way — "nothing manages it" is the whole answer, never an error.
    if let Some(name) = &req.name {
        let dive = dive_sections(&resolved, req.view);
        let detail = match inventory::detail_for(&dive, &all, name) {
            Ok(detail) => detail,
            Err(_) => builtin_detail(ctx, name)
                .unwrap_or_else(|| unmanaged_detail(ctx, name, discover.as_ref())),
        };
        // A one-skill answer names THIS skill's external source and NO other. Every line above it
        // is about the skill on screen, so an unrelated repository's last check reads as one more
        // fact about that skill — the listing's "what is my machine tracking?" block answering a
        // question nobody asked here. Recomputed over the DIVE's sections (which can exceed the
        // listing's — a bare dive inside a project answers from the machine scope too) and then
        // narrowed to the source the answering row itself names: the repository that delivers it,
        // or nothing at all.
        data.forge = inventory::forge_sources(ctx, &dive);
        let origin = detail_forge_origin(&detail);
        data.forge
            .retain(|f| origin.as_deref() == Some(f.source.as_str()));
        data.detail = Some(detail);
        return Ok(ListOutcome {
            data,
            warnings,
            untracked_view: req.untracked,
        });
    }

    // The placed BUILT-IN meta-skill's row — engine custody with no manifest row, re-sourced from
    // its own record + on-disk placements (it originates from disk, so the inventory shows it;
    // `remove topos`'s own target must be findable). Every OTHER unclaimed store record mints NO
    // line: records may describe rows, never create them — a record nothing demands resolves once
    // on the next `update` and its files become the person's own.
    let builtin = builtin_entry(ctx);

    // The `--channel`/`--skill` filters, ALL-OR-NONE across the SHOWN sections (the built-in row
    // counts as a matchable row by its name).
    let narrowed = !(req.channels.is_empty() && req.skills.is_empty());
    if narrowed {
        validate_filters(&sections, builtin.as_ref(), &req.skills, &req.channels)?;
    }
    let keeps = |name: &str, via: &[String]| -> bool {
        if !narrowed {
            return true;
        }
        req.skills.iter().any(|s| s == name)
            || req.channels.iter().any(|c| via.iter().any(|v| v == c))
    };
    for section in &sections {
        let mut rows: Vec<SkillEntry> = section
            .inventory_rows()
            .filter(|r| keeps(&r.name, &r.via_channels))
            .map(skill_entry)
            .map(|e| with_source_health(e, &data.forge))
            .collect();
        if section.scope == "machine"
            && let Some(b) = &builtin
            && keeps(&b.skill, &[])
        {
            rows.push(b.clone());
        }
        if page.is_active() {
            mark(section.scope, page.apply(&mut rows), &mut truncated);
        }
        data.scopes.push(ListScope {
            scope: section.scope.to_owned(),
            manifest: manifest_display(ctx, section),
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
            // The built-in's row counts into the summary's `skills` — a summary that undercounts
            // what `-g` would show breaks the summary's promise. Its pending state does NOT ride
            // `updates_pending`: no manifest row means `topos update -g` has nothing to act on
            // there, and the count's command must stay true. Nothing else off-row counts: an
            // unclaimed store record mints no line, so it inflates no summary either.
            data.machine_summary = Some(ListScopeSummary {
                skills: machine.skills() + u64::from(builtin.is_some()),
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
/// required field with nothing to put in it); an `"off"` switch reads `off` — the file's own
/// standing statement, never a delivery state.
fn skill_entry(row: &Row) -> SkillEntry {
    let status = match row.state {
        StatusItemState::Applied => Some(SkillStatus::Current),
        StatusItemState::Behind => Some(SkillStatus::Behind),
        StatusItemState::LocalEdits => Some(SkillStatus::Draft),
        StatusItemState::Off => Some(SkillStatus::Off),
        // Never applied / pending / not available / no delivery yet: no update status is honestly
        // claimable.
        _ => None,
    };
    SkillEntry {
        skill: row.name.clone(),
        workspace_id: row.workspace_id.clone(),
        version_id: row.version.clone().unwrap_or_else(|| ZERO_HEX.to_owned()),
        bundle_digest: row.digest.clone().unwrap_or_else(|| ZERO_HEX.to_owned()),
        draft: matches!(row.state, StatusItemState::LocalEdits),
        pending_proposals: Vec::new(),
        source: origin_of(&row.reference),
        status,
        kind: row.kind.clone(),
        source_health: None,
    }
}

/// Join the source check log onto ONE row: a row whose origin has stopped answering carries that
/// fact itself. The join key is the row's own origin — the same string the `from` column prints and
/// the same one the check log files under — so a repo-set's members and an explicit import of the
/// same repository all learn about it, which is exactly right: every one of them is frozen.
///
/// A source that is answering attaches NOTHING. Its identity is already on the row.
fn with_source_health(
    mut entry: SkillEntry,
    forge: &[topos_types::results::ForgeSource],
) -> SkillEntry {
    let Some(origin) = entry.source.as_deref() else {
        return entry;
    };
    if let Some(f) = forge
        .iter()
        .find(|f| f.source == origin && f.error.is_some())
    {
        entry.source_health = Some(topos_types::results::SourceHealth {
            answered_at: f.answered_at,
        });
    }
    entry
}

/// Where a row's bytes COME FROM, as the inventory's `from` column names it: the workspace address
/// for anything a workspace delivers (`topos.sh/acme` — an explicit bundle row, a channel's
/// members and a feed's all answer the one address a person would type), the repository for a
/// forge row (`github.com/owner/repo` — the same spelling the auto-update check log files it
/// under), the folder as the manifest spells it for a local one.
///
/// The manifest FILE is deliberately not this answer: the section header above the row already
/// names it, so repeating it per row spent the column on the one fact the reader already had. A
/// reference that does not classify answers `None` — a column with nothing true to say prints
/// nothing.
fn origin_of(reference: &str) -> Option<String> {
    match keys::classify_key(reference).ok()? {
        KeyShape::LocalPath { raw } => Some(raw),
        KeyShape::RepoSet { host, owner, repo }
        | KeyShape::RepoSkill {
            host, owner, repo, ..
        } => Some(format!("{host}/{owner}/{repo}")),
        KeyShape::Feed { host, workspace }
        | KeyShape::WorkspaceBundle {
            host, workspace, ..
        }
        | KeyShape::Channel {
            host, workspace, ..
        } => Some(format!("{host}/{workspace}")),
    }
}

/// The governing file as the LIST header names it: a PROJECT manifest relative to where you stand
/// (`./topos.toml` in this folder, `../topos.toml` a level up), the machine file `~`-abbreviated.
/// The absolute path is noise in a header read from inside the folder it governs; the relative
/// spelling is the one a person could paste. Falls back to the scope's own pretty spelling when no
/// relative form exists (no working directory, or a manifest that is not an ancestor of it).
fn manifest_display(ctx: &Ctx<'_>, section: &ScopeResolution) -> Option<String> {
    let pretty = section.manifest.clone();
    if section.scope != "project" {
        return pretty;
    }
    let file = section.manifest_path.as_deref()?;
    let cwd = ctx.roots.as_ref()?.cwd.as_deref()?;
    relative_manifest(cwd, file).or(pretty)
}

/// `<cwd>` + the manifest's own path → `./topos.toml` / `../topos.toml` / `../../topos.toml`. A
/// project manifest governs the cwd or an ancestor of it, so the walk is always upward; anything
/// else answers `None` rather than a path that would not resolve from here.
fn relative_manifest(cwd: &Path, file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    let up = cwd.strip_prefix(file.parent()?).ok()?.components().count();
    let prefix = if up == 0 {
        "./".to_owned()
    } else {
        "../".repeat(up)
    };
    Some(format!("{prefix}{name}"))
}

/// The ALL-OR-NONE filter gate: every `--skill` selector must match at least one shown row's name
/// (else the typed no-such-skill), every `--channel` selector at least one row's cached delivery
/// channel (else the uniform not-found). The built-in's row counts by its name.
fn validate_filters(
    sections: &[&ScopeResolution],
    builtin: Option<&SkillEntry>,
    skills: &[String],
    channels: &[String],
) -> Result<(), ClientError> {
    for s in skills {
        let hit = sections
            .iter()
            .any(|sec| sec.rows.iter().any(|r| r.name == *s))
            || builtin.is_some_and(|b| b.skill == *s);
        if !hit {
            return Err(ClientError::NoSuchSkill { name: s.clone() });
        }
    }
    for c in channels {
        let hit = sections.iter().any(|sec| {
            sec.rows
                .iter()
                .any(|r| r.via_channels.iter().any(|v| v == c))
        });
        if !hit {
            return Err(crate::resolve::not_found(c));
        }
    }
    Ok(())
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

/// The external repository a deep dive's answer comes from, as the check log spells it
/// (`<host>/<owner>/<repo>`) — `None` when no row answered, or the row that did is not a forge
/// row. The key the answering row is spelled with IS the reference, so classifying it is the same
/// derivation the check log's own key gets: a repo-SET row and every member it delivers carry the
/// set's key, a single import carries its own, and both resolve to the one repository.
fn detail_forge_origin(detail: &ListDetail) -> Option<String> {
    match keys::classify_key(detail.source_key.as_deref()?).ok()? {
        KeyShape::RepoSet { host, owner, repo }
        | KeyShape::RepoSkill {
            host, owner, repo, ..
        } => Some(format!("{host}/{owner}/{repo}")),
        _ => None,
    }
}

/// The placed BUILT-IN meta-skill's inventory row — read from its own record in the MACHINE store
/// (engine custody: `ops::builtin` places and force-syncs it with no manifest row anywhere). It
/// originates from disk, so the inventory shows it — hiding it would make `remove topos`'s own
/// target invisible. `None` when the machine holds no built-in record (opted out, or never
/// placed). A hand edit shows `draft` honestly until the next sweep overwrites it
/// (snapshot-first).
fn builtin_entry(ctx: &Ctx<'_>) -> Option<SkillEntry> {
    let (lock, draft, _) = builtin_record(ctx)?;
    Some(SkillEntry {
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
        kind: None,
        source_health: None,
    })
}

/// The deep answer for the placed built-in (`topos list topos`): the record IS the answer — no
/// manifest row names it, so `source_file`/`source_key`/`feed` and the attribution are honestly
/// absent, and the version, placements and state come from its machine-store record.
fn builtin_detail(ctx: &Ctx<'_>, token: &str) -> Option<ListDetail> {
    if token != super::builtin::BUILTIN_NAME {
        return None;
    }
    let (lock, draft, placements) = builtin_record(ctx)?;
    let version = lock.base_commit.clone();
    Some(ListDetail {
        name: lock.name,
        scope: Some("machine".to_owned()),
        source_file: None,
        source_key: None,
        feed: None,
        attribution: None,
        version: (!version.bytes().all(|b| b == b'0')).then_some(version),
        pin: None,
        placements,
        state: if draft {
            StatusItemState::LocalEdits
        } else {
            StatusItemState::Applied
        },
        kind: None,
        harnesses: Vec::new(),
        managed: true,
        folders: Vec::new(),
    })
}

/// The built-in's `(lock, draft, placements)` off its machine-store record: a draft iff ANY
/// recorded placement holds bytes hashing to a different digest than the lock pins. A missing
/// record answers `None`; an unreadable map is no-draft, no placements — never an error.
fn builtin_record(ctx: &Ctx<'_>) -> Option<(Lock, bool, Vec<String>)> {
    let sid = crate::id::SkillId::parse(super::builtin::BUILTIN_NAME).ok()?;
    let sp = ctx.layout.published(&sid);
    let lock = doc::read_doc::<Lock>(ctx.fs, &sp.lock).ok().flatten()?;
    let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) else {
        return Some((lock, false, Vec::new()));
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
    Some((lock, draft, map.placements))
}

/// The deep answer for a name NOTHING manages — no row in any visible scope, not the built-in:
/// the not-managed headline, plus the folders of unmanaged copies discovery finds on disk
/// (matched by name). A SUCCESS, never an error — "nothing manages it" is the whole answer, and
/// `topos add <folder>` is the way to change it.
fn unmanaged_detail(ctx: &Ctx<'_>, token: &str, roots: Option<&DiscoveryRoots>) -> ListDetail {
    let folders = roots
        .and_then(|r| discover_untracked(ctx, r).ok())
        .map(|found| {
            found
                .into_iter()
                .filter(|u| u.name == token)
                .map(|u| u.path)
                .collect()
        })
        .unwrap_or_default();
    ListDetail {
        name: token.to_owned(),
        scope: None,
        source_file: None,
        source_key: None,
        feed: None,
        attribution: None,
        version: None,
        pin: None,
        placements: Vec::new(),
        state: StatusItemState::Unknown,
        kind: None,
        harnesses: Vec::new(),
        managed: false,
        folders,
    }
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
    // custody), so its recorded dirs are seeded by hand — carrying the same `built-in` label the
    // inventory row does. An unreadable record marks nothing (the view stays a best-effort read).
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
    // Deterministic order: FOLDER first, then name — the TTY groups the listing by folder, so
    // each folder's entries must arrive contiguous (name-first interleaved the folders and
    // reprinted a folder header per row).
    out.sort_by(|a, b| {
        let folder = |p: &str| {
            Path::new(p)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        };
        folder(&a.path)
            .cmp(&folder(&b.path))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}

/// Every tracked skill's placement paths in the MACHINE store, canonicalized (a placement that no
/// longer resolves on disk is dropped — it can't shadow a real discovery). The same dedup key
/// `add`'s `reject_already_tracked` uses. A RETIRED record's paths are NOT tracked: its kept
/// copies are the person's own now, so discovery must surface them as adoptable again.
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
        if sidecar::record_retired(ctx.fs, &ctx.layout, &id) {
            continue;
        }
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

    /// Every reference shape answers ONE origin — the address a person would type to get those
    /// bytes — and a set answers the same origin as the members it delivers, so a repo row and
    /// its skills read as one source rather than two.
    #[test]
    fn every_reference_shape_names_where_its_bytes_come_from() {
        let origin = |r: &str| origin_of(r).expect("a classifiable reference");
        assert_eq!(origin("topos.sh/acme/deploy"), "topos.sh/acme");
        assert_eq!(origin("topos.sh/acme"), "topos.sh/acme");
        assert_eq!(origin("topos.sh/acme/channels/frontend"), "topos.sh/acme");
        assert_eq!(origin("github.com/o/r"), "github.com/o/r");
        assert_eq!(origin("github.com/o/r/alpha"), "github.com/o/r");
        assert_eq!(origin("./tools/repo-helper"), "./tools/repo-helper");
        assert_eq!(origin("~/skills/notes"), "~/skills/notes");
        // A reference that classifies to nothing says nothing — never a guessed column.
        assert_eq!(origin_of("gitlab.com/o/r"), None);
        assert_eq!(origin_of(""), None);
    }

    /// The project manifest is spelled from where you stand: in the folder, a level up, two up.
    /// A file that is not an ancestor of the cwd answers `None` rather than a path that would not
    /// resolve from here.
    #[test]
    fn the_project_manifest_is_spelled_relative_to_where_you_stand() {
        let rel = |cwd: &str, file: &str| relative_manifest(Path::new(cwd), Path::new(file));
        assert_eq!(
            rel("/repo", "/repo/topos.toml").as_deref(),
            Some("./topos.toml")
        );
        assert_eq!(
            rel("/repo/api", "/repo/topos.toml").as_deref(),
            Some("../topos.toml")
        );
        assert_eq!(
            rel("/repo/api/src", "/repo/topos.toml").as_deref(),
            Some("../../topos.toml")
        );
        assert_eq!(rel("/elsewhere", "/repo/topos.toml"), None);
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
        // The governing file, spelled from where you stand — the absolute path is noise in a
        // header read from inside the folder it governs.
        assert_eq!(project.manifest.as_deref(), Some("./topos.toml"));
        let names: Vec<&str> = project.rows.iter().map(|r| r.skill.as_str()).collect();
        assert!(names.contains(&"deploy"), "{names:?}");
        // Each row names its ORIGIN, never the file the header already named: the workspace
        // address for a delivered bundle, the folder for one adopted in place.
        let source = |n: &str| {
            project
                .rows
                .iter()
                .find(|r| r.skill == n)
                .and_then(|r| r.source.clone())
        };
        assert_eq!(source("deploy").as_deref(), Some("topos.sh/acme"));
        assert_eq!(
            source("repo-helper").as_deref(),
            Some("./tools/repo-helper")
        );
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
    /// spelling for a feed-delivered one, and the not-managed success on a miss.
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

        // A name nothing manages is a SUCCESS carrying the not-managed headline, not an error.
        let detail = deep("nowhere").unwrap().data.detail.expect("a detail");
        assert!(!detail.managed, "{detail:?}");
        assert!(detail.folders.is_empty(), "{detail:?}");
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
        // The built-in carries its own honest marker — the inventory's `built-in` label.
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

    /// A store record no row claims MINTS NO ROW: records may describe rows, never create
    /// them. The one exception is the placed BUILT-IN (engine custody, originates from disk) —
    /// it stays listed with its own source column, because `remove topos`'s own target must be
    /// findable. The leftovers (a withdrawn delivery's retained bytes, a removed row's kept
    /// custody) are simply absent: their one-time resolution belongs to `update`, not here.
    #[test]
    fn unclaimed_store_records_mint_no_rows_and_the_builtin_stays_listed() {
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
        // A purely local leftover (row removed, bytes kept): no cache entry, no origin.
        home.store_applied(&skill_id_of("frozen"), "frozen", &"b".repeat(64), &[]);

        let out = run(&home, &cwd, &request()).unwrap();
        let machine = scope(&out, "machine");
        let topos = machine
            .rows
            .iter()
            .find(|r| r.skill == "topos")
            .unwrap_or_else(|| {
                panic!("the built-in rides the machine section: {:?}", machine.rows)
            });
        assert_eq!(topos.source.as_deref(), Some("built-in"));
        assert_eq!(topos.status, Some(SkillStatus::Current));
        assert_eq!(topos.version_id, "a".repeat(64));
        // The unclaimed records mint NOTHING — no row, no rendered mention.
        for name in ["ghosty", "frozen"] {
            assert!(
                !machine.rows.iter().any(|r| r.skill == name),
                "{name} has no row and must not appear: {:?}",
                machine.rows
            );
        }
        let text = crate::render::list_tty(&out);
        assert!(
            !text.contains("ghosty") && !text.contains("frozen"),
            "{text}"
        );
        assert!(!text.contains("detached"), "{text}");
    }

    /// A REMOVED row's retained record (the delivery cache still lists it, nothing withdrew it)
    /// is NOT listed: no row claims it, so the inventory says nothing about it — the remove
    /// receipt already resolved it, and `update`'s one-time resolution owns whatever remains.
    #[test]
    fn a_removed_rows_retained_record_is_not_listed() {
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
        assert!(
            !machine.rows.iter().any(|r| r.skill == "lingery"),
            "no row demands it, so no line originates from it: {:?}",
            machine.rows
        );
    }

    /// `list <name>` answers for the placed BUILT-IN (its record + placements ARE the answer),
    /// and answers NOT-MANAGED for a leftover record no row claims — a success carrying the
    /// headline, never a store mention and never an error.
    #[test]
    fn the_deep_dive_answers_the_builtin_and_not_managed_for_leftovers() {
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
        // A withdrawn delivery whose bytes are retained — the unclaimed leftover.
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
            "the built-in's answer carries the machine scope its record lives in"
        );
        assert_eq!(detail.version.as_deref(), Some(&*"a".repeat(64)));
        assert_eq!(
            detail.placements,
            vec![placed.to_string_lossy().into_owned()],
            "the built-in answers with the placements its store map records"
        );
        // No row names it, so no file, no key, no feed — the record IS the answer.
        assert_eq!(detail.source_file, None);
        assert_eq!(detail.source_key, None);
        assert_eq!(detail.feed, None);
        assert!(detail.managed);
        assert!(
            matches!(detail.state, StatusItemState::Applied),
            "{detail:?}"
        );

        // The leftover record answers NOT-MANAGED: no store mention, no state claim, a success.
        let out = deep("ghosty").unwrap();
        let detail = out.data.detail.as_ref().expect("a detail");
        assert!(!detail.managed, "{detail:?}");
        assert!(detail.folders.is_empty(), "{detail:?}");
        assert_eq!(detail.version, None);
        assert_eq!(
            crate::render::list_tty(&out),
            "ghosty — not managed on this machine"
        );
    }

    /// The NOT-MANAGED answer lists the folders of unmanaged copies discovery finds (matched by
    /// name), one per line under the byte-exact headline — and nothing else.
    #[test]
    fn the_not_managed_answer_lists_unmanaged_folders() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let probe_home = TempHome::new();
        for dir in [".claude/skills/deploy", ".cursor/skills/deploy"] {
            let d = probe_home.0.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), b"# d\n").unwrap();
        }
        let out = run_discovering(
            &home,
            &cwd,
            &ListRequest {
                name: Some("deploy".to_owned()),
                ..request()
            },
            Some(DiscoveryRoots {
                home: probe_home.0.clone(),
                cwd: None,
            }),
        )
        .unwrap();
        let detail = out.data.detail.as_ref().expect("a detail");
        assert!(!detail.managed);
        let claude = probe_home.0.join(".claude/skills/deploy");
        let cursor = probe_home.0.join(".cursor/skills/deploy");
        assert_eq!(
            detail.folders,
            vec![
                claude.to_string_lossy().into_owned(),
                cursor.to_string_lossy().into_owned()
            ]
        );
        assert_eq!(
            crate::render::list_tty(&out),
            format!(
                "deploy — not managed on this machine\n  {}\n  {}\nto manage: topos add -g deploy \
                 --dest <dest>",
                claude.display(),
                cursor.display()
            )
        );
    }

    /// EVERY folder holding a copy is enumerated — no cap, no ellipsis, however many there are.
    /// The answer's whole job is "where is it?"; a shortened list would hide a copy the person
    /// then never learns about.
    #[test]
    fn the_not_managed_answer_enumerates_every_folder_holding_a_copy() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let probe_home = TempHome::new();
        // Seven harness skill dirs, each holding a copy of the same name.
        let dirs = [
            ".augment/skills",
            ".bob/skills",
            ".claude/skills",
            ".continue/skills",
            ".cursor/skills",
            ".factory/skills",
            ".gemini/skills",
        ];
        for dir in dirs {
            let d = probe_home.0.join(dir).join("deploy");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), b"# d\n").unwrap();
        }
        let out = run_discovering(
            &home,
            &cwd,
            &ListRequest {
                name: Some("deploy".to_owned()),
                ..request()
            },
            Some(DiscoveryRoots {
                home: probe_home.0.clone(),
                cwd: None,
            }),
        )
        .unwrap();
        let detail = out.data.detail.as_ref().expect("a detail");
        assert!(!detail.managed);
        // The established order — folder first, then name — over every discovered copy.
        let mut expected: Vec<String> = dirs
            .iter()
            .map(|d| {
                probe_home
                    .0
                    .join(d)
                    .join("deploy")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        expected.sort();
        assert_eq!(detail.folders, expected, "every copy, none dropped");
        assert!(detail.folders.len() >= 6, "{:?}", detail.folders);
        let text = crate::render::list_tty(&out);
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("deploy — not managed on this machine"),
            "{text}"
        );
        for folder in &expected {
            assert_eq!(lines.next(), Some(format!("  {folder}").as_str()), "{text}");
        }
        assert_eq!(
            lines.next(),
            Some("to manage: topos add -g deploy --dest <dest>"),
            "{text}"
        );
        assert_eq!(lines.next(), None, "{text}");
        assert!(!text.contains("..."), "no ellipsis, ever: {text}");
    }

    /// The `--untracked` listing sorts by (folder, name): each folder's entries arrive
    /// contiguous, so the TTY's per-folder grouping prints one header per folder.
    #[test]
    fn untracked_discoveries_sort_by_folder_then_name() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let probe_home = TempHome::new();
        for dir in [
            ".claude/skills/zeta",
            ".claude/skills/alpha",
            ".cursor/skills/beta",
        ] {
            let d = probe_home.0.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), b"# s\n").unwrap();
        }
        let out = run_discovering(
            &home,
            &cwd,
            &ListRequest {
                untracked: true,
                ..request()
            },
            Some(DiscoveryRoots {
                home: probe_home.0.clone(),
                cwd: None,
            }),
        )
        .unwrap();
        let names: Vec<&str> = out.data.untracked.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "zeta", "beta"],
            "folder first, then name: {:?}",
            out.data.untracked
        );
    }

    /// `list <name> -g` inside a project answers from the MACHINE scope alone — the flag means the
    /// same thing on the deep dive as on the listing. A name only the project delivers answers
    /// NOT-MANAGED under `-g` (it would otherwise silently answer with the project copy).
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
        let detail = deep("repo-helper", ScopeView::Machine)
            .unwrap()
            .data
            .detail
            .expect("a detail");
        assert!(
            !detail.managed,
            "the machine manages nothing of that name — {detail:?}"
        );
    }

    /// The machine summary counts ROWS plus the listed built-in — never an unclaimed store
    /// record (records may describe rows, not inflate summaries). The built-in's state stays out
    /// of `updates pending`: no manifest row means `topos update -g` has nothing to act on.
    #[test]
    fn the_machine_summary_counts_rows_and_the_builtin_only() {
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
        // One CLAIMED machine record (the feed delivers notes) + the built-in + an UNCLAIMED
        // leftover the count must NOT include.
        home.store_applied(&skill_id_of("notes"), "notes", &"d".repeat(64), &[]);
        home.store_applied("topos", "topos", &"a".repeat(64), &[]);
        home.store_applied(&skill_id_of("frozen"), "frozen", &"b".repeat(64), &[]);

        let out = run(&home, &repo, &request()).unwrap();
        let summary = out.data.machine_summary.expect("a machine summary");
        assert_eq!(
            summary.skills, 2,
            "the delivered row + the built-in; never the leftover"
        );
        assert_eq!(
            summary.updates_pending, 0,
            "the built-in never claims `topos update -g`"
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
                    managed: true,
                    folders: Vec::new(),
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
