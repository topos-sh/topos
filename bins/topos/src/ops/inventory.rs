//! The shared per-scope RESOLUTION machinery `list` and `status` both read — one resolution, two
//! views. Computed ENTIRELY from local state: the parsed manifests (`manifest::scopes`), the
//! sessions file, the offline delivery cache (`sync_status`), and the per-scope sidecar stores.
//! No network, no writes.
//!
//! SCOPES ARE UNBLENDED, so the resolution is SECTIONED, never merged: the NEAREST project
//! manifest (taken whole) when one covers the working directory, then the MACHINE scope — the
//! global manifest, a COMPLETE recipe: only its rows deliver, and a workspace's feed flows iff a
//! feed row says so; with no file nothing is demanded machine-wide. Within a scope the delivered
//! set is the union of the rows minus the `"off"` switches, deduped by bundle identity — an
//! explicit row beats any set's or any feed's delivery of the same bundle.
//!
//! Per line: the winning reference, ONE source (which manifest row — or which workspace's feed —
//! asked), the delivery attribution (`assigned by <name>` / `picked by you`), and an honest state
//! (applied-as-of-the-last-sync / local edits / behind / off / not available / never delivered /
//! unknown). What a row cannot carry rides each scope's `notes`: the loud "the global manifest
//! does not adopt these assignments" line, feeds and channel lines whose exchange has brought
//! nothing yet, rows that add nothing, `"off"` switches for bundles nobody assigns any more, set
//! collisions, declined-but-delivered bundles, and cross-scope version splits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use topos_types::persisted::{ConflictState, Lock, SyncState};
use topos_types::results::{ForgeSource, ListDetail, StatusItemState, StatusRegime};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::document::{EntryValue, ManifestScope};
use crate::manifest::keys::{self, KeyShape};
use crate::manifest::scopes::{self, ScopePlan};
use crate::sessions::Sessions;
use crate::sidecar::Layout;
use crate::sync_status::{DeliveredSkill, SyncStatus, WorkspaceSync};
use crate::{doc, placement, sessions, sync_status};

/// The all-zero sentinel a first-receive baseline carries — a delivered bundle whose sync doc
/// still holds it has never been applied here (the next `update` applies it).
pub(crate) const ZERO_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Which scope(s) a read verb shows in full — the same three-way split `update` acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ScopeView {
    /// Where you stand: the nearest project manifest when one covers the cwd, else the machine.
    #[default]
    Here,
    /// The machine scope alone (`-g`), even from inside a project.
    Machine,
    /// Both scopes in full (`--all`).
    All,
}

/// What a row's local edits amount to ON DISK — the draft classification's own verdict, kept so a
/// listing can NAME the folder a person would open instead of only flagging that edits exist. It is
/// the [`crate::placement::classify_draft`] answer verbatim (one edited copy is THE draft; copies
/// that explain none of the others are the freeze), carried rather than recomputed: the scan that
/// decides a row is edited at all is the one being classified.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum DraftCopies {
    /// No edited copy — or a read that could not classify one (an unreadable map, a scan that
    /// failed). A row with nothing provable to say about a folder says nothing.
    #[default]
    None,
    /// Exactly one advanced edited copy — THE draft — in this folder.
    In(PathBuf),
    /// Several copies hold edits that disagree (always ≥ 2). No single one is the draft, so a ROW
    /// names none of them — it reports the count and sends the reader to the deep dive, which
    /// answers with these: each copy in the two spellings every surface that names one uses (the
    /// folder as a person reads it, and the `--dest` value that names it back). They are spelled
    /// HERE, at the scope whose store was read, through the same
    /// [`crate::ops::dest_select::copy_spellings`] the placement freeze's refusal uses — one
    /// vocabulary for one state.
    Diverged(Vec<crate::ops::dest_select::CopySpelling>),
}

/// One resolved line plus the provenance the deep answer spells out.
pub(crate) struct Row {
    /// The bundle's name (the dedupe key within a scope).
    pub name: String,
    /// The winning reference (canonical where known). The inventory's `from` column is DERIVED
    /// from it — the reference already names where the bytes come from, so no second source
    /// string rides along to drift from it.
    pub reference: String,
    /// EVERY channel the last delivery cached for it — the `--channel` filter's key (its first
    /// entry is the delivering channel, when the line rides one).
    pub via_channels: Vec<String>,
    /// The delivery attribution (`assigned by <name>` / `picked by you`), when known.
    pub attribution: Option<String>,
    /// The applied version (the scope store's lock base), when the store holds one.
    pub version: Option<String>,
    /// The applied bytes' consent hash (the lock's), when the store holds one.
    pub digest: Option<String>,
    /// The line's honest state, phrased from local knowledge only.
    pub state: StatusItemState,
    /// The workspace's opaque id, when the delivery cache names one.
    pub workspace_id: Option<String>,
    /// The manifest FILE whose row delivers it (absent on a feed-delivered line).
    pub source_file: Option<String>,
    /// That row's spelled key (the joined reference).
    pub source_key: Option<String>,
    /// `<host>/<workspace>` when a feed delivers it.
    pub feed: Option<String>,
    /// The row's version pin, when one is spelled.
    pub pin: Option<String>,
    /// The placement dirs this machine holds for it.
    pub placements: Vec<String>,
    /// Where this row's local EDITS sit, when it has any (see [`DraftCopies`]).
    pub draft_in: DraftCopies,
    /// The scope-store record this row's applied state was read from, when one answered — what a
    /// reader who needs another of that record's documents opens, instead of walking the store
    /// for a matching name. Two non-retired records in one scope can hold ONE display name (two
    /// workspaces, or a workspace copy beside a local one), so a name is not an identity.
    pub record: Option<crate::id::SkillId>,
    /// A BUNDLE line (not an `"off"` switch) — what the inventory counts as a delivered skill.
    pub bundle: bool,
    /// The cached bundle kind (`Some("mcp")` for a config-placed bundle; `None` = a skill).
    pub kind: Option<String>,
    /// For an `mcp` line: the per-agent config states the last converge cached (agent + state +
    /// placed file) — the deep dive shows them instead of placement dirs.
    pub harness_states: Vec<topos_types::results::McpAgentState>,
    /// For an `mcp` line whose row freezes a `dest` naming no config file topos can edit: why the
    /// bundle reaches NO agent. `None` whenever it reaches at least one.
    pub mcp_unreachable: Option<String>,
}

/// One scope's whole resolution: its label, its governing file, its lines, and the disclosure
/// notes only this scope can say.
pub(crate) struct ScopeResolution {
    /// `"project"` or `"machine"`.
    pub scope: &'static str,
    /// The governing manifest file (pretty-printed); `None` = no file (nothing demanded
    /// machine-wide).
    pub manifest: Option<String>,
    /// The same file as a PATH — what a view spelling it another way (`list`'s header renders a
    /// project manifest relative to the working directory) needs, since a pretty spelling cannot
    /// be un-abbreviated back into one.
    pub manifest_path: Option<PathBuf>,
    pub rows: Vec<Row>,
    pub notes: Vec<String>,
    /// The SET rows the file adopts, canonical (`<host>/<ws>/channels/<name>`, repo sets) — the
    /// `--remote` view's "adopted in which file" lookup.
    pub sets: Vec<String>,
    /// The exact forge QUESTIONS this scope asks — `<source>#<ref>`, one per external row, the ref
    /// being the row's pin (empty when it floats). The check log is filed under exactly these, so
    /// carrying them is what lets a scoped read answer about its OWN rows: two scopes can track one
    /// repository at different pins, and a hidden scope's newer answer is not this one's news.
    pub forge_questions: Vec<String>,
}

impl ScopeResolution {
    /// The rows the INVENTORY shows: the bundle lines plus the `"off"` switches (an off row is a
    /// standing statement, never invisible).
    pub(crate) fn inventory_rows(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|r| r.bundle || matches!(r.state, StatusItemState::Off))
    }

    /// Delivered skills in this scope (the summary's `skills` count).
    pub(crate) fn skills(&self) -> u64 {
        self.rows.iter().filter(|r| r.bundle).count() as u64
    }

    /// Rows sitting behind their last-known served target (`updates pending`).
    pub(crate) fn updates_pending(&self) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.bundle && matches!(r.state, StatusItemState::Behind))
            .count() as u64
    }

    /// Rows whose placements scan Modified (`drafts ahead`). A BLOCKED row is deliberately not one
    /// of them: its edits are not a draft anybody can share, and the command this count ends in
    /// would send the reader to a row whose exits are the merge's two.
    pub(crate) fn drafts_ahead(&self) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.bundle && matches!(r.state, StatusItemState::LocalEdits))
            .count() as u64
    }

    /// Rows whose merge is undecided — the one count that names a DECISION rather than an update
    /// that would apply itself. Their NAMES, because the count's pointer is only useful if it
    /// reaches the workbench: `topos list` says a merge waits, `topos list <name>` says where it
    /// is and how to end it, and one waiting merge has a name to spell.
    pub(crate) fn waiting_on_you(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter(|r| r.bundle && matches!(r.state, StatusItemState::Blocked))
            .map(|r| r.name.as_str())
            .collect()
    }
}

/// The whole resolution: the scopes in render order (project first when one covers the cwd, the
/// machine scope always last), the per-workspace regimes (a machine-scope fact), and the PERSON
/// plan the machine scope was resolved from — the counts that speak for the machine scope
/// (`awaiting_first_sync`) need the same recipe the rows came from, and re-parsing the global
/// manifest to get it would risk two different answers from one command.
pub(crate) struct Resolved {
    pub scopes: Vec<ScopeResolution>,
    pub regimes: Vec<StatusRegime>,
    pub person_plan: ScopePlan,
}

impl Resolved {
    /// The project scope, when one covers the working directory.
    pub(crate) fn project(&self) -> Option<&ScopeResolution> {
        self.scopes.iter().find(|s| s.scope == "project")
    }

    /// The machine scope (always resolved).
    pub(crate) fn machine(&self) -> &ScopeResolution {
        self.scopes
            .iter()
            .find(|s| s.scope == "machine")
            .expect("the machine scope is always resolved")
    }
}

/// One scope's pass — its lines plus the notes only that pass can see.
#[derive(Default)]
struct ScopeOut {
    rows: Vec<Row>,
    /// Two SETS delivering one name at different versions — the winner named.
    collisions: Vec<String>,
    /// `"off"` switches whose bundle is not in the cached feed any more.
    stale_offs: Vec<String>,
    /// Feeds/channel lines that have delivered nothing yet, or whose completed exchange assigned
    /// this person nothing — said as sentences, since no row can carry them.
    quiet_lines: Vec<String>,
    /// Feed ADDRESSES whose last exchange completed and assigned this person nothing. The fact is
    /// the WORKSPACE's, not a scope's, so `resolve` says it once however many recipes adopt it.
    empty_feeds: Vec<String>,
}

/// Resolve both scopes, the regimes, and the per-scope notes — the whole offline answer.
///
/// # Errors
/// A read failure or a manifest the grammar refuses (typed, naming the fix).
pub(crate) fn resolve(
    ctx: &Ctx<'_>,
    all: &Sessions,
    cache: &SyncStatus,
) -> Result<Resolved, ClientError> {
    let connected: Vec<(String, String)> = all
        .live()
        .map(|s| (s.host.clone(), s.workspace_name.clone()))
        .collect();
    let person_plan = scopes::person_plan(ctx.fs, &ctx.layout)?;
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
        let project_store = crate::sidecar::existing_project_store(ctx.fs, dir);
        project_out = scope_rows(
            ctx,
            plan,
            ManifestScope::Project,
            project_store.as_ref(),
            cache,
            all,
        );
    }
    let mut person_out = scope_rows(
        ctx,
        &person_plan,
        ManifestScope::Global,
        Some(&ctx.layout),
        cache,
        all,
    );

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

    // The MACHINE scope's notes, in reading order: the loud one first, then the quiet feeds,
    // then the inert rows, then the frictions.
    let mut machine_notes = Vec::new();
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
            machine_notes.push(format!(
                "global manifest adopts {adopts} bundles; {unadopted} assigned bundles are not \
                 adopted here (no feed row) — `topos add -g @{ws}` restores them"
            ));
        }
    }
    // A feed that HAS exchanged and brought nothing: the rows can only fall silent, so the fact
    // gets words. Said once per address — it is the workspace's answer, not each scope's.
    let mut said: BTreeSet<&str> = BTreeSet::new();
    let mut project_notes = Vec::new();
    for feed in &person_out.empty_feeds {
        if said.insert(feed.as_str()) {
            machine_notes.push(format!("{feed}: exchanged — nothing assigned to you yet"));
        }
    }
    for feed in &project_out.empty_feeds {
        if said.insert(feed.as_str()) {
            project_notes.push(format!("{feed}: exchanged — nothing assigned to you yet"));
        }
    }
    machine_notes.append(&mut person_out.quiet_lines);
    project_notes.append(&mut project_out.quiet_lines);
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
            machine_notes.push(format!(
                "{} adds nothing — the feed already delivers it",
                row.reference
            ));
        }
    }
    machine_notes.extend(person_out.stale_offs.iter().cloned());
    project_notes.extend(project_out.stale_offs.iter().cloned());
    machine_notes.extend(person_out.collisions.iter().cloned());
    project_notes.extend(project_out.collisions.iter().cloned());
    // Declined on the web, delivered here anyway: the file outranks the person's own web choice.
    for entry in cache.workspaces.values() {
        let (Some(host), Some(ws)) = (&entry.host, &entry.workspace_name) else {
            continue;
        };
        for name in entry.declined.values() {
            if person_plan.explicit_claims(host, ws, name) {
                machine_notes.push(format!(
                    "{name}: declined on the web, delivered here by your global manifest"
                ));
            }
        }
    }
    // The same bundle at BOTH scopes on different versions — nothing blends, so both copies land.
    // Disclosed on the PROJECT side: it is the project copy that is projected in-repo.
    let claude = claude_code_detected(ctx);
    for prow in project_out.rows.iter().filter(|r| r.bundle) {
        let Some(person) = person_out
            .rows
            .iter()
            .find(|r| r.bundle && r.name == prow.name)
        else {
            continue;
        };
        let here = prow.pin.clone().or_else(|| prow.version.clone());
        let there = person.pin.clone().or_else(|| person.version.clone());
        if let (Some(a), Some(b)) = (here, there)
            && a != b
        {
            let mut note = format!(
                "{}: project pins {}, machine delivers {} — the project copy is projected in-repo",
                prow.name,
                crate::render::short(&a),
                crate::render::short(&b)
            );
            if claude {
                note.push_str(
                    "; Claude Code resolves personal skills ahead of project ones, so the \
                     personal copy may win anyway",
                );
            }
            project_notes.push(note);
        }
    }

    let mut out = Vec::new();
    if let Some((_, plan)) = &project {
        out.push(ScopeResolution {
            scope: "project",
            manifest: plan.file.as_deref().map(|p| pretty(ctx, p)),
            manifest_path: plan.file.clone(),
            rows: project_out.rows,
            notes: project_notes,
            sets: plan.sets.iter().map(|r| r.shape.canonical()).collect(),
            forge_questions: forge_questions(plan),
        });
    }
    out.push(ScopeResolution {
        scope: "machine",
        manifest: person_plan.file.as_deref().map(|p| pretty(ctx, p)),
        manifest_path: person_plan.file.clone(),
        rows: person_out.rows,
        notes: machine_notes,
        sets: person_plan
            .sets
            .iter()
            .map(|r| r.shape.canonical())
            .collect(),
        forge_questions: forge_questions(&person_plan),
    });
    Ok(Resolved {
        scopes: out,
        regimes,
        person_plan,
    })
}

/// One scope's lines, in reading order: the explicit THING rows, each SET row's cached members
/// itemized, the flowing FEEDS, then the `"off"` switches. Deduped by bundle identity as it goes —
/// the first statement to claim an identity wins, and things are read before sets and feeds,
/// which IS the "an explicit row beats a set" rule. A set or feed that itemizes NOTHING becomes a
/// sentence (a quiet line), never a silent absence.
fn scope_rows(
    ctx: &Ctx<'_>,
    plan: &ScopePlan,
    scope: ManifestScope,
    layout: Option<&Layout>,
    cache: &SyncStatus,
    all: &Sessions,
) -> ScopeOut {
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
    // This scope's MCP ownership ledger — the ONLY record a LOCAL mcp row (a folder adopted in
    // place, a document imported by name or URL) has of where its config entries went: no
    // workspace delivers it, so no delivery cache ever describes it. Read once, and only when the
    // document is there, so the ordinary machine with no mcp bundles touches nothing.
    let ledger = layout
        .filter(|l| ctx.fs.exists(&l.mcp_ledger_path()))
        .and_then(|l| crate::mcp_ledger::read(ctx.fs, l).ok())
        .unwrap_or_default();

    // 1. The explicit THING rows — one bundle each, the row's own fields winning.
    for row in &plan.things {
        let identity = row.shape.canonical();
        if !claimed.insert(identity.clone()) {
            continue;
        }
        let name = row.display_name();
        let mut via_channels = Vec::new();
        let mut attribution = None;
        let mut workspace_id = None;
        let mut kind = row.fields().kind.clone();
        let mut harness_states = Vec::new();
        // The identities this row could be filed under in the scope's MCP ledger, in order: a
        // workspace bundle by its skill id; a local folder by the id this scope's store tracks it
        // by, or — for an imported document no store adopted — its name-keyed local identity.
        let mut ledger_ids: Vec<String> = Vec::new();
        let applied = match &row.shape {
            KeyShape::WorkspaceBundle {
                host,
                workspace,
                bundle,
            } => match cache_lookup(cache, host, workspace, bundle) {
                Some(hit) => {
                    via_channels = hit.ds.via_channels.clone();
                    attribution = attribution_of(hit.ds);
                    workspace_id = Some(hit.workspace_id.to_owned());
                    kind = kind.or_else(|| hit.ds.kind.clone());
                    harness_states = hit.ds.harness_states.clone();
                    ledger_ids.push(hit.skill_id.to_owned());
                    // A PINNED row's target is its pin, never the served current: the pin is what
                    // `update` delivers here, so measuring against `current` would report a row
                    // sitting exactly where it was asked to sit as "behind — `topos update` lands
                    // the newer version", advice the next `update` correctly refuses to take.
                    let target = row.pin().unwrap_or_else(|| hit.ds.served_version.clone());
                    applied_for_id(ctx, layout, hit.skill_id, &target)
                }
                None => {
                    ledger_ids.extend(stored_id(ctx, layout, &mut index, &name));
                    stored_by_name(ctx, layout, &mut index, &name)
                        .unwrap_or_else(|| Applied::plain(session_state(all, host, workspace)))
                }
            },
            // A local folder: its presence IS the delivery (adopted in place — there is no
            // upstream to be behind or ahead of).
            KeyShape::LocalPath { raw } => {
                if ctx.fs.exists(&local_dir(ctx, base.as_deref(), raw)) {
                    ledger_ids.extend(stored_id(ctx, layout, &mut index, &name));
                    ledger_ids.push(format!("local:{name}"));
                    stored_by_name(ctx, layout, &mut index, &name)
                        .unwrap_or_else(|| Applied::plain(StatusItemState::Applied))
                } else {
                    Applied::plain(StatusItemState::Unknown)
                }
            }
            // A repo skill: only this scope's store can answer it offline.
            _ => stored_by_name(ctx, layout, &mut index, &name).unwrap_or_else(Applied::unknown),
        };
        // An mcp row the delivery cache says nothing about — every LOCAL one — takes its per-agent
        // entries from this scope's ledger. Without the join the deep dive reads "no agent config
        // entries recorded yet" forever, about entries this very scope placed.
        if kind.as_deref() == Some("mcp") && harness_states.is_empty() {
            harness_states = ledger_states(&ledger, &ledger_ids);
        }
        // A frozen `dest` that names no config file topos can edit costs the bundle every agent.
        // The deep dive resolves it here rather than reading it off the cache: the row is right in
        // front of it, and the fact must be told whether or not a sweep has run since the typo.
        //
        // ONLY while nothing of this bundle's is still sitting in an agent's config, though. An
        // entry topos placed and then found hand-edited is LEFT in place (drift is never
        // clobbered), and it keeps its ledger entry — so a dest change made afterwards would have
        // this line claim the bundle reaches nobody while that entry is still in the file, and
        // possibly still being loaded. A live entry outranks the row's arithmetic: the states below
        // say where the bytes actually are, and the sweep's own warning carries the causality.
        let placed_somewhere = ledger_ids.iter().any(|id| ledger.has_entries_for(id));
        let mcp_unreachable = (kind.as_deref() == Some("mcp") && !placed_somewhere)
            .then(|| {
                let narrowing = super::reconcile::mcp_dest_narrowing(row.fields().dest, scope);
                narrowing.reaches_nothing().then(|| {
                    crate::manifest::dest::dest_names_no_mcp_file(&narrowing.unknown, scope)
                })
            })
            .flatten();
        out.rows.push(Row {
            name,
            reference: identity,
            via_channels,
            attribution,
            version: applied.version,
            digest: applied.digest,
            state: applied.state,
            workspace_id,
            source_file: file.clone(),
            source_key: Some(row.reference.clone()),
            feed: None,
            pin: row.pin(),
            placements: applied.placements,
            draft_in: applied.draft_in,
            record: applied.record,
            bundle: true,
            kind,
            harness_states,
            mcp_unreachable,
        });
    }

    // 2. The SET rows — the members the last delivery cached under each. A set that itemizes
    // nothing is a sentence, not a silent absence: it promises the exchange, or names the access
    // fact standing in the way.
    for row in &plan.sets {
        let identity = row.shape.canonical();
        // A REPO set's expansion is not in the delivery cache (no workspace serves it) — it is
        // in this scope's own store, where every landed member records the origin it came from.
        // Itemizing it here is what makes those members ROWS of the set that delivers them: the
        // inventory's lines all originate from a manifest row, so a member the set row does not
        // itemize would simply be invisible — contradicting the `update` keeping it current.
        if let KeyShape::RepoSet { host, owner, repo } = &row.shape {
            let origin = format!("{host}/{owner}/{repo}");
            for member in repo_set_members(ctx, layout, &origin) {
                if !claimed.insert(member.reference.clone()) {
                    continue;
                }
                collide(
                    &mut by_name,
                    &mut out.collisions,
                    &member.name,
                    &identity,
                    &member.applied.version.clone().unwrap_or_default(),
                );
                out.rows.push(Row {
                    name: member.name,
                    reference: member.reference,
                    via_channels: Vec::new(),
                    attribution: None,
                    version: member.applied.version,
                    digest: member.applied.digest,
                    state: member.applied.state,
                    workspace_id: None,
                    source_file: file.clone(),
                    source_key: Some(row.reference.clone()),
                    feed: None,
                    pin: row.pin(),
                    placements: member.applied.placements,
                    draft_in: member.applied.draft_in,
                    record: member.applied.record,
                    bundle: true,
                    // A repo-set member is always a skill: `kind = "mcp"` on GitHub rows refuses.
                    kind: None,
                    harness_states: Vec::new(),
                    mcp_unreachable: None,
                });
            }
            continue;
        }
        let KeyShape::Channel {
            host,
            workspace,
            channel,
        } = &row.shape
        else {
            continue;
        };
        let entry = ws_entry(cache, host, workspace);
        let mut itemized = 0usize;
        for (ws_id, skill_id, ds) in entry
            .into_iter()
            .flat_map(|(id, e)| e.delivered.iter().map(move |(sid, d)| (id, sid, d)))
        {
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
            let applied = applied_for_id(ctx, layout, skill_id, &ds.served_version);
            out.rows.push(Row {
                name: ds.name.clone(),
                reference: member,
                via_channels: ds.via_channels.clone(),
                attribution: attribution_of(ds),
                version: applied.version,
                digest: applied.digest,
                state: applied.state,
                workspace_id: Some(ws_id.to_owned()),
                source_file: file.clone(),
                source_key: Some(row.reference.clone()),
                feed: None,
                pin: None,
                placements: applied.placements,
                draft_in: applied.draft_in,
                record: applied.record,
                bundle: true,
                kind: ds.kind.clone(),
                harness_states: ds.harness_states.clone(),
                mcp_unreachable: None,
            });
            itemized += 1;
        }
        if itemized == 0 {
            out.quiet_lines.push(quiet_set_line(
                &row.reference,
                session_state(all, host, workspace),
            ));
        }
    }

    // 3. The FEEDS that flow here (the global file's feed rows).
    for (host, workspace) in &plan.feeds {
        let feed = format!("{host}/{workspace}");
        let entry = ws_entry(cache, host, workspace);
        let mut itemized = 0usize;
        for (ws_id, skill_id, ds) in entry
            .into_iter()
            .flat_map(|(id, e)| e.delivered.iter().map(move |(sid, d)| (id, sid, d)))
        {
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
            let applied = applied_for_id(ctx, layout, skill_id, &ds.served_version);
            out.rows.push(Row {
                name: ds.name.clone(),
                reference: identity,
                via_channels: ds.via_channels.clone(),
                attribution: attribution_of(ds),
                version: applied.version,
                digest: applied.digest,
                state: applied.state,
                workspace_id: Some(ws_id.to_owned()),
                source_file: None,
                source_key: None,
                feed: Some(feed.clone()),
                pin: None,
                placements: applied.placements,
                draft_in: applied.draft_in,
                record: applied.record,
                bundle: true,
                kind: ds.kind.clone(),
                harness_states: ds.harness_states.clone(),
                mcp_unreachable: None,
            });
            itemized += 1;
        }
        // A feed that has itemized NOTHING and has no completed exchange behind it: ONE honest
        // sentence for the workspace itself. The test is delivery PROVENANCE, never the existence
        // of a cache entry — a landed publish SEEDS its workspace's entry (host, address, its own
        // manifest-driven row) without any delivery having answered, and only the sweep ever
        // stamps `last_delivery_at`. Reading the seed as an exchange would delete this line and
        // drop the adopted feed out of the answer entirely, since a `via_manifest` row itemizes
        // nothing here.
        //
        // A live session that has never delivered here is missing the EXCHANGE, not an apply —
        // the ordinary unknown state would promise `update` lands something, and this line cannot
        // know that the workspace assigns anything at all. Every other verdict (`session_state`'s
        // no-session and pending answers) is about access and stands.
        let exchanged = entry.is_some_and(|(_, e)| e.last_delivery_at.is_some());
        if !exchanged && itemized == 0 {
            let state = match session_state(all, host, workspace) {
                StatusItemState::Unknown => StatusItemState::NoDeliveryYet,
                settled => settled,
            };
            out.quiet_lines.push(quiet_feed_line(&feed, state));
        } else if exchanged && !entry.is_some_and(|(_, e)| assigns_anything(e)) {
            // The other side of the same fact: the exchange DID complete and the workspace has
            // nothing for this person. Said in words, the same words the sweep's receipt uses.
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
            name: row.display_name(),
            reference: identity,
            via_channels: Vec::new(),
            attribution: None,
            version: None,
            digest: None,
            state: StatusItemState::Off,
            workspace_id: None,
            source_file: file.clone(),
            source_key: Some(row.reference.clone()),
            feed: None,
            pin: None,
            placements: Vec::new(),
            // An `"off"` switch is a statement of the file, never a placed copy — nothing to edit,
            // and no store record was read for it.
            draft_in: DraftCopies::None,
            record: None,
            bundle: false,
            kind: None,
            harness_states: Vec::new(),
            mcp_unreachable: None,
        });
    }
    out
}

/// The sentence for an adopted feed that has itemized nothing (no rows can say it).
fn quiet_feed_line(feed: &str, state: StatusItemState) -> String {
    match state {
        StatusItemState::PendingSession => {
            format!("{feed}: awaiting session approval — delivery starts once approved")
        }
        StatusItemState::NotAvailable => {
            format!("{feed}: not available with your current access")
        }
        _ => format!("{feed}: no delivery yet — `topos update` performs the first exchange"),
    }
}

/// The sentence for a channel line that has itemized nothing.
fn quiet_set_line(reference: &str, state: StatusItemState) -> String {
    match state {
        StatusItemState::PendingSession => {
            format!("{reference}: awaiting session approval — delivery starts once approved")
        }
        StatusItemState::NotAvailable => {
            format!("{reference}: not available with your current access")
        }
        _ => format!(
            "{reference}: nothing delivered from this line yet — `topos update` performs the \
             exchange"
        ),
    }
}

/// The deep dive's answer: the wire detail the caller renders, and the store RECORD the answering
/// row's state was read from. The identity stays internal — a reader never types an opaque id, so
/// it is no field of the detail — but a caller that needs another of that record's documents must
/// open THAT record, never one a name-walk happens to find first.
#[derive(Debug)]
pub(crate) struct Dive {
    pub detail: ListDetail,
    /// The answering row's record, when a store record answered (see [`Row::record`]).
    pub record: Option<crate::id::SkillId>,
}

/// The deep single-skill answer (`topos list <name>`): the token resolved against the SAME lines
/// the inventory renders (a row reference, a leaf name, or a cached feed name; `@ws/…` and the
/// canonical spellings ride [`keys::parse_input`] with the one connected host as the default).
/// `sections` is what THIS invocation shows — the scope flags mean the same thing on the deep dive
/// as on the listing, so `-g` answers from the machine scope alone even inside a project — and its
/// order IS precedence.
///
/// # Errors
/// A token no shown scope delivers is the uniform [`ClientError::TargetNotFound`]; `list`'s caller
/// then answers for the placed built-in, or with the not-managed headline — never an error for a
/// local miss.
pub(crate) fn detail_for(
    sections: &[&ScopeResolution],
    all: &Sessions,
    token: &str,
) -> Result<Dive, ClientError> {
    let hosts: BTreeSet<&str> = all.live().map(|s| s.host.as_str()).collect();
    let default_host = if hosts.len() == 1 {
        hosts.iter().copied().next()
    } else {
        None
    };
    let canonical = keys::parse_input(token, default_host)
        .ok()
        .map(|r| r.shape.canonical());
    let hit = sections.iter().find_map(|s| {
        s.rows
            .iter()
            .find(|r| canonical.as_deref() == Some(r.reference.as_str()) || r.name == token)
            .map(|r| (s.scope, r))
    });
    let Some((scope, row)) = hit else {
        return Err(ClientError::TargetNotFound {
            target: token.to_owned(),
        });
    };
    Ok(Dive {
        record: row.record.clone(),
        detail: ListDetail {
            name: row.name.clone(),
            scope: Some(scope.to_owned()),
            source_file: row.source_file.clone(),
            source_key: row.source_key.clone(),
            feed: row.feed.clone(),
            attribution: row.attribution.clone(),
            version: row.version.clone(),
            pin: row.pin.clone(),
            placements: row.placements.clone(),
            state: row.state,
            kind: row.kind.clone(),
            // For an mcp line: the cached per-agent config entries (placed file + state) — the
            // deep dive's answer instead of placement dirs.
            harnesses: row.harness_states.clone(),
            mcp_unreachable: row.mcp_unreachable.clone(),
            managed: true,
            folders: Vec::new(),
            // The competing copies, when the row's own classification found some — the question
            // the row's `draft in N folders that disagree (see: topos list <name>)` line sends
            // here.
            diverged: match &row.draft_in {
                DraftCopies::Diverged(copies) => copies
                    .iter()
                    .map(|c| topos_types::results::DivergedCopy {
                        display: c.display.clone(),
                        dest: c.dest.clone(),
                    })
                    .collect(),
                DraftCopies::None | DraftCopies::In(_) => Vec::new(),
            },
            // The blocked line's workbench folder is filled by the dive's caller, which knows
            // which scope answered — the folder is that scope's store's, and it is spelled from
            // where the reader stands like every other path the dive prints.
            conflict_copy: None,
            conflict_reason: None,
        },
    })
}

/// How much of what the workspaces ASSIGN this person has never been applied here — the machine
/// scope's `assignments not applied` count (the delivery cache's feed rows; a manifest-driven row
/// is demand, not an assignment). A delivery whose sidecar sync doc still holds the all-zero
/// baseline has never been applied; any unreadable doc makes the count honestly absent, never a
/// partial number.
///
/// Only a workspace this installation still holds a LIVE session for can assign anything here:
/// the cache OUTLIVES a logout, so counting every cached entry would keep crediting a workspace
/// nothing dials any more. A PENDING session counts (the workspace assigns; only an owner's
/// approval gates the bytes); an ENDED one does not. The key is `(host, workspace id)`, never the
/// id alone: ids are opaque strings each SERVER mints on its own, and an entry with NO recorded
/// host is not placeable on any server, so it counts for nobody.
///
/// And only what the MACHINE RESOLUTION DEMANDS counts. The count's whole promise is that
/// `topos update -g` applies it, so an assignment the person plan WITHHOLDS — a file-backed global
/// manifest with no feed row for that workspace, or an `"off"` row over the bundle — must not
/// appear: `update` deliberately will not land it, and the line would send the reader to a command
/// that cannot move the number. Those withheld rows are disclosed elsewhere (the loud no-feed-row
/// note; the `"off"` row shows as its own inventory line), so nothing goes silent.
pub(crate) fn awaiting_first_sync(
    ctx: &Ctx<'_>,
    all: &Sessions,
    cache: &SyncStatus,
    plan: &ScopePlan,
) -> Option<u64> {
    // Keyed by `(host, workspace id)` — the live check; valued by the workspace's ADDRESS name,
    // which is what the plan's rows are spelled with (`<host>/<workspace>/<bundle>`).
    let live: BTreeMap<(&str, &str), &str> = all
        .live()
        .map(|s| {
            (
                (s.host.as_str(), s.workspace_id.as_str()),
                s.workspace_name.as_str(),
            )
        })
        .collect();
    let mut awaiting = Some(0u64);
    for (workspace_id, entry) in &cache.workspaces {
        let Some(host) = entry.host.as_deref() else {
            continue;
        };
        let Some(workspace) = live.get(&(host, workspace_id.as_str())).copied() else {
            continue;
        };
        for (skill_id, d) in &entry.delivered {
            if d.withdrawn || d.via_manifest {
                continue;
            }
            // The demand test: the workspace's feed flows here (a feed row spells it — with no
            // global file nothing is demanded machine-wide) and no `"off"` row covers the bundle —
            // or an explicit row claims the bundle outright, which delivers it whatever the feed
            // does.
            let demanded = plan.explicit_claims(host, workspace, &d.name)
                || (plan.has_feed(host, workspace)
                    && plan.off_for(host, workspace, &d.name).is_none());
            if !demanded {
                continue;
            }
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
    awaiting
}

// =================================================================================================
// The offline readers — the delivery cache, the per-scope stores, the sessions file.
// =================================================================================================

/// The honest local state of one bundle, read from one scope's store alone.
struct Applied {
    state: StatusItemState,
    version: Option<String>,
    digest: Option<String>,
    placements: Vec<String>,
    draft_in: DraftCopies,
    /// The store record every field above was read from, when one answered — the identity a
    /// later reader needs to open the SAME record's other documents. Carried rather than
    /// re-derived from the name: two non-retired records in one scope can hold ONE display name
    /// (two workspaces, or a workspace copy beside a local one), so a name is not an identity.
    record: Option<crate::id::SkillId>,
}

impl Applied {
    fn plain(state: StatusItemState) -> Self {
        Applied {
            state,
            version: None,
            digest: None,
            placements: Vec::new(),
            draft_in: DraftCopies::None,
            record: None,
        }
    }

    fn unknown() -> Self {
        Applied::plain(StatusItemState::Unknown)
    }
}

/// The APPLIED state for one bundle in one scope's store: never applied → `Unknown` ("not applied
/// here yet"); an undecided merge → `Blocked`; a scanned draft → `LocalEdits`; a served version
/// past the applied one → `Behind`; else `Applied` — an offline cache fact ("as of the last sync"),
/// never a live claim. Any unreadable doc degrades to `Unknown`.
///
/// **The blocked arm decides the row's VERSION, not only its word.** A conflict advances the lock's
/// base to the team's version while leaving every folder holding the person's own bytes, so the
/// lock is exactly the wrong thing for a blocked row to print: `<name>@<theirs> (draft)` reads as
/// "you are on the team's new version with local edits" about a folder that never received it. The
/// conflict record's own `draft_commit`/`draft_digest` are the version the folders DO hold, so a
/// blocked row names those. Presence of the record is the test, the same gate `publish` applies —
/// a leftover record from a crashed exit blocks publishing too, and healing it is the next
/// `update`'s job, not a read verb's.
fn applied_for_id(
    ctx: &Ctx<'_>,
    layout: Option<&Layout>,
    skill_id: &str,
    served_version: &str,
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
    let blocked: Option<ConflictState> = doc::read_doc(ctx.fs, &sp.conflict).ok().flatten();
    let version = Some(
        blocked
            .as_ref()
            .map_or_else(|| lock.base_commit.clone(), |cs| cs.draft_commit.clone()),
    );
    let digest = Some(
        blocked
            .as_ref()
            .map_or_else(|| lock.bundle_digest.clone(), |cs| cs.draft_digest.clone()),
    );
    let mut placements = Vec::new();
    let mut edited = false;
    let mut draft_in = DraftCopies::None;
    if let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) {
        placements.clone_from(&map.placements);
        let scoped = super::pull::ctx_with_layout(ctx, layout);
        if let Ok(scans) = placement::scan_placements(&scoped, &map) {
            // The ONE draft classification — the same verdict the sync engine reconciles with —
            // read off the scan that was happening anyway. `edited` stays a separate flag so a
            // verdict whose folder cannot be named still reports the state it always did.
            match placement::classify_draft(&scans, &map) {
                placement::DraftVerdict::NoDraft => {}
                placement::DraftVerdict::One { idx, .. } => {
                    edited = true;
                    if let Some(scan) = scans.get(idx) {
                        draft_in = DraftCopies::In(scan.dir.clone());
                    }
                }
                placement::DraftVerdict::Competitors(indices) => {
                    edited = true;
                    // Spelled at the SCOPED ctx (the store that was just read), so the folders the
                    // deep dive prints and the `--dest` values its commands take are the same
                    // strings the freeze's own refusal would print for this bundle.
                    draft_in = DraftCopies::Diverged(
                        scans
                            .iter()
                            .filter(|s| indices.contains(&s.idx))
                            .map(|s| super::dest_select::copy_spellings(&scoped, &s.dir))
                            .collect(),
                    );
                }
            }
        }
    }
    // A RESOLUTION is a draft even though every folder agrees with the record: topos itself wrote
    // those bytes, so nothing scans as edited, yet what they hold is not the version the base
    // names — and `publish` is its way out, which is exactly what `draft` means everywhere else.
    // The durable documents say so on their own: `work_hash` is what topos last wrote, and it
    // differs from the base's digest only where a merge (or a `--keep-mine`) settled onto it. No
    // folder is named as THE draft, because every copy holds it.
    if sync.work_hash != lock.bundle_digest {
        edited = true;
    }
    // The block outranks the draft word: these edits are not a publishable draft, and the row's
    // exits are the merge's two, never `publish`. No folder is named either — the question a
    // blocked row answers is which version to keep, not where to edit.
    if blocked.is_some() {
        return Applied {
            state: StatusItemState::Blocked,
            version,
            digest,
            placements,
            draft_in: DraftCopies::None,
            record: Some(sid),
        };
    }
    if edited {
        return Applied {
            state: StatusItemState::LocalEdits,
            version,
            digest,
            placements,
            draft_in,
            record: Some(sid),
        };
    }
    if !served_version.is_empty() && served_version != lock.base_commit {
        return Applied {
            state: StatusItemState::Behind,
            version,
            digest,
            placements,
            draft_in: DraftCopies::None,
            record: Some(sid),
        };
    }
    Applied {
        state: StatusItemState::Applied,
        version,
        digest,
        placements,
        draft_in: DraftCopies::None,
        record: Some(sid),
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
        // A RETIRED record answers for nothing: a row spelling its name reads honestly
        // never-applied until the next `update` re-claims (and revives) the record.
        if crate::sidecar::record_retired(ctx.fs, layout, &sid) {
            continue;
        }
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
    let id = stored_id(ctx, layout, index, name)?;
    Some(applied_for_id(ctx, layout, &id, ""))
}

/// The skill id this scope's store files a NAME under, when it holds one (the same index
/// [`stored_by_name`] reads, built at most once per scope).
fn stored_id(
    ctx: &Ctx<'_>,
    layout: Option<&Layout>,
    index: &mut Option<BTreeMap<String, String>>,
    name: &str,
) -> Option<String> {
    let layout = layout?;
    index
        .get_or_insert_with(|| store_index(ctx, layout))
        .get(name)
        .cloned()
}

/// The per-agent config entries one scope's MCP ledger records for a bundle. The ledger holds only
/// COMMITTED placements — a drifted or unprovable surface commits none — so every entry it answers
/// is one topos wrote and last knew as current: `state: "current"` here is an "as of the last
/// converge" claim (the deep dive's rendered heading says so), the same one a cached workspace row
/// makes, for the local rows no cache describes. It is NOT a live probe — an entry hand-edited
/// since that converge still answers `current` until the next converge observes the drift.
///
/// The first identity that answers wins: a bundle is filed under ONE of the spellings its row
/// could carry, never several at once.
fn ledger_states(
    ledger: &crate::mcp_ledger::McpLedger,
    ids: &[String],
) -> Vec<topos_types::results::McpAgentState> {
    ids.iter()
        .find_map(|id| {
            let states: Vec<topos_types::results::McpAgentState> = ledger
                .entries
                .iter()
                .filter(|(_, e)| &e.bundle_id == id)
                .filter_map(|(key, e)| {
                    Some(topos_types::results::McpAgentState {
                        // The ledger key is `"<harness slug>/<entry key>"`.
                        agent: key.split_once('/')?.0.to_owned(),
                        // As of the last converge (see the fn doc) — the ledger's fingerprint,
                        // not a live read of the file.
                        state: "current".to_owned(),
                        note: None,
                        file: Some(e.file.clone()),
                    })
                })
                .collect();
            (!states.is_empty()).then_some(states)
        })
        .unwrap_or_default()
}

/// The forge QUESTIONS one scope's recipe asks: every external row, at the ref the row spells.
/// This is the key the check log files outcomes under, so it is what a scoped read must match on —
/// the same row at two different pins in two scopes is two questions, and only one of them is any
/// given scope's business.
fn forge_questions(plan: &ScopePlan) -> Vec<String> {
    plan.sets
        .iter()
        .chain(plan.things.iter())
        .filter(|row| row.shape.is_forge())
        .map(|row| {
            let source = match &row.shape {
                KeyShape::RepoSet { host, owner, repo }
                | KeyShape::RepoSkill {
                    host, owner, repo, ..
                } => format!("{host}/{owner}/{repo}"),
                _ => String::new(),
            };
            crate::forge_check::question(&source, &row.pin().unwrap_or_default())
        })
        .collect()
}

/// The last auto-update check of every external source the SHOWN scopes name. Read from the
/// machine's own check log — the scopes supply which sources are in play, the log supplies what
/// happened to each.
///
/// SHOWN, not resolved: the scope flags mean the same thing here as everywhere else, so `-g`
/// answers about the machine scope alone and never surfaces a failure belonging to a project the
/// invocation deliberately did not ask about. Both read verbs show the same rows, because the
/// question ("is my GitHub row still being kept current?") is the same question in both. A source
/// no check has touched yet contributes nothing: an empty answer is honest, an invented one is not.
pub(crate) fn forge_sources(ctx: &Ctx<'_>, shown: &[&ScopeResolution]) -> Vec<ForgeSource> {
    let wanted: BTreeSet<&str> = shown
        .iter()
        .flat_map(|section| section.forge_questions.iter().map(String::as_str))
        .collect();
    // The log is filed per QUESTION (a source at a ref); the display is per SOURCE. Where one
    // repository is asked about at two refs BY THE SHOWN SCOPES, the most RECENT check answers "is
    // this still being kept current?" — an older question's outcome is not news about the source.
    let log = crate::forge_check::read(ctx.fs, &ctx.layout);
    let mut newest: BTreeMap<&str, &crate::forge_check::SourceCheck> = BTreeMap::new();
    for (key, check) in &log.sources {
        if !wanted.contains(key.as_str()) {
            continue;
        }
        let source = crate::forge_check::source_of(key);
        newest
            .entry(source)
            .and_modify(|held| {
                if check.checked_at_ms >= held.checked_at_ms {
                    *held = check;
                }
            })
            .or_insert(check);
    }
    newest
        .into_iter()
        .map(|(source, check)| ForgeSource {
            source: source.to_owned(),
            checked_at: check.checked_at_ms,
            answered_at: check.answered_at_ms,
            commit: check.commit.clone(),
            error: check.failure.as_ref().map(|f| f.reason.clone()),
            gone: check.failure.as_ref().is_some_and(|f| f.gone),
        })
        .collect()
}

/// One member a repo-set row delivers, as the inventory renders it.
struct RepoSetMember {
    name: String,
    /// The canonical 4-segment reference for the member (`<host>/<owner>/<repo>/<skill>`).
    reference: String,
    applied: Applied,
}

/// The members a repo-set row DELIVERS into this scope right now, read from the scope's own store:
/// the imports that record this origin AND still hold placed bytes here. That record is written
/// when a member lands and rewritten on every refresh, so it is the offline answer to "what does
/// this line deliver here?" — the same role the delivery cache plays for a channel line.
///
/// STILL HOLD is the load-bearing half. When an upstream repository drops a member, the reconcile
/// retires its placements but deliberately KEEPS the origin and lock: the bytes are custody, and
/// custody outlives delivery. A record whose placements are gone is a retained leftover, not a
/// delivery — claiming it here would make `list` assert a withdrawn member is live, which is the
/// same lie as the one this enumeration was added to stop, told in the other direction. (The
/// leftover itself mints no line; the one-time orphan resolution on `update` retires it.)
///
/// No served version is knowable offline, so a member never reads `behind` — only applied, edited,
/// or unknown, exactly as any other stored-by-name resolution.
fn repo_set_members(ctx: &Ctx<'_>, layout: Option<&Layout>, origin: &str) -> Vec<RepoSetMember> {
    let Some(layout) = layout else {
        return Vec::new();
    };
    let sctx = crate::ops::ctx_with_layout(ctx, layout);
    crate::ops::forge_imports(&sctx)
        .into_iter()
        .filter(|i| i.origin.source == origin && holds_placed_bytes(ctx, layout, &i.sid))
        .map(|i| RepoSetMember {
            reference: format!("{origin}/{}", i.lock.name),
            applied: applied_for_id(ctx, Some(layout), i.sid.as_str(), ""),
            name: i.lock.name,
        })
        .collect()
}

/// Whether this scope still holds MANAGED bytes on disk for a skill id — a recorded placement that
/// really exists. The placement map is the record of what was written; a retirement empties it (or
/// leaves paths that are gone), and that absence is the difference between a delivery and a
/// leftover.
fn holds_placed_bytes(ctx: &Ctx<'_>, layout: &Layout, sid: &crate::id::SkillId) -> bool {
    let Ok(Some(map)) = doc::read_map(ctx.fs, &layout.published(sid).map) else {
        return false;
    };
    map.placements.iter().any(|p| ctx.fs.exists(Path::new(p)))
}

/// One cached delivery a line resolves against.
struct CacheHit<'a> {
    workspace_id: &'a str,
    skill_id: &'a str,
    ds: &'a DeliveredSkill,
}

/// The cache entry of one workspace, by its manifest-grammar address — keyed, so a row can carry
/// the workspace's opaque id alongside.
fn ws_entry<'a>(
    cache: &'a SyncStatus,
    host: &str,
    workspace: &str,
) -> Option<(&'a str, &'a WorkspaceSync)> {
    cache
        .workspaces
        .iter()
        .find(|(_, e)| {
            e.host.as_deref() == Some(host) && e.workspace_name.as_deref() == Some(workspace)
        })
        .map(|(id, e)| (id.as_str(), e))
}

/// The cached delivery of `<host>/<workspace>/<name>`, whatever asked for it.
fn cache_lookup<'a>(
    cache: &'a SyncStatus,
    host: &str,
    workspace: &str,
    name: &str,
) -> Option<CacheHit<'a>> {
    let (workspace_id, entry) = ws_entry(cache, host, workspace)?;
    let (skill_id, ds) = entry
        .delivered
        .iter()
        .find(|(_, d)| !d.withdrawn && d.name == name)?;
    Some(CacheHit {
        workspace_id,
        skill_id,
        ds,
    })
}

/// Whether the workspace's last delivery ASSIGNED this bundle to the person (a feed row — a
/// manifest-driven delivery is demand, not an assignment).
pub(crate) fn feed_delivers(cache: &SyncStatus, host: &str, workspace: &str, bundle: &str) -> bool {
    ws_entry(cache, host, workspace).is_some_and(|(_, e)| {
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
    let Some((_, entry)) = ws_entry(cache, host, workspace) else {
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
pub(crate) fn pretty(ctx: &Ctx<'_>, path: &Path) -> String {
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

/// The reader both verbs open with: the sessions file and the offline delivery cache.
pub(crate) fn read_sources(ctx: &Ctx<'_>) -> Result<(Sessions, SyncStatus), ClientError> {
    let all = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let cache = sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();
    Ok((all, cache))
}

/// The shared fixture kit for the resolution's three consumers (`inventory`/`status`/`list`
/// tests): a self-cleaning temp MACHINE home with writers for the global manifest, sessions, the
/// delivery cache, and a scope store — plus the one Ctx builder every suite runs through.
#[cfg(test)]
pub(crate) mod testkit {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_types::PERSISTED_SCHEMA_VERSION;
    use topos_types::persisted::{
        Lock, PlacementKind, PlacementMap, PlacementState, SwapCapability, SyncState,
    };

    use crate::ctx::Ctx;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::manifest::MANIFEST_FILE;
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;
    use crate::sync_status::{DeliveredSkill, WorkspaceSync};
    use crate::{doc, sync_status};

    /// A self-cleaning temp MACHINE home (RAII): `<home>/.topos` is the sidecar, `<home>` the
    /// root the manifest walk and the placement probes resolve against.
    pub(crate) struct TempHome(pub PathBuf);

    impl TempHome {
        pub(crate) fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-inv-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn layout(&self) -> Layout {
            Layout::new(&self.0.join(".topos"))
        }

        /// Write the GLOBAL manifest (`~/.topos/topos.toml`).
        pub(crate) fn global(&self, text: &str) {
            let dir = self.0.join(".topos");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(MANIFEST_FILE), text).unwrap();
        }

        /// A session for `<host>/<ws>`.
        pub(crate) fn session(&self, host: &str, ws_id: &str, ws: &str, status: &str) {
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
        pub(crate) fn cache(
            &self,
            ws_id: &str,
            host: &str,
            ws: &str,
            delivered: Vec<(String, DeliveredSkill)>,
            declined: Vec<(String, String)>,
        ) {
            sync_status::record(
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
                        last_exchange_fault: None,
                    },
                )],
            )
            .unwrap();
        }

        /// A PRE-SESSION-MODEL cache entry: one the document still tolerates, with no host and no
        /// address recorded.
        pub(crate) fn hostless_cache(&self, ws_id: &str, delivered: Vec<(String, DeliveredSkill)>) {
            sync_status::record(
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
                        last_exchange_fault: None,
                    },
                )],
            )
            .unwrap();
        }

        /// Lay an APPLIED store entry (dir + sync + lock + optional placements) for one skill in
        /// the MACHINE scope store. A placement dir's drift against the recorded sha is what a
        /// draft scan reads, and a lock `version` behind the cached served one is what `behind`
        /// reads.
        pub(crate) fn store_applied(
            &self,
            id: &str,
            name: &str,
            version: &str,
            placements: &[&str],
        ) {
            let fs = RealFs;
            let layout = self.layout();
            let sid = crate::id::SkillId::parse(id).unwrap();
            std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();
            let sp = layout.published(&sid);
            doc::write_doc(
                &fs,
                &sp.sync,
                &SyncState {
                    schema_version: PERSISTED_SCHEMA_VERSION,
                    observed: 1,
                    observed_version_id: version.to_owned(),
                    applied: 1,
                    base_commit: version.to_owned(),
                    work_hash: "e".repeat(64),
                    held: false,
                    draft_observed: None,
                },
            )
            .unwrap();
            doc::write_doc(
                &fs,
                &sp.lock,
                &Lock {
                    schema_version: PERSISTED_SCHEMA_VERSION,
                    skill_id: id.to_owned(),
                    name: name.to_owned(),
                    base_commit: version.to_owned(),
                    bundle_digest: "e".repeat(64),
                    files: Vec::new(),
                },
            )
            .unwrap();
            if !placements.is_empty() {
                doc::write_map(
                    &fs,
                    &sp.map,
                    &PlacementMap {
                        schema_version: 2,
                        placements: placements.iter().map(|p| (*p).to_owned()).collect(),
                        applied_commit: version.to_owned(),
                        materialized_sha: "e".repeat(64),
                        pre_existing_sha: None,
                        swap_capability: SwapCapability::Unsupported,
                        harness: None,
                        harness_layer: None,
                        harness_slug: None,
                        placement_state: placements
                            .iter()
                            .map(|_| PlacementState {
                                kind: PlacementKind::Native,
                                agent: None,
                                // The recorded sha deliberately never matches real dir bytes, so
                                // an EXISTING placement dir scans Modified (a draft) and an
                                // absent one scans Absent (clean).
                                materialized_sha: Some("e".repeat(64)),
                                pre_existing_sha: None,
                                swap_capability: SwapCapability::Unsupported,
                                adopted_source: false,
                            })
                            .collect(),
                    },
                )
                .unwrap();
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The skill id [`assigned`] derives for `name` — for laying a matching store entry.
    pub(crate) fn skill_id_of(name: &str) -> String {
        let tag = name.chars().next().unwrap_or('x');
        format!("topos_{}", std::iter::repeat_n(tag, 32).collect::<String>())
    }

    /// A cached FEED row (assigned to the person; never manifest-driven). `by = None` reads as
    /// the person's own pick.
    pub(crate) fn assigned(name: &str, by: Option<&str>) -> (String, DeliveredSkill) {
        (
            skill_id_of(name),
            DeliveredSkill {
                name: name.to_owned(),
                review_required: false,
                served_version: "d".repeat(64),
                withdrawn: false,
                via_channels: vec!["everyone".to_owned()],
                via_manifest: false,
                assigned_by: by.map(str::to_owned),
                kind: None,
                harness_states: Vec::new(),
                picked: by.is_none(),
            },
        )
    }

    /// Run `f` over a real-seam Ctx rooted at `home` (cwd optional) — the one builder every
    /// inventory-reading suite shares.
    pub(crate) fn with_ctx<R>(
        home: &TempHome,
        cwd: Option<&Path>,
        f: impl FnOnce(&Ctx<'_>) -> R,
    ) -> R {
        let fs = RealFs;
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = Ctx {
            progress: crate::progress::silent(),
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
                cwd: cwd.map(Path::to_path_buf),
            }),
        };
        f(&ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{TempHome, assigned, with_ctx};
    use super::*;
    use crate::fs_seam::RealFs;
    use crate::manifest::MANIFEST_FILE;

    fn resolve_at(home: &TempHome, cwd: &Path) -> Resolved {
        with_ctx(home, Some(cwd), |ctx| {
            let (all, cache) = read_sources(ctx).unwrap();
            resolve(ctx, &all, &cache).unwrap()
        })
    }

    fn awaiting_at(home: &TempHome, cwd: &Path) -> Option<u64> {
        with_ctx(home, Some(cwd), |ctx| {
            let (all, cache) = read_sources(ctx).unwrap();
            let resolved = resolve(ctx, &all, &cache).unwrap();
            awaiting_first_sync(ctx, &all, &cache, &resolved.person_plan)
        })
    }

    /// NO global file: nothing is demanded machine-wide — no rows, no counts, no regime. The
    /// feed flows only when a feed row says so (login writes it on first connection).
    #[test]
    fn no_global_file_demands_nothing_machine_wide() {
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

        let r = resolve_at(&home, &cwd);
        let m = r.machine();
        assert!(m.manifest.is_none(), "no file to name");
        assert!(
            m.rows.is_empty(),
            "{:?}",
            m.rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(awaiting_at(&home, &cwd), Some(0));
        assert!(r.regimes.is_empty(), "{:?}", r.regimes);
    }

    /// The workspace's FEED ROW itemizes the cached assignments — attributed, sourced to the
    /// feed itself.
    #[test]
    fn a_feed_row_itemizes_the_workspace_feed() {
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
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");

        let r = resolve_at(&home, &cwd);
        let m = r.machine();
        assert!(
            m.manifest
                .as_deref()
                .is_some_and(|f| f.ends_with("topos.toml")),
            "{:?}",
            m.manifest
        );
        let row = |n: &str| m.rows.iter().find(|i| i.name == n).expect("a row");
        assert_eq!(row("deploy").feed.as_deref(), Some("topos.sh/acme"));
        assert_eq!(row("deploy").reference, "topos.sh/acme/deploy");
        assert_eq!(
            row("deploy").attribution.as_deref(),
            Some("assigned by Dana")
        );
        assert_eq!(row("notes").attribution.as_deref(), Some("picked by you"));
        assert_eq!(
            row("deploy").via_channels.first().map(String::as_str),
            Some("everyone")
        );
        assert_eq!(row("deploy").workspace_id.as_deref(), Some("w_acme"));
        // Never applied here (no store): the honest not-applied-yet state, no false claim.
        assert!(matches!(row("deploy").state, StatusItemState::Unknown));
        assert_eq!(awaiting_at(&home, &cwd), Some(2));
        // The feed row adopts everything the workspace assigns; nothing to disclose.
        assert_eq!(r.regimes.len(), 1);
        assert_eq!(r.regimes[0].regime, "adopting all assigned");
        assert!(m.notes.is_empty(), "{:?}", m.notes);
    }

    /// The delivery cache OUTLIVES a logout, so the count must be session-scoped or a workspace
    /// nobody dials any more keeps claiming to assign bundles.
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
        // BOTH feed rows stand — a row outlives its session exactly as the cache does, so the
        // COUNT is what must stay session-scoped.
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/gone\" = \"*\"\n");

        assert_eq!(awaiting_at(&home, &cwd), Some(0));

        // A session for it — however it is minted — makes the same rows count again.
        home.session(
            "topos.sh",
            "w_gone",
            "gone",
            crate::sessions::SESSION_ACTIVE,
        );
        let r = resolve_at(&home, &cwd);
        assert_eq!(awaiting_at(&home, &cwd), Some(1));
        assert!(r.machine().rows.iter().any(|i| i.name == "ghost"));
    }

    /// PENDING counts, ENDED does not: a pending session's workspace still assigns (only an
    /// owner's approval gates the bytes), while an ended one has taken its access away.
    #[test]
    fn a_pending_session_counts_and_an_ended_one_does_not() {
        for (status, expected) in [
            (crate::sessions::SESSION_PENDING, Some(1)),
            (crate::sessions::SESSION_ENDED, Some(0)),
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
            home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");
            assert_eq!(awaiting_at(&home, &cwd), expected, "status {status}");
        }
    }

    /// Only what the machine resolution DEMANDS counts. A file-backed global manifest that omits a
    /// workspace's feed row withholds every assignment from it, and an `"off"` row withholds its
    /// bundle — `topos update -g` deliberately applies neither, so counting them would put a
    /// number next to a command that cannot move it. The withheld rows are still disclosed: the
    /// loud no-feed-row note names them, and an `"off"` row is its own inventory line.
    #[test]
    fn a_withheld_assignment_is_not_counted_as_not_applied() {
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
            vec![assigned("deploy", None), assigned("notes", None)],
            Vec::new(),
        );

        // No global file at all — nothing is demanded machine-wide, so nothing counts.
        assert_eq!(awaiting_at(&home, &cwd), Some(0));

        // The feed row demands the whole feed: both assignments count.
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");
        assert_eq!(awaiting_at(&home, &cwd), Some(2));

        // A global file with NO feed row for acme: the whole feed is withheld.
        home.global("[bundles]\n\"github.com/acme/tools\" = \"*\"\n");
        assert_eq!(
            awaiting_at(&home, &cwd),
            Some(0),
            "a feed the global manifest never adopts assigns nothing `update -g` can apply"
        );
        // …and it is not silent: the loud no-feed-row note names the count.
        let r = resolve_at(&home, &cwd);
        assert!(
            r.machine()
                .notes
                .iter()
                .any(|n| n.contains("2 assigned bundles are not adopted here (no feed row)")),
            "{:?}",
            r.machine().notes
        );

        // The feed row back, with ONE bundle switched off: the off bundle drops out of the count.
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"topos.sh/acme/notes\" = \"off\"\n");
        assert_eq!(awaiting_at(&home, &cwd), Some(1));

        // An EXPLICIT row claims a bundle whatever the feed does — a feed-less file still demands
        // the row it spells.
        home.global("[bundles]\n\"topos.sh/acme/deploy\" = \"*\"\n");
        assert_eq!(awaiting_at(&home, &cwd), Some(1));
    }

    /// A workspace id is an OPAQUE string each server mints on its own, so it is not an address.
    /// Matching the live set on the id alone would let a session on one host resurrect another
    /// host's stale cache entry.
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
        // The file demands BOTH addresses; the COUNT is what must stay live-session-scoped.
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n\"other.test/acme\" = \"*\"\n");

        assert_eq!(awaiting_at(&home, &cwd), Some(0));

        // The HOST is what decides: a session on the server that cached it counts the same rows.
        home.session(
            "other.test",
            "w_shared",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        let r = resolve_at(&home, &cwd);
        assert_eq!(awaiting_at(&home, &cwd), Some(1));
        assert!(r.machine().rows.iter().any(|i| i.name == "ghost"));
    }

    /// A cache entry with no recorded host cannot be placed on any server — it counts for nobody.
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

        let r = resolve_at(&home, &cwd);
        assert_eq!(awaiting_at(&home, &cwd), Some(0));
        assert!(!r.machine().rows.iter().any(|i| i.name == "ghost"));
    }

    /// A feed that has never delivered here is missing the EXCHANGE, not an apply — its note
    /// must not promise `update` lands something it cannot know exists. The access verdicts
    /// (pending approval) are a different fact and still word the note.
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
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");

        let r = resolve_at(&home, &cwd);
        assert!(
            r.machine().notes.iter().any(|n| n
                == "topos.sh/acme: no delivery yet — `topos update` performs the first exchange"),
            "{:?}",
            r.machine().notes
        );

        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_PENDING,
        );
        let r = resolve_at(&home, &cwd);
        assert!(
            r.machine()
                .notes
                .iter()
                .any(|n| n.contains("awaiting session approval")),
            "{:?}",
            r.machine().notes
        );
    }

    /// A cache ENTRY is not an exchange. A landed publish seeds its workspace's entry — host,
    /// address, and its own manifest-driven row — with no delivery having answered; only the
    /// sweep stamps `last_delivery_at`. Reading the seed as an exchange would delete the
    /// no-delivery note and drop the adopted feed out of the answer with nothing said.
    #[test]
    fn a_publish_seeded_entry_keeps_the_never_delivered_note() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        home.session(
            "topos.sh",
            "w_acme",
            "acme",
            crate::sessions::SESSION_ACTIVE,
        );
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");
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

        let r = resolve_at(&home, &cwd);
        assert!(
            r.machine()
                .notes
                .iter()
                .any(|n| n.starts_with("topos.sh/acme: no delivery yet")),
            "{:?}",
            r.machine().notes
        );
        // A manifest-driven row is demand, not an assignment — it counts for nothing here, and
        // nothing claims an exchange that never happened.
        assert_eq!(awaiting_at(&home, &cwd), Some(0));
        assert!(
            !r.machine().notes.iter().any(|n| n.contains("exchanged")),
            "{:?}",
            r.machine().notes
        );
    }

    /// The exchange COMPLETED and the workspace had nothing for this person: said in words, once,
    /// in the sweep receipt's own vocabulary.
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
        home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");

        let r = resolve_at(&home, &cwd);
        assert_eq!(
            r.machine().notes,
            vec!["topos.sh/acme: exchanged — nothing assigned to you yet"]
        );
        assert!(r.machine().rows.is_empty());

        // A workspace that DID assign something says nothing of the sort.
        home.cache(
            "w_acme",
            "topos.sh",
            "acme",
            vec![assigned("deploy", None)],
            Vec::new(),
        );
        let r = resolve_at(&home, &cwd);
        assert!(
            !r.machine().notes.iter().any(|n| n.contains("exchanged")),
            "{:?}",
            r.machine().notes
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

        let r = resolve_at(&home, &cwd);
        let m = r.machine();
        assert!(
            m.manifest
                .as_deref()
                .is_some_and(|f| f.ends_with("topos.toml")),
            "{:?}",
            m.manifest
        );
        // The file's own row delivers, sourced to the file — the other assignments do not flow.
        let deploy = m.rows.iter().find(|i| i.name == "deploy").expect("a row");
        assert!(
            deploy
                .source_file
                .as_deref()
                .is_some_and(|f| f.ends_with("topos.toml")),
            "{:?}",
            deploy.source_file
        );
        assert_eq!(deploy.feed, None);
        assert_eq!(deploy.attribution.as_deref(), Some("assigned by Dana"));
        assert!(!m.rows.iter().any(|i| i.name == "notes"));
        assert!(!m.rows.iter().any(|i| i.name == "triage"));
        // The LOUD line names both counts and the exact way back.
        let loud = m
            .notes
            .iter()
            .find(|n| n.contains("not adopted here"))
            .expect("the loud note");
        assert!(loud.contains("global manifest adopts 1 bundles"), "{loud}");
        assert!(loud.contains("2 assigned bundles"), "{loud}");
        assert!(loud.contains("`topos add -g @acme`"), "{loud}");
        // The regime says the same thing in one phrase.
        assert_eq!(
            r.regimes[0].regime,
            "explicit: 1 bundles; 2 assigned not adopted here"
        );
    }

    /// `"off"` is a statement like any other: it stays a row. An off switch for a bundle nobody
    /// assigns any more is inert — that gets a note, never a silent line.
    #[test]
    fn off_rows_stand_and_a_stale_one_is_disclosed() {
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

        let r = resolve_at(&home, &cwd);
        let m = r.machine();
        let noisy = m
            .rows
            .iter()
            .find(|i| i.name == "noisy")
            .expect("the off row");
        assert!(matches!(noisy.state, StatusItemState::Off));
        assert!(!noisy.bundle, "an off switch is not a delivered bundle");
        // The off bundle is withheld from the feed's itemization (one row, not two).
        assert_eq!(m.rows.iter().filter(|i| i.name == "noisy").count(), 1);
        // The still-assigned bundle keeps flowing.
        assert!(m.rows.iter().any(|i| i.name == "deploy"));
        // The stale switch is called out by reference; the live one needs no note.
        assert!(
            m.notes
                .iter()
                .any(|n| n == "off — not currently assigned: topos.sh/acme/gone"),
            "{:?}",
            m.notes
        );
        assert!(!m.notes.iter().any(|n| n.contains("acme/noisy")));
        assert_eq!(r.regimes[0].regime, "adopting all assigned, 2 off");
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

        let r = resolve_at(&home, &cwd);
        assert!(
            r.machine()
                .notes
                .iter()
                .any(|n| n == "topos.sh/acme/deploy adds nothing — the feed already delivers it"),
            "{:?}",
            r.machine().notes
        );
        // Deduped by identity: the explicit row wins, the feed does not repeat it.
        assert_eq!(
            r.machine()
                .rows
                .iter()
                .filter(|i| i.name == "deploy")
                .count(),
            1
        );
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

        let r = resolve_at(&home, &cwd);
        assert!(
            r.machine()
                .notes
                .iter()
                .any(|n| n == "legacy: declined on the web, delivered here by your global manifest"),
            "{:?}",
            r.machine().notes
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

        let r = resolve_at(&home, &cwd);
        let regime = |ws: &str| {
            r.regimes
                .iter()
                .find(|x| x.workspace == ws)
                .map(|x| x.regime.clone())
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

        let r = resolve_at(&home, &nested);
        let p = r.project().expect("a project scope");
        assert_eq!(p.rows.len(), 1, "{:?}", p.rows.len());
        assert_eq!(p.rows[0].name, "api-only");
        assert!(
            p.manifest
                .as_deref()
                .is_some_and(|m| m.ends_with("services/api/topos.toml")),
            "{:?}",
            p.manifest
        );
        // Connected but never delivered here — the honest local answer, nothing dialed.
        assert!(matches!(p.rows[0].state, StatusItemState::Unknown));
    }

    /// `detail_for` names WHERE one skill comes from: a row spells its file and its key; a feed
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

        with_ctx(&home, Some(&cwd), |ctx| {
            let (all, cache) = read_sources(ctx).unwrap();
            let r = resolve(ctx, &all, &cache).unwrap();
            // The default view's sections: project-then-machine, precedence order.
            let sections: Vec<&ScopeResolution> = r.scopes.iter().collect();

            // The row-delivered bundle: the file + the spelled key.
            let detail = detail_for(&sections, &all, "deploy")
                .expect("the deep answer")
                .detail;
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

            // The feed-delivered one: the feed, not a file. The `@ws/name` spelling resolves too.
            let detail = detail_for(&sections, &all, "@acme/notes")
                .expect("the deep answer")
                .detail;
            assert_eq!(detail.name, "notes");
            assert_eq!(detail.source_file, None);
            assert_eq!(detail.source_key, None);
            assert_eq!(detail.feed.as_deref(), Some("topos.sh/acme"));
            assert_eq!(detail.attribution.as_deref(), Some("picked by you"));

            // A token no scope delivers is the uniform not-found.
            let err = detail_for(&sections, &all, "nowhere").expect_err("an unknown token");
            assert!(matches!(err, ClientError::TargetNotFound { .. }), "{err:?}");
        });
    }

    /// A LOCAL mcp row's config entries live in this scope's ledger and NOWHERE else — no
    /// workspace delivers the bundle, so no delivery cache can describe it. The deep answer reads
    /// them there instead of saying "no agent config entries recorded yet" about entries this very
    /// scope placed. Another bundle's entries never ride along.
    #[test]
    fn a_local_mcp_row_reads_its_agent_entries_from_the_scope_ledger() {
        use crate::mcp_ledger::{LedgerEntry, McpLedger, placement_key};

        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let dir = home.0.join("weather");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("server.json"), b"{}\n").unwrap();
        home.global(&format!(
            "[bundles]\n\"{}\" = {{ kind = \"mcp\" }}\n",
            dir.display()
        ));

        let entry = |bundle: &str, file: &str| LedgerEntry {
            bundle_id: bundle.to_owned(),
            version_id: String::new(),
            file: file.to_owned(),
            fingerprint: "fp".to_owned(),
            owns_file: false,
        };
        let mut ledger = McpLedger::default();
        ledger.entries.insert(
            placement_key("cursor", "topos-local-weather"),
            entry("local:weather", "/agents/cursor/mcp.json"),
        );
        ledger.entries.insert(
            placement_key("codex", "topos-local-weather"),
            entry("local:weather", "/agents/codex/config.toml"),
        );
        // A DIFFERENT bundle's committed entry, under the same harness.
        ledger.entries.insert(
            placement_key("cursor", "topos-local-notes"),
            entry("local:notes", "/agents/cursor/mcp.json"),
        );
        crate::mcp_ledger::write(&RealFs, &home.layout(), &ledger).unwrap();

        with_ctx(&home, Some(&cwd), |ctx| {
            let (all, cache) = read_sources(ctx).unwrap();
            let r = resolve(ctx, &all, &cache).unwrap();
            let sections: Vec<&ScopeResolution> = r.scopes.iter().collect();
            let detail = detail_for(&sections, &all, "weather")
                .expect("the deep answer")
                .detail;
            assert_eq!(detail.kind.as_deref(), Some("mcp"));
            let agents: Vec<(&str, &str, &str)> = detail
                .harnesses
                .iter()
                .map(|h| {
                    (
                        h.agent.as_str(),
                        h.state.as_str(),
                        h.file.as_deref().unwrap_or_default(),
                    )
                })
                .collect();
            assert_eq!(
                agents,
                vec![
                    ("codex", "current", "/agents/codex/config.toml"),
                    ("cursor", "current", "/agents/cursor/mcp.json"),
                ],
                "only this bundle's committed entries, each naming its file"
            );
        });
    }
}
