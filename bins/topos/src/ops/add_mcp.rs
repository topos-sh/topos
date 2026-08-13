//! `add --kind mcp <path>` — bring an MCP server bundle on this machine into its agents' MCP
//! configs.
//!
//! ONE local door: a **folder holding a `server.json` at its root** — the dir IS the bundle. The
//! gate runs, the folder is adopted exactly as `topos add ./dir` adopts one, and the row it records
//! gains `kind = "mcp"`. A workspace bundle needs no flag at all: the catalog already records what
//! each bundle is, so `topos add <name>` gets it with its kind intact.
//!
//! topos fetches nothing here. A registry server name and an https link to a server document are
//! both recognized and REFUSED toward the covered path — the server is added to a workspace on the
//! web (that page takes a registry name, a URL, or the pasted document), and every machine then
//! adds it by the name it got there. One governed copy the team can review, revert, and keep
//! current beats a pointer each machine resolved for itself.
//!
//! Everything the door accepts comes from [`crate::mcp_validate`] — the same rules, refusal codes,
//! and credential shapes the web tier enforces, from the same vectors at `tests/fixtures/mcp/`.
//! The VALIDATION gate stands on the door: a document carrying a credential is refused BEFORE a
//! byte is written, so the refusal is the whole outcome and nothing lands half-trusted.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use topos_types::results::{AddData, McpServerSummary, TargetOutcome};

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::mcp_validate::{self, McpSummary};

use super::manifest_edit::{self as medit, EditTarget};

// =================================================================================================
// The source shape
// =================================================================================================

/// What an MCP positional IS. Decided by SHAPE alone, so the decision is testable without a
/// filesystem, a network, or a session. Only [`McpSourceShape::Path`] opens a door — the two
/// fetch-shaped spellings are recognized so their refusal can teach the covered path by name
/// instead of answering "unreadable" to a spelling topos understands perfectly well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpSourceShape {
    /// A PATH-SPELLED folder on this machine that exists (`./x`, `../x`, `~/x`, `/abs`, `.`,
    /// `..`) — the one spelling that opens the local door.
    Path(PathBuf),
    /// An official-registry server name (`<reverse.dns>/<server>`). topos does not fetch from the
    /// registry; the refusal names the workspace path.
    RegistryName,
    /// An https link to a server document. topos does not fetch one; same refusal.
    DocumentUrl,
    /// A workspace / channel / feed / forge reference. The kind flag has nothing to add to one: a
    /// workspace bundle already carries its kind in the catalog.
    Reference,
    /// Nothing this door can read.
    Unreadable,
}

/// Classify an MCP positional. `host` is this machine's default manifest host (the workspace
/// reference grammar's), `exists` answers whether a token names a real directory.
///
/// The order is deliberate: an `@`-led token is unambiguously a workspace sugar; ONLY a path
/// SPELLING opens the Path door (a bare token never silently adopts a folder); a scheme is a
/// document link; and only then does the registry's own name grammar get to claim a `<a.b>/<c>`
/// token — except when its first segment is a server this machine talks to (or `github.com`),
/// which would make the flag a way to mislabel a governed reference.
pub(crate) fn classify_mcp_source(
    source: &str,
    host: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> McpSourceShape {
    let token = source.trim();
    if token.is_empty() {
        return McpSourceShape::Unreadable;
    }
    if token.starts_with('@') || token.starts_with('#') {
        return McpSourceShape::Reference;
    }
    // ONLY a path spelling opens the Path door.
    let looks_path = token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
        || token == "."
        || token == "..";
    if looks_path {
        return if exists(Path::new(token)) {
            McpSourceShape::Path(PathBuf::from(token))
        } else {
            // A path spelling that names nothing is a missing folder, not a server name.
            McpSourceShape::Unreadable
        };
    }
    if token.contains("://") {
        return if token.starts_with("https://") {
            McpSourceShape::DocumentUrl
        } else {
            McpSourceShape::Unreadable
        };
    }
    let first = token.split('/').next().unwrap_or_default();
    let governed_host = first == "github.com" || host.is_some_and(|h| h == first);
    if governed_host {
        return McpSourceShape::Reference;
    }
    if mcp_validate::is_registry_name(token) {
        // A directory wearing the registry name no longer collides with anything — no registry
        // door stands for it to shadow, and `./<name>` is still the spelling that adopts a folder.
        return McpSourceShape::RegistryName;
    }
    if token.contains('/') {
        // Three or more segments, or a segment the registry grammar rejects: whatever it is, it is
        // not a server name.
        return McpSourceShape::Reference;
    }
    McpSourceShape::Unreadable
}

// =================================================================================================
// The verb
// =================================================================================================

/// What `add --kind mcp` answers with: the receipt, plus the delivery half's typed failures.
///
/// The two are separate because they answer separate questions. The RECEIPT says what was
/// recorded — a durable row, true whatever the configs did. The MESSAGES say what could not be
/// delivered, and they are what the exit status and `ok` are computed from: an add whose entries
/// reached no agent at all is not a success, and one that reached some is a success that still
/// has to exit non-zero (the sweep's own rule) so an agent watching a loop learns something is
/// not converging.
#[derive(Debug)]
pub(crate) struct McpAdded {
    pub data: Box<AddData>,
    /// One `failure` per surface the converge could not write.
    pub messages: Vec<topos_types::Message>,
    /// Every planned surface failed: the row landed and NOTHING was delivered.
    pub reached_nobody: bool,
}

/// `topos add --kind mcp <path> [-g]`. The one door applies immediately with an undo-led receipt;
/// `--yes` is parsed by the CLI and changes nothing here.
///
/// # Errors
/// [`ClientError::McpRefused`] when the gate refuses the document (the shared, typed vocabulary);
/// [`ClientError::InvalidArgument`] for a source this door cannot read — a registry name and a
/// document link included, each refused toward the workspace path by name; the `add`-family errors
/// from the local adopt; a filesystem failure.
pub(crate) fn add_mcp(
    ctx: &Ctx<'_>,
    source: &str,
    global: bool,
    selection: &super::dest_select::Selection,
) -> Result<McpAdded, ClientError> {
    let host = medit::manifest_host(ctx);
    let exists = |p: &Path| ctx.fs.exists(p);
    match classify_mcp_source(source, host.as_deref(), &exists) {
        McpSourceShape::Path(dir) => adopt_local(ctx, &dir, global, selection),
        // The two fetch-shaped spellings. topos reads neither: a server the team shares is added
        // to the workspace ONCE, on the web, and every machine gets the governed copy by name.
        McpSourceShape::RegistryName => Err(ClientError::InvalidArgument(format!(
            "`{source}` is a registry server name — topos does not fetch from the MCP registry; \
             add the server to your workspace on the web (\"Add an MCP server\" takes a registry \
             name, a URL, or the pasted server.json), then add it here by the name it gets there"
        ))),
        McpSourceShape::DocumentUrl => Err(ClientError::InvalidArgument(format!(
            "`{source}` links to a server document — topos does not fetch one; add the server to \
             your workspace on the web (\"Add an MCP server\" takes a URL like this one), then \
             add it here by the name it gets there"
        ))),
        McpSourceShape::Reference => Err(ClientError::InvalidArgument(format!(
            "`{source}` names a workspace bundle — a workspace already records what each bundle \
             is, so `topos add {source}` gets it with its kind intact, no `--kind` needed"
        ))),
        McpSourceShape::Unreadable => Err(ClientError::InvalidArgument(format!(
            "`{source}` is not a folder on this machine — `--kind mcp` adopts a folder whose root \
             holds server.json (`topos add --kind mcp ./weather`); a server your workspace \
             publishes needs no flag (`topos add {source}`)"
        ))),
    }
}

/// Fold the typed MCP block onto a WORKSPACE-DELIVERED bundle's add receipt — read back from the
/// scope store the subscribe just delivered into, so the receipt states the document that
/// actually landed. Best-effort by construction: the delivery itself is (the row is the durable
/// demand), so a store the bytes have not reached yet just leaves the receipt block-less and the
/// next `update` lands them. No `bundle` folder is named — a workspace bundle's bytes live in
/// the store, not in a folder of their own.
pub(crate) fn fold_workspace_mcp(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    global: bool,
    data: &mut AddData,
) {
    let Some(sid) = data
        .skill_id
        .as_deref()
        .and_then(|s| crate::id::SkillId::parse(s).ok())
    else {
        return;
    };
    let Some(layout) = super::manifest_edit::mcp_scope_store(ctx, target) else {
        return;
    };
    let sctx = super::ctx_with_layout(ctx, &layout);
    let Ok(Some((_version, bytes))) = crate::mcp_engine::stored_server_json(&sctx, &sid) else {
        return;
    };
    let Ok(summary) = mcp_validate::validate_server_json(&bytes) else {
        return;
    };
    let (filter, _) = row_narrowing(ctx, target, &data.name);
    let agents = engaged_agents(ctx, target, global, filter.as_deref());
    fold_receipt(data, &summary, None, agents, &ConvergeReceipt::default());
}

// -------------------------------------------------------------------------------------------------
// The LOCAL FOLDER door
// -------------------------------------------------------------------------------------------------

/// A folder already on this machine: gate it WHOLE, adopt it in place exactly as a plain
/// `add <path>` does, record the row WITH its kind, and converge the scope's MCP config. The gate
/// is the candidate gate, not just the document's: the exact allowed file set and a credential
/// scan over every file's bytes — a stray script beside `server.json` refuses before anything is
/// adopted. Applies immediately — an on-disk folder is the person's own material, and the row is
/// its exact inverse away.
fn adopt_local(
    ctx: &Ctx<'_>,
    dir: &Path,
    global: bool,
    selection: &super::dest_select::Selection,
) -> Result<McpAdded, ClientError> {
    let server = dir.join("server.json");
    if ctx.fs.read_opt(&server)?.is_none() {
        return Err(ClientError::InvalidArgument(format!(
            "{} holds no server.json at its root — an MCP bundle is a folder whose root holds \
             server.json",
            dir.display()
        )));
    };
    // The WHOLE folder goes through the candidate gate — the same allowlist + per-file scan the
    // publish preflight and the web tier run (a subdirectory's files carry their nested paths, so
    // the allowlist refuses them by name too).
    let scanned = crate::scan::scan(dir)?;
    let files: Vec<(&str, &[u8])> = scanned
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.bytes.as_slice()))
        .collect();
    let summary = mcp_validate::validate_candidate_files(&files)
        .map_err(|r| ClientError::mcp_refused_about(r, &dir.display().to_string()))?;

    let scope = medit::add_scope(ctx, global)?;
    // The `-a`/`--dest` selection resolves to config FILES at this scope — refused whole before
    // the adopt when an entry names no known file.
    let dest_entries = selection.mcp_entries(scope.target.scope)?;
    let mut data = super::adopt_path_any_kind(ctx, &scope, dir, BundleKind::Mcp)?;
    medit::note_added_path_kind_dest_in(
        ctx,
        &mut data,
        &scope.target,
        dir,
        Some(BundleKind::Mcp),
        &dest_entries,
    )?;
    if !dest_entries.is_empty() {
        data.dest = dest_entries;
    }
    let (filter, _) = row_narrowing(ctx, &scope.target, &data.name);
    let agents = engaged_agents(ctx, &scope.target, global, filter.as_deref());
    let bundle_id = data.skill_id.clone().unwrap_or_default();
    let converge = converge_one(ctx, &scope.target, &bundle_id, &data.name);
    fold_receipt(&mut data, &summary, Some(dir), agents, &converge);
    Ok(McpAdded {
        reached_nobody: converge.reached_nobody(),
        messages: converge.all_messages(),
        data: Box::new(data),
    })
}

// -------------------------------------------------------------------------------------------------
// The converge + the receipt
// -------------------------------------------------------------------------------------------------

/// What the add's inline converge answered — the receipt's lines, the config files it resolved,
/// and the typed FAILURES.
///
/// The failures ride the same channel the sweep's do (one `failure` per failed surface, derived
/// into the legacy `warnings` array by the finisher), because an add whose entries reached no
/// agent is not a success with a note about it: the row landed, and NOTHING was delivered. They
/// used to exist only as clauses inside one prose blob, on an `ok: true` envelope that exited 0.
#[derive(Default)]
pub(crate) struct ConvergeReceipt {
    /// The per-config receipt lines, one clause each.
    lines: Vec<String>,
    /// The config files as they RESOLVED on this machine.
    resolved: Vec<String>,
    /// One typed failure per surface that could not be written.
    messages: Vec<topos_types::Message>,
    /// The STANDING not-placed lines — a capability this build or this machine does not have, an
    /// entry topos does not own. They ride the typed channel beside the failures (an agent reading
    /// `--json` needs them) and they fail nothing: no act was attempted, so no act failed.
    advisories: Vec<topos_types::Message>,
    /// How many surfaces now hold this bundle's entry. Zero with failures standing is the state
    /// that makes the whole add a failure.
    placed: usize,
}

impl ConvergeReceipt {
    /// The add landed a row and delivered NOTHING **because something broke**: every surface it
    /// planned failed to be written. Distinct from a machine with no MCP-capable agent on it
    /// (nothing failed there — there was nothing to reach), and from a machine none of whose
    /// agents this bundle can be set up for, which is [`Self::placed_nothing`]: a condition, not a
    /// fault. This one alone decides `ok` and the exit status.
    fn reached_nobody(&self) -> bool {
        self.placed == 0 && !self.messages.is_empty()
    }

    /// Nothing of this bundle stands anywhere after the add — for ANY reason, broken or standing.
    /// What the receipt says out loud, so a person is never left to infer it from the absence of
    /// placement lines.
    fn placed_nothing(&self) -> bool {
        self.placed == 0 && !(self.messages.is_empty() && self.advisories.is_empty())
    }

    /// Every typed line, failures first — the channel the `--json` envelope carries whole.
    fn all_messages(&self) -> Vec<topos_types::Message> {
        let mut out = self.messages.clone();
        out.extend(self.advisories.iter().cloned());
        out
    }
}

/// Run the edited scope's MCP config convergence for ONE bundle, right now, so the receipt names
/// the per-agent placements instead of deferring the fact to the next sweep. Removals are OFF: an
/// add converges what it just asked for and never touches another bundle's entries.
///
/// The ROW is durable either way — the next sweep reaches the same end state — so nothing here
/// fails the add. What a failure does do is ride the typed channel and set the exit status, which
/// is the difference between "delivery is pending" and "delivery silently did not happen".
fn converge_one(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    bundle_id: &str,
    name: &str,
) -> ConvergeReceipt {
    let Some(roots) = ctx.roots.clone() else {
        return ConvergeReceipt::default();
    };
    let Some((layout, project_root)) = super::manifest_edit::mcp_scope_target(ctx, target) else {
        return ConvergeReceipt::default();
    };
    // The dir the row points at IS the bundle; read its document back from disk, so what gets
    // placed is exactly what a later `update` will read.
    let Some(dir) = row_dir_of(ctx, target, name) else {
        return ConvergeReceipt::default();
    };
    let Ok(Some(server_json)) = ctx.fs.read_opt(&dir.join("server.json")) else {
        return ConvergeReceipt::default();
    };
    let detected: BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &roots.home,
        project_root.as_deref().or(roots.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    // The SAME narrowing the sweep resolves for this row (its `dest` entries mapped to the
    // config files they name) — one shared resolution, so the add can never place into a
    // harness the next sweep would claw back. It is a PLANNER input: the plan turns it into the
    // config files this bundle's entries belong in.
    let (reach, filter_warnings) = row_narrowing(ctx, target, name);
    let io = crate::mcp_engine::ScopeIo {
        fs: ctx.fs,
        runtimes: &crate::mcp_render::PathRuntimes,
        layout: &layout,
        home: roots.home.clone(),
        project_root,
    };
    let descriptors = topos_harness::mcp::descriptor::mcp_harnesses();
    let demand = crate::mcp_engine::DemandedBundle {
        bundle_id: bundle_id.to_owned(),
        name: name.to_owned(),
        workspace_slug: None,
        version_id: String::new(),
        server_json,
        reach,
    }
    .planned(&io, &descriptors, &detected);
    let outcome = crate::mcp_engine::converge(
        &io,
        std::slice::from_ref(&demand),
        &descriptors,
        &detected,
        &HashSet::new(),
        false,
    );
    let mut out = ConvergeReceipt {
        lines: filter_warnings,
        messages: outcome.warnings,
        advisories: outcome.advisories,
        ..ConvergeReceipt::default()
    };
    for bundle in outcome.bundles {
        let mut states = bundle.states;
        super::sync_engine::prettify_state_files(ctx, &mut states);
        // The config files as they RESOLVED on this machine — the same `~`-abbreviated paths the
        // per-agent lines name. An env override moves these away from the row's own spelling, and
        // the receipt's headline has to follow the bytes.
        out.resolved
            .extend(states.iter().filter_map(|s| s.file.clone()));
        out.placed += states.iter().filter(|s| s.state.stands()).count();
        // ONE CANONICAL SENTENCE PER FACT. An agent whose outcome already has a sentence of its
        // own below — a capability gap, an entry topos does not own, a surface that would not be
        // written — is not given a short second version of it here. Two lines for one fact, in two
        // wordings and at two lengths, is what a person met when eleven surfaces failed for one
        // reason; the sentence is the one that carries the way out, so the sentence is the one
        // that stays.
        out.lines.extend(
            states
                .iter()
                .filter(|s| !bundle.spoken_for.contains(&s.agent))
                .map(agent_line),
        );
    }
    out.resolved.dedup();
    out.lines
        .extend(outcome.notices.iter().map(|n| n.text.clone()));
    // ONE CANONICAL SENTENCE PER FACT. Each typed line is said once, in the words it was written
    // in — the per-agent row above carries the short cause, and this carries the whole sentence
    // with the way out. They used to be joined into one blob behind semicolons on a
    // reached-nobody add, which fused reasons that had nothing to do with each other and closed
    // with a `Run 'topos update'` that could not fix any of them; each sentence carries its own
    // remedy, or honestly carries none.
    for line in out.messages.iter().chain(&out.advisories) {
        if !out.lines.contains(&line.text) {
            out.lines.push(line.text.clone());
        }
    }
    out
}

/// One config outcome as an add-receipt line, KEYED BY THE CONFIG FILE the entry landed in —
/// receipts speak in destinations, never agents; a state with no file to name (not supported,
/// unprovable) keys by the agent slug, the only place it exists. The reload/sign-in note stays —
/// teaching text about how the change goes live in that harness. Phrases come from the ONE
/// shared vocabulary ([`TargetOutcome::word`]) — the same words `update` prints, letter for
/// letter, so a person meets one name per outcome and not two.
fn agent_line(state: &topos_types::results::McpAgentState) -> String {
    let where_ = state.file.as_deref().unwrap_or("its config");
    match state.state {
        // A created/refreshed/current entry carries the harness's reload note (how the change
        // goes live — "restart Cursor", "sign in with /mcp"), exactly as the update path's
        // receipt does. All three are a server entry sitting in that file, which is what this
        // receipt reports — they differ only in whether THIS run wrote it, and an `add` receipt
        // has already said that it did.
        TargetOutcome::Created | TargetOutcome::Refreshed | TargetOutcome::Current => {
            match &state.note {
                Some(note) => format!("{where_}: server entry — {note}"),
                None => format!("{where_}: server entry"),
            }
        }
        TargetOutcome::Drifted => format!("{where_}: hand-edited entry left in place"),
        // The cause, not a guess at it: an entry holding the NAME and an entry pointing at the
        // same SERVER under another name are different discoveries, and this line called both a
        // name clash — sending a reader to look for a name that was never there.
        TargetOutcome::Conflicting => match &state.note {
            Some(note) => format!("{where_}: {note}"),
            None => format!("{where_}: an entry topos does not manage already stands here"),
        },
        other => match &state.note {
            Some(note) => format!(
                "{}: {} — {note}",
                state.file.as_deref().unwrap_or(&state.agent),
                other.word()
            ),
            None => format!(
                "{}: {}",
                state.file.as_deref().unwrap_or(&state.agent),
                other.word()
            ),
        },
    }
}

/// The folder the row just written points at, found by matching the row's leaf against `name`
/// under the file that was just edited. There is one arm now — the local adopt — so this reads the
/// manifest rather than a record: the row is the thing that was definitely written.
fn row_dir_of(ctx: &Ctx<'_>, target: &EditTarget, name: &str) -> Option<PathBuf> {
    // The row-write stamped `data.reference`, but the caller has already moved on; re-resolve the
    // one local-path row whose leaf matches, under the file that was just edited.
    let text = medit::read_text(ctx, &target.path).ok()??;
    let doc = crate::manifest::document::parse_manifest(&text, target.scope).ok()?;
    let mut best: Option<PathBuf> = None;
    for row in &doc.rows {
        let crate::manifest::keys::KeyShape::LocalPath { raw } = &row.shape else {
            continue;
        };
        let dir = if let Some(rest) = raw.strip_prefix("~/") {
            ctx.roots.as_ref()?.home.join(rest)
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            target.dir.join(raw.trim_start_matches("./"))
        };
        if dir.file_name().map(|n| n.to_string_lossy().into_owned()) == Some(name.to_owned()) {
            best = Some(dir);
        }
    }
    best
}

/// The SAME dest-file narrowing the sweep would resolve for this scope's row named `name`: the
/// row's own `dest = [...]` (when the row is already written), each entry mapped to the harness
/// whose config file it names — through the one shared resolution
/// ([`super::reconcile::mcp_dest_narrowing`]), warnings and all. A manifest that cannot be read
/// narrows nothing (the converge still runs; the next sweep re-resolves).
fn row_narrowing(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    name: &str,
) -> (Option<Vec<String>>, Vec<String>) {
    let Ok(Some(text)) = medit::read_text(ctx, &target.path) else {
        return (None, Vec::new());
    };
    let Ok(doc) = crate::manifest::document::parse_manifest(&text, target.scope) else {
        return (None, Vec::new());
    };
    let row_dest = doc.rows.iter().find_map(|row| {
        let crate::manifest::keys::KeyShape::LocalPath { raw } = &row.shape else {
            return None;
        };
        (Path::new(raw).file_name().is_some_and(|n| n == name))
            .then(|| match &row.value {
                crate::manifest::document::EntryValue::Fields(f) => f.dest.clone(),
                _ => None,
            })
            .flatten()
    });
    let narrowing = super::reconcile::mcp_dest_narrowing(row_dest, target.scope);
    // A row whose dest names SOME unknown file still delivers to the rest; one that maps NOTHING
    // delivers nowhere, and the line says which it is — the same split the sweep reports.
    let warnings = if narrowing.unknown.is_empty() {
        Vec::new()
    } else if narrowing.reaches_nothing() {
        vec![format!(
            "{}: \"{name}\" reaches no agent — {}",
            target.path.display(),
            crate::manifest::dest::dest_names_no_mcp_file(&narrowing.unknown, target.scope)
        )]
    } else {
        narrowing
            .unknown
            .iter()
            .map(|entry| {
                format!(
                    "{}: \"{name}\" — {}",
                    target.path.display(),
                    crate::manifest::dest::unknown_mcp_file(entry, target.scope)
                )
            })
            .collect()
    };
    (narrowing.filter, warnings)
}

/// The MCP-capable agents this scope's converge would engage — the same predicate the engine uses
/// (the harness is detected here, OR its config file already exists), narrowed by the same
/// `filter` the demand carries — so the receipt's breadth line is the one the converge delivers.
fn engaged_agents(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    global: bool,
    filter: Option<&[String]>,
) -> Vec<String> {
    let Some(roots) = ctx.roots.clone() else {
        return Vec::new();
    };
    let project_root = (!global).then(|| target.dir.clone());
    let detected: BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &roots.home,
        project_root.as_deref().or(roots.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    let mut out = Vec::new();
    for h in topos_harness::mcp::descriptor::mcp_harnesses() {
        if filter.is_some_and(|f| !f.iter().any(|s| s == h.slug)) {
            continue;
        }
        let surface = match &project_root {
            Some(root) => h.mcp().and_then(|m| m.project).and_then(|(rel, _)| {
                let path = root.join(rel);
                crate::placement::within_project(root, &path).then_some(path)
            }),
            None => h.mcp_user_path(&roots.home),
        };
        let Some(path) = surface else { continue };
        if detected.contains(h.slug) || ctx.fs.exists(&path) {
            out.push(h.display_name.to_owned());
        }
    }
    out
}

/// The typed receipt payload — derived facts only; the document is never echoed whole.
/// `bundle_dir` is the folder this add adopted; `None` for a workspace-delivered
/// bundle, whose bytes live in the scope store rather than a folder here.
fn summarize(
    summary: &McpSummary,
    bundle_dir: Option<&Path>,
    agents: Vec<String>,
) -> McpServerSummary {
    McpServerSummary {
        server: summary.name.clone(),
        description: summary.description.clone(),
        version: summary.version.clone(),
        url: summary.url.clone(),
        transport: summary.transport.to_owned(),
        packages: summary
            .packages
            .iter()
            .map(|p| topos_types::results::McpPackageSummary {
                registry: p.registry.clone(),
                identifier: p.identifier.clone(),
                version: p.version.clone(),
            })
            .collect(),
        auth: summary.auth_hint.map(|a| a.as_str().to_owned()),
        headers: summary.headers.iter().map(|h| h.name.clone()).collect(),
        bundle: bundle_dir.map(|d| d.display().to_string()),
        agents,
    }
}

/// The one line that says WHAT this server is. A bundle offers an address every machine dials, a
/// package every machine runs, or both — and the sentence names the one an agent here will
/// actually use, because that is the fact a person is checking. (Both, in full, ride the typed
/// block beside it.)
///
/// Which PACKAGE it names is the placement's own choice, not the document's order
/// ([`crate::mcp_render::preferred_package`]): the selection prefers npm over pypi and passes over
/// a package no entry could express, so a receipt reading off the first listed one would name a
/// package no agent was given. A document offering nothing this build can run falls back to the
/// first — there is no chosen one to name, and the per-agent lines beside this say why.
fn what_it_is(summary: &McpSummary) -> String {
    if !summary.url.is_empty() {
        return format!("{} over {}", summary.url, summary.transport);
    }
    match crate::mcp_render::preferred_package(&summary.packages)
        .or_else(|| summary.packages.first())
    {
        // An identifier that already PINS is the whole coordinate: a container image names its
        // digest inside the identifier (`ghcr.io/acme/x@sha256:8ba1…`), and the document still
        // carries a `version` beside it for people to read. Appending that produced
        // `…@sha256:8ba1…@1.0.0`, which is not a thing anyone can paste anywhere.
        //
        // The `@` must be an INNER one. An npm scope leads with the same character
        // (`@acme/weather-mcp`) and pins nothing, so a naive "contains an @" test dropped the
        // version off every scoped npm package on the way to fixing the digests.
        Some(package)
            if package.version.is_empty()
                || package.identifier.trim_start_matches('@').contains('@') =>
        {
            format!(
                "the {} package {}, run on this machine",
                package.registry, package.identifier
            )
        }
        Some(package) => format!(
            "the {} package {}@{}, run on this machine",
            package.registry, package.identifier, package.version
        ),
        None => "nothing an agent here can run".to_owned(),
    }
}

/// Fold what landed into the add receipt: the typed `mcp` block (the same derived facts the
/// two-phase describe used to carry), then the prose note — the server this row now points at
/// and the per-agent outcomes of the converge that just ran. A converge that engaged NO agent
/// says so honestly instead of leaving a receipt that names nothing.
fn fold_receipt(
    data: &mut AddData,
    summary: &McpSummary,
    bundle_dir: Option<&Path>,
    agents: Vec<String>,
    converge: &ConvergeReceipt,
) {
    let nobody = agents.is_empty();
    let (lines, resolved) = (&converge.lines, &converge.resolved);
    // Only when it actually MOVED. The field's whole meaning is "your row says one thing and this
    // machine put it somewhere else", so it needs a row spelling to differ FROM: on a bare add
    // there is no `dest`, every resolved file trivially differs from the empty list, and the guard
    // let the receipt claim a move that the row never proposed. No `dest` ⇒ nothing to contradict.
    if !data.dest.is_empty() && !resolved.is_empty() && *resolved != data.dest {
        data.dest_resolved = resolved.clone();
    }
    data.mcp = Some(summarize(summary, bundle_dir, agents));
    let auth = match summary.auth_hint {
        Some(hint) => format!(", auth {}", hint.as_str()),
        None => String::new(),
    };
    // EACH FACT ON ITS OWN LINE. These are sentences, not asides, and the `·` fold that suits
    // two short row notes ran the server summary, the reached-nobody statement and the first
    // per-config line together into one paragraph while the other fourteen config lines sat
    // underneath it, one to a line.
    medit::push_note_line(
        data,
        format!(
            "MCP server {} v{} — {}{auth}",
            summary.name,
            summary.version,
            what_it_is(summary)
        ),
    );
    if converge.placed_nothing() {
        // NOTHING stands anywhere. The row is real, so the receipt says the one thing a reader
        // cannot see from the lines below — and then lets each line say its own reason and its own
        // way out. It used to fuse every reason behind semicolons and close with `Run 'topos
        // update' to finish the delivery`, which is a cure for exactly one of them: a machine
        // missing node finishes nothing by re-running, and told to re-run it does, forever.
        medit::push_note_line(
            data,
            format!("\"{}\" was recorded, and it reached no agent.", data.name),
        );
    }
    if !lines.is_empty() {
        // ONE CLAUSE PER LINE — surfaces differ here, so each says its own thing.
        medit::push_note_line(data, lines.join("\n"));
    }
    if nobody {
        medit::push_note_line(
            data,
            "No MCP-capable agent is set up here yet — the row waits for one",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing_exists(_: &Path) -> bool {
        false
    }
    fn everything_exists(_: &Path) -> bool {
        true
    }

    /// **The receipt names the package the placement takes, not the first one listed.** The
    /// selection prefers npm over pypi and passes over a package no entry can express; a line
    /// reading off the document's order would name a package no agent here was given.
    #[test]
    fn the_receipt_names_the_package_the_placement_would_take() {
        let package =
            |registry: &str, identifier: &str, transport: &str| crate::mcp_validate::McpPackage {
                registry: registry.to_owned(),
                identifier: identifier.to_owned(),
                version: "1.2.3".to_owned(),
                transport: transport.to_owned(),
            };
        let summary = |packages: Vec<crate::mcp_validate::McpPackage>| McpSummary {
            name: "io.github.acme/x".to_owned(),
            description: "d".to_owned(),
            version: "1.0.0".to_owned(),
            url: String::new(),
            transport: "",
            headers: Vec::new(),
            auth_hint: None,
            packages,
        };

        // pypi listed first, npm second: the placement takes npm, so the line says npm.
        assert_eq!(
            what_it_is(&summary(vec![
                package("pypi", "acme-x", "stdio"),
                package("npm", "@acme/x", "stdio"),
            ])),
            "the npm package @acme/x@1.2.3, run on this machine"
        );
        // A registry with no arm listed first is passed over the same way.
        assert_eq!(
            what_it_is(&summary(vec![
                package("nuget", "Acme.X", "stdio"),
                package("pypi", "acme-x", "stdio"),
            ])),
            "the pypi package acme-x@1.2.3, run on this machine"
        );
        // …and so is an npm package served over http, which no entry can express.
        assert_eq!(
            what_it_is(&summary(vec![
                package("npm", "@acme/served", "streamable-http"),
                package("pypi", "acme-x", "stdio"),
            ])),
            "the pypi package acme-x@1.2.3, run on this machine"
        );
        // Nothing this build can run: the first one, since there is no chosen one to name.
        assert_eq!(
            what_it_is(&summary(vec![package("nuget", "Acme.X", "stdio")])),
            "the nuget package Acme.X@1.2.3, run on this machine"
        );
        // AN IDENTIFIER THAT ALREADY PINS IS THE WHOLE COORDINATE. A container image carries its
        // digest inside the identifier and the document still states a `version` beside it for
        // people to read; appending that produced `…@sha256:8ba1…@1.2.3`, which is not a thing
        // anyone can paste anywhere. The `@` that leads an npm scope pins nothing, and the
        // scoped packages above prove it still keeps its version.
        assert_eq!(
            what_it_is(&summary(vec![package(
                "oci",
                "ghcr.io/acme/x@sha256:8ba1",
                "stdio"
            )])),
            "the oci package ghcr.io/acme/x@sha256:8ba1, run on this machine"
        );
        // An address outranks every package — it is what the agent will actually use.
        let mut dialed = summary(vec![package("npm", "@acme/x", "stdio")]);
        dialed.url = "https://mcp.example/x".to_owned();
        dialed.transport = "streamable-http";
        assert_eq!(
            what_it_is(&dialed),
            "https://mcp.example/x over streamable-http"
        );
    }

    /// ONLY a path spelling opens the door. A bare token never silently adopts a folder, and a
    /// path spelling that names nothing is a missing folder rather than a server name.
    #[test]
    fn only_a_path_spelling_opens_the_path_door() {
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &everything_exists),
            McpSourceShape::Path(PathBuf::from("./servers/weather"))
        );
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
        assert_eq!(
            classify_mcp_source("weather", None, &everything_exists),
            McpSourceShape::Unreadable
        );
    }

    /// A registry-shaped name is RECOGNIZED so its refusal can teach the workspace path — and it
    /// is recognized identically whether or not a folder of that name happens to stand here: no
    /// registry door remains for a directory to shadow.
    #[test]
    fn a_registry_name_is_recognized_so_the_refusal_can_teach() {
        let host = Some("topos.sh");
        for exists in [
            &nothing_exists as &dyn Fn(&Path) -> bool,
            &everything_exists,
        ] {
            assert_eq!(
                classify_mcp_source("io.github.acme/weather", host, exists),
                McpSourceShape::RegistryName
            );
            // Whitespace around the token is the shell's, not the person's intent.
            assert_eq!(
                classify_mcp_source("  io.github.acme/weather  ", host, exists),
                McpSourceShape::RegistryName
            );
        }
        // Three segments cannot be a server name.
        assert_eq!(
            classify_mcp_source("io.github.acme/team/weather", host, &nothing_exists),
            McpSourceShape::Reference
        );
    }

    /// An https link is recognized for the same reason; anything else with a scheme is a spelling
    /// this door has nothing to say about.
    #[test]
    fn an_https_link_is_recognized_and_other_schemes_are_not() {
        assert_eq!(
            classify_mcp_source("https://acme.example/server.json", None, &nothing_exists),
            McpSourceShape::DocumentUrl
        );
        for token in ["http://acme.example/server.json", "file:///etc/passwd"] {
            assert_eq!(
                classify_mcp_source(token, None, &nothing_exists),
                McpSourceShape::Unreadable,
                "{token}"
            );
        }
    }

    /// The kind flag never re-labels a governed reference: the workspace sugar, this machine's own
    /// host, and github.com all refuse toward the plain `add`.
    #[test]
    fn workspace_and_forge_references_refuse() {
        for token in [
            "@acme/weather",
            "@acme",
            "topos.sh/acme",
            "topos.sh/acme/weather",
            "github.com/acme/servers",
        ] {
            assert_eq!(
                classify_mcp_source(token, Some("topos.sh"), &nothing_exists),
                McpSourceShape::Reference,
                "{token}"
            );
        }
        // The host check is against THIS machine's default host — an unrelated two-segment token
        // is still a registry name.
        assert_eq!(
            classify_mcp_source("topos.sh/acme", Some("example.com"), &nothing_exists),
            McpSourceShape::RegistryName
        );
    }

    /// A bare word names no folder; so does nothing at all.
    #[test]
    fn a_bare_word_is_unreadable() {
        for token in ["weather", "", "   "] {
            assert_eq!(
                classify_mcp_source(token, Some("topos.sh"), &nothing_exists),
                McpSourceShape::Unreadable,
                "{token:?}"
            );
        }
    }

    /// ONE STATE VOCABULARY. A state that landed nowhere has no file to name, so `add` says about
    /// it exactly what `update` says — the same sentence, not a second spelling of it. The raw
    /// engine token (`not-supported`) is never what a person is handed.
    #[test]
    fn add_and_update_name_a_withheld_state_identically() {
        let withheld = topos_types::results::McpAgentState {
            agent: "openclaw".to_owned(),
            state: topos_types::results::TargetOutcome::Withheld,
            note: Some("no project-level config".to_owned()),
            file: None,
        };
        assert_eq!(
            agent_line(&withheld),
            crate::render::mcp_agent_line(&withheld, false)
        );
        assert_eq!(
            agent_line(&withheld),
            "openclaw: not placed — no project-level config"
        );

        // Noteless, over every outcome that lands in NO FILE — the class this test exists for.
        // (`add` speaks about the ONE server it just landed, so its lines for a placed entry say
        // "server entry" where `update`'s say the outcome word; that difference is deliberate and
        // is not what this pins.) The set is CLOSED now — a free-form state string can no longer
        // reach a receipt at all — so there is no "unknown token" case left to pin either.
        for state in [
            TargetOutcome::Unprovable,
            TargetOutcome::Removed,
            TargetOutcome::Withheld,
        ] {
            let s = topos_types::results::McpAgentState {
                agent: "openclaw".to_owned(),
                state,
                note: None,
                file: None,
            };
            assert_eq!(
                agent_line(&s),
                crate::render::mcp_agent_line(&s, false),
                "{state:?}"
            );
        }
    }
}
