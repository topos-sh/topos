//! `add --mcp <name|url|path>` — bring a REMOTE MCP server into this machine's agents.
//!
//! Three spellings, one act. The flag declares the intent, and the SOURCE's shape decides which
//! door it goes through:
//!
//! - a **local folder** holding a `server.json` at its root — the dir IS the bundle. The bytes are
//!   already the person's own, so there is nothing to consent to: the gate runs, the folder is
//!   adopted exactly as `topos add ./dir` adopts one, and the row it records gains `kind = "mcp"`.
//! - an **official-registry name** (`io.github.<owner>/<server>`) or an **https URL** to a
//!   `server.json` — bytes from a source this machine has never seen. That is a NEW-SOURCE TRUST
//!   moment, so a bare run DESCRIBES (what the server is, where it points, which agents it would
//!   reach) and changes nothing; `--yes` writes the document, records the row, and converges.
//!
//! Everything both doors put in front of a person comes from [`crate::mcp_validate`] — the same
//! rules, refusal codes, and credential shapes the web tier enforces, from the same vectors at
//! `tests/fixtures/mcp/`. A document carrying a credential is refused BEFORE a byte is written,
//! so the refusal is the whole outcome and nothing lands half-trusted.
//!
//! ## What the fetched arm does NOT do
//!
//! It does not mint a version history. A fetched registry document is a POINTER, not a draft you
//! edit and publish; the person-scope destination lives under `~/.topos/`, which the adopt path
//! refuses by construction (so `uninstall` can never delete somebody's own bytes). The row plus
//! the config converge is the whole act — which is exactly what the reconcile already understands:
//! a local `kind = "mcp"` row whose dir no store tracks carries the `local:<name>` ledger
//! identity, and every later `update` re-reads the same folder. Authoring a server to SHARE with
//! the team is the local-folder door, which does adopt and therefore does publish.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use topos_types::results::{AddData, AddDescribeData, McpServerSummary};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::mcp_validate::{self, McpSummary};

use super::manifest_edit::{self as medit, EditTarget};

/// The official registry the named arm reads a server's latest version from.
const REGISTRY_BASE: &str = "https://registry.modelcontextprotocol.io";

/// The bundle folder a person-scope import writes into, under the sidecar home.
const PERSON_MCP_DIR: &str = "mcp";

/// Where the fetched-document arm gets its bytes. Production dials the network
/// ([`crate::plane_http::UreqMcpSource`]); tests hand back bytes, so no suite ever reaches the
/// real registry.
pub(crate) trait McpDocSource {
    /// Fetch the document at `url` (https, proven by the caller). The implementation caps the
    /// body at [`mcp_validate::MAX_SERVER_JSON_BYTES`] and bounds the wait.
    ///
    /// # Errors
    /// [`ClientError::RemoteFetch`] for every transport / status / size fault, with `permanent`
    /// separating what a retry can fix from what it cannot.
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ClientError>;
}

/// What `add --mcp` did — or, for a never-seen source, what it WOULD do. The same two-phase shape
/// the forge-import arm uses.
#[derive(Debug)]
pub(crate) enum AddMcpOutcome {
    /// The row is written, the bytes are in place, and the config converge has run.
    Applied(Box<AddData>),
    /// A registry name or URL this machine has not read before: what it holds and what would land.
    Described {
        data: Box<AddDescribeData>,
        yes_argv: Vec<String>,
    },
}

// =================================================================================================
// The source shape
// =================================================================================================

/// Which door a `--mcp` positional goes through. Decided by SHAPE alone, so the decision is
/// testable without a filesystem, a network, or a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpSourceShape {
    /// A folder on this machine that exists — the bytes in front of you win over any spelling.
    Path(PathBuf),
    /// An official-registry server name (`<reverse.dns>/<server>`).
    Registry(String),
    /// An https URL to a `server.json` document.
    Url(String),
    /// A workspace / channel / feed / forge reference. `--mcp` has nothing to add to one: a
    /// workspace bundle already carries its kind in the catalog.
    Reference,
    /// Nothing this flag can read.
    Unreadable,
}

/// Classify a `--mcp` positional. `host` is this machine's default manifest host (the workspace
/// reference grammar's), `exists` answers whether a path-shaped token names a real directory.
///
/// The order is deliberate: an `@`-led token is unambiguously a workspace sugar; a real folder
/// beats every shorthand; a scheme is a URL; and only then does the registry's own name grammar
/// get to claim a `<a.b>/<c>` token — except when its first segment is a server this machine talks
/// to (or `github.com`), which would make `--mcp` a way to mislabel a governed reference.
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
    // A path spelling, or any token that names a folder that is actually here.
    let looks_path = token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
        || token == "."
        || token == "..";
    if (looks_path || !token.contains("://")) && exists(Path::new(token)) {
        return McpSourceShape::Path(PathBuf::from(token));
    }
    if looks_path {
        // A path spelling that names nothing is a missing folder, not a registry name.
        return McpSourceShape::Unreadable;
    }
    if token.contains("://") {
        return if token.starts_with("https://") {
            McpSourceShape::Url(token.to_owned())
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
        return McpSourceShape::Registry(token.to_owned());
    }
    if token.contains('/') {
        // Three or more segments, or a segment the registry grammar rejects: whatever it is, it is
        // not a server name.
        return McpSourceShape::Reference;
    }
    McpSourceShape::Unreadable
}

/// The registry read endpoint for one server name. The name's slash is ONE path segment, not two,
/// so it is percent-encoded.
pub(crate) fn registry_url(name: &str) -> String {
    format!(
        "{REGISTRY_BASE}/v0.1/servers/{}/versions/latest",
        encode(name)
    )
}

/// Percent-encode everything outside the unreserved set — the registry name grammar only ever
/// needs the slash escaped, but encoding by the rule (not by the one known case) is what keeps a
/// hostile name from splicing a path.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The registry answers `{ server, _meta }` for a single server; a raw URL serves the bare
/// document. Unwrap to the SERVER document either way — that, canonicalized, is what the bundle
/// stores, so an envelope and a hand-fetched document converge on the same bytes.
pub(crate) fn unwrap_server_document(raw: &[u8]) -> Vec<u8> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return raw.to_vec(); // not JSON — hand it to the gate, which names that plainly
    };
    let inner = value.get("server").filter(|v| v.is_object());
    let document = inner.unwrap_or(&value);
    canonical_server_json(document)
}

/// The bytes an MCP bundle stores: the server document, pretty-printed with a trailing newline.
fn canonical_server_json(document: &serde_json::Value) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(document).unwrap_or_else(|_| document.to_string());
    text.push('\n');
    text.into_bytes()
}

// =================================================================================================
// The verb
// =================================================================================================

/// `topos add --mcp <name|url|path> [-g] [--yes]`.
///
/// # Errors
/// [`ClientError::McpRefused`] when the gate refuses the document (the shared, typed vocabulary);
/// [`ClientError::InvalidArgument`] for a source shape `--mcp` cannot read, or a bundle folder
/// name already taken; [`ClientError::NoManifest`] when no `topos.toml` covers this folder and
/// `-g` was not asked for; [`ClientError::RemoteFetch`] from the registry/URL read; the
/// `add`-family errors from the local adopt; a filesystem failure.
pub(crate) fn add_mcp(
    ctx: &Ctx<'_>,
    docs: Option<&dyn McpDocSource>,
    source: &str,
    global: bool,
    yes: bool,
) -> Result<AddMcpOutcome, ClientError> {
    let host = medit::manifest_host(ctx);
    let exists = |p: &Path| ctx.fs.exists(p);
    match classify_mcp_source(source, host.as_deref(), &exists) {
        McpSourceShape::Path(dir) => adopt_local(ctx, &dir, global),
        McpSourceShape::Registry(name) => {
            fetch_arm(ctx, docs, source, &registry_url(&name), global, yes)
        }
        McpSourceShape::Url(url) => fetch_arm(ctx, docs, source, &url.clone(), global, yes),
        McpSourceShape::Reference => Err(ClientError::InvalidArgument(format!(
            "`{source}` names a workspace bundle — a workspace already records what each bundle \
             is, so `topos add {source}` gets it with its kind intact; `--mcp` imports a server \
             this workspace does not have yet (a registry name like `io.github.acme/weather`, an \
             https link to its server.json, or a folder holding one)"
        ))),
        McpSourceShape::Unreadable => Err(ClientError::InvalidArgument(format!(
            "`{source}` is not something `--mcp` can read — name an official-registry server \
             (`io.github.acme/weather`), an https link to its server.json, or a local folder \
             holding one"
        ))),
    }
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
fn adopt_local(ctx: &Ctx<'_>, dir: &Path, global: bool) -> Result<AddMcpOutcome, ClientError> {
    let server = dir.join("server.json");
    if ctx.fs.read_opt(&server)?.is_none() {
        return Err(ClientError::InvalidArgument(format!(
            "{} holds no server.json at its root — an MCP bundle IS its folder, and the document \
             is what topos places",
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
    let summary = mcp_validate::validate_candidate_files(&files)?;

    let scope = medit::add_scope(ctx, global)?;
    let sctx = super::ctx_with_layout(ctx, &scope.layout);
    let mut data = super::adopt_path(&sctx, &scope.target, dir)?;
    medit::note_added_path_kind_in(ctx, &mut data, &scope.target, dir, Some("mcp"))?;
    // The durable kind marker, beside the adopted store's docs — what keeps this record
    // classifying as config-placed even if the scope's ledger is ever lost.
    if let Ok(sid) = crate::id::SkillId::parse(&data.skill_id) {
        crate::mcp_engine::write_kind_marker(&sctx, &sid);
    }
    let lines = converge_one(ctx, &scope.target, global, &data.skill_id, &data.name);
    fold_receipt(&mut data, &summary, &lines);
    Ok(AddMcpOutcome::Applied(Box::new(data)))
}

// -------------------------------------------------------------------------------------------------
// The FETCHED-DOCUMENT door
// -------------------------------------------------------------------------------------------------

/// A registry name or URL: fetch once (the read serves both the describe and the apply), gate the
/// bytes, then either describe or land them.
fn fetch_arm(
    ctx: &Ctx<'_>,
    docs: Option<&dyn McpDocSource>,
    source: &str,
    url: &str,
    global: bool,
    yes: bool,
) -> Result<AddMcpOutcome, ClientError> {
    let Some(docs) = docs else {
        return Err(ClientError::InvalidArgument(
            "importing a server by name or URL needs a network source — this run has none".into(),
        ));
    };
    // The scope resolves BEFORE the fetch, so a folder with no `topos.toml` refuses without
    // dialing anything — and the describe below never mints a project store.
    let Some(target) = medit::edit_target(ctx, global)? else {
        return Err(ClientError::NoManifest);
    };

    let _phase = crate::progress::phase(ctx.progress, &format!("fetching {source}"));
    let raw = docs.fetch(url)?;
    drop(_phase);
    let document = unwrap_server_document(&raw);
    let summary = mcp_validate::validate_server_json(&document)?;

    let slug = mcp_validate::suggested_name_for(&summary.name);
    let bundle_dir = bundle_destination(ctx, &target, global, &slug);
    // A folder already standing at the destination is never written through — with ONE resumable
    // exception: an interrupted import's own leftovers (the dir landed, the row did not). Those
    // are provably ours — no manifest row names the dir AND its `server.json` bytes equal exactly
    // the document this import would write — so the retry finishes the job instead of refusing it
    // forever. Anything else (foreign bytes, a registered row) still refuses by name, so a person
    // renames or removes deliberately instead of discovering an overwrite afterwards.
    if ctx.fs.exists(&bundle_dir) && !resumable_leftover(ctx, &target, &bundle_dir, &document) {
        return Err(ClientError::InvalidArgument(format!(
            "{} already exists — {} would be written there; move or remove it, or import the \
             folder directly (`topos add --mcp {}`)",
            bundle_dir.display(),
            summary.name,
            bundle_dir.display()
        )));
    }

    let agents = engaged_agents(ctx, &target, global);
    if !yes {
        let mut yes_argv = vec![
            "topos".to_owned(),
            "add".to_owned(),
            "--mcp".to_owned(),
            source.to_owned(),
        ];
        if global {
            yes_argv.push("-g".to_owned());
        }
        yes_argv.push("--yes".to_owned());
        return Ok(AddMcpOutcome::Described {
            data: Box::new(AddDescribeData {
                source: source.to_owned(),
                members: Vec::new(),
                manifest: target.path.display().to_string(),
                reference: row_spelling(&target, global, &bundle_dir),
                value: "{ kind = \"mcp\" }".to_owned(),
                note: None,
                mcp: Some(summarize(&summary, &bundle_dir, agents)),
            }),
            yes_argv,
        });
    }

    // ---- APPLY ----
    // The BYTES first (the row must never point at a folder that is not there), then the row, then
    // the converge — each step convergent on its own, so a failure part-way leaves a state the
    // next `update` finishes rather than a half-trusted one.
    let scope = medit::add_scope(ctx, global)?;
    ctx.fs.create_dir_all(&bundle_dir)?;
    crate::atomic::atomic_write(ctx.fs, &bundle_dir.join("server.json"), &document)?;

    let mut data = row_only_receipt(&slug);
    medit::note_added_path_kind_in(ctx, &mut data, &scope.target, &bundle_dir, Some("mcp"))?;
    let bundle_id = format!("local:{slug}");
    let lines = converge_one(ctx, &scope.target, global, &bundle_id, &slug);
    fold_receipt(&mut data, &summary, &lines);
    Ok(AddMcpOutcome::Applied(Box::new(data)))
}

/// An interrupted import's leftovers, and nothing else: the standing dir is UNREGISTERED (no row
/// in the target manifest resolves to it) and its `server.json` bytes equal the document this
/// import would write. Different bytes are somebody's material; a registered row is a live bundle
/// — both refuse exactly as before.
fn resumable_leftover(ctx: &Ctx<'_>, target: &EditTarget, dir: &Path, document: &[u8]) -> bool {
    let Ok(Some(standing)) = ctx.fs.read_opt(&dir.join("server.json")) else {
        return false;
    };
    if standing != document {
        return false;
    }
    !dir_registered(ctx, target, dir)
}

/// Whether any local-path row in the target manifest resolves to `dir` (canonicalized compare, so
/// the row's `./<slug>` spelling and the absolute destination read as the same folder).
fn dir_registered(ctx: &Ctx<'_>, target: &EditTarget, dir: &Path) -> bool {
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let Ok(Some(text)) = medit::read_text(ctx, &target.path) else {
        return false;
    };
    let Ok(doc) = crate::manifest::document::parse_manifest(&text, target.scope) else {
        return false;
    };
    doc.rows.iter().any(|row| {
        let crate::manifest::keys::KeyShape::LocalPath { raw } = &row.shape else {
            return false;
        };
        let resolved = if let Some(rest) = raw.strip_prefix("~/") {
            match ctx.roots.as_ref() {
                Some(roots) => roots.home.join(rest),
                None => return false,
            }
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            target.dir.join(raw.trim_start_matches("./"))
        };
        resolved.canonicalize().unwrap_or(resolved) == canon
    })
}

/// Where a fetched document's folder goes: under the sidecar's own `mcp/` at person scope (topos's
/// material, gone when topos is uninstalled), and beside the person at project scope — the folder
/// they stand in, so the row records the committable `./<slug>` spelling.
fn bundle_destination(ctx: &Ctx<'_>, target: &EditTarget, global: bool, slug: &str) -> PathBuf {
    if global {
        return ctx.layout.home().join(PERSON_MCP_DIR).join(slug);
    }
    ctx.roots
        .as_ref()
        .and_then(|r| r.cwd.clone())
        .unwrap_or_else(|| target.dir.clone())
        .join(slug)
}

/// The row key [`medit::note_added_path_kind_in`] will write for `bundle_dir` — computed here only
/// so the DESCRIBE can print the exact line the apply lands (the writer re-derives it itself).
fn row_spelling(target: &EditTarget, global: bool, bundle_dir: &Path) -> String {
    let dir = target
        .dir
        .canonicalize()
        .unwrap_or_else(|_| target.dir.clone());
    let abs = bundle_dir
        .canonicalize()
        .unwrap_or_else(|_| bundle_dir.to_path_buf());
    if !global && abs.starts_with(&dir) {
        return medit::path_reference(&dir, &abs);
    }
    abs.display().to_string()
}

/// The receipt shape a row-only add returns (the fetched arm mints no version history — see the
/// module docs). Same fields the reference arms fill for a feed or channel row.
fn row_only_receipt(name: &str) -> AddData {
    AddData {
        skill_id: String::new(),
        name: name.to_owned(),
        version_id: "0".repeat(64),
        bundle_digest: "0".repeat(64),
        tracked: true,
        harness: None,
        harness_slug: None,
        currency: None,
        triggers: Vec::new(),
        origin: None,
        manifest: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
        published_match: None,
        note: None,
    }
}

// -------------------------------------------------------------------------------------------------
// The converge + the receipt
// -------------------------------------------------------------------------------------------------

/// Run the edited scope's MCP config convergence for ONE bundle, right now, so the receipt names
/// the per-agent placements instead of deferring the fact to the next sweep. Removals are OFF: an
/// add converges what it just asked for and never touches another bundle's entries.
///
/// Best-effort by construction — the row already landed, and the next sweep reaches the same end
/// state; every failure becomes a receipt line rather than a failed add.
fn converge_one(
    ctx: &Ctx<'_>,
    target: &EditTarget,
    global: bool,
    bundle_id: &str,
    name: &str,
) -> Vec<String> {
    let Some(roots) = ctx.roots.clone() else {
        return Vec::new();
    };
    let (layout, project_root) = if global {
        (Some(ctx.layout.clone()), None)
    } else {
        (
            crate::sidecar::existing_project_store(ctx.fs, &target.dir),
            Some(target.dir.clone()),
        )
    };
    let Some(layout) = layout else {
        return Vec::new();
    };
    // The dir the row points at IS the bundle; read its document back from disk, so what gets
    // placed is exactly what a later `update` will read.
    let Some(dir) = row_dir_of(ctx, target, bundle_id, name) else {
        return Vec::new();
    };
    let Ok(Some(server_json)) = ctx.fs.read_opt(&dir.join("server.json")) else {
        return Vec::new();
    };
    let detected: BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &roots.home,
        project_root.as_deref().or(roots.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    let io = crate::mcp_engine::ScopeIo {
        fs: ctx.fs,
        layout: &layout,
        home: roots.home.clone(),
        project_root,
    };
    let demand = crate::mcp_engine::McpDemand {
        bundle_id: bundle_id.to_owned(),
        name: name.to_owned(),
        workspace_slug: None,
        version_id: String::new(),
        server_json,
        harness_filter: Vec::new(),
    };
    let outcome = crate::mcp_engine::converge(
        &io,
        std::slice::from_ref(&demand),
        topos_harness::mcp::descriptor::mcp_harnesses(),
        &detected,
        &HashSet::new(),
        false,
    );
    let mut lines: Vec<String> = Vec::new();
    for bundle in &outcome.bundles {
        for state in &bundle.states {
            let where_ = state.file.as_deref().unwrap_or("its config");
            lines.push(match state.state.as_str() {
                "current" => format!("{}: server entry in {where_}", state.agent),
                "drifted" => format!(
                    "{}: hand-edited entry left in place ({where_})",
                    state.agent
                ),
                "conflicting" => format!(
                    "{}: an entry topos does not own already holds that name ({where_})",
                    state.agent
                ),
                other => match &state.note {
                    Some(note) => format!("{}: {other} — {note}", state.agent),
                    None => format!("{}: {other}", state.agent),
                },
            });
        }
    }
    lines.extend(outcome.warnings.iter().cloned());
    lines
}

/// The folder the row just written points at. The adopt arm knows it from the tracked record; the
/// fetched arm's `local:<slug>` identity resolves through the row itself.
fn row_dir_of(ctx: &Ctx<'_>, target: &EditTarget, bundle_id: &str, name: &str) -> Option<PathBuf> {
    let _ = (bundle_id, name);
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

/// The MCP-capable agents this scope's converge would engage — the same predicate the engine uses
/// (the harness is detected here, OR its config file already exists), so the describe's breadth
/// line is the one the apply delivers.
fn engaged_agents(ctx: &Ctx<'_>, target: &EditTarget, global: bool) -> Vec<String> {
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
        let surface = match &project_root {
            Some(root) => h.project_surface.and_then(|(rel, _)| {
                let path = root.join(rel);
                crate::placement::within_project(root, &path).then_some(path)
            }),
            None => topos_harness::mcp::descriptor::user_surface_path(h, &roots.home),
        };
        let Some(path) = surface else { continue };
        if detected.contains(h.slug) || ctx.fs.exists(&path) {
            out.push(h.display_name.to_owned());
        }
    }
    out
}

/// The typed describe payload — derived facts only; the document is never echoed whole.
fn summarize(summary: &McpSummary, bundle_dir: &Path, agents: Vec<String>) -> McpServerSummary {
    McpServerSummary {
        server: summary.name.clone(),
        description: summary.description.clone(),
        version: summary.version.clone(),
        url: summary.url.clone(),
        transport: summary.transport.to_owned(),
        auth: summary.auth_hint.map(|a| a.as_str().to_owned()),
        headers: summary.headers.iter().map(|h| h.name.clone()).collect(),
        bundle: bundle_dir.display().to_string(),
        agents,
    }
}

/// Fold what landed into the add receipt's note: the server this row now points at, then the
/// per-agent outcomes of the converge that just ran.
fn fold_receipt(data: &mut AddData, summary: &McpSummary, lines: &[String]) {
    let auth = match summary.auth_hint {
        Some(hint) => format!(", auth {}", hint.as_str()),
        None => String::new(),
    };
    medit::push_note(
        data,
        format!(
            "MCP server {} v{} — {} over {}{auth}",
            summary.name, summary.version, summary.url, summary.transport
        ),
    );
    if !lines.is_empty() {
        medit::push_note(data, lines.join(" · "));
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

    /// A registry NAME is the two-segment reverse-DNS grammar, and nothing else.
    #[test]
    fn a_registry_name_is_read_as_one() {
        let host = Some("topos.sh");
        assert_eq!(
            classify_mcp_source("io.github.acme/weather", host, &nothing_exists),
            McpSourceShape::Registry("io.github.acme/weather".to_owned())
        );
        // Whitespace around the token is the shell's, not the person's intent.
        assert_eq!(
            classify_mcp_source("  io.github.acme/weather  ", host, &nothing_exists),
            McpSourceShape::Registry("io.github.acme/weather".to_owned())
        );
        // Three segments cannot be a server name.
        assert_eq!(
            classify_mcp_source("io.github.acme/team/weather", host, &nothing_exists),
            McpSourceShape::Reference
        );
    }

    /// An https URL is a document address; anything else with a scheme is refused rather than
    /// fetched over a transport this verb does not promise.
    #[test]
    fn only_https_urls_are_fetchable() {
        assert_eq!(
            classify_mcp_source("https://acme.example/server.json", None, &nothing_exists),
            McpSourceShape::Url("https://acme.example/server.json".to_owned())
        );
        assert_eq!(
            classify_mcp_source("http://acme.example/server.json", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
        assert_eq!(
            classify_mcp_source("file:///etc/passwd", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
    }

    /// The bytes in front of you win: a token naming a real folder is a local import however the
    /// grammar would otherwise read it.
    #[test]
    fn a_real_folder_beats_every_shorthand() {
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &everything_exists),
            McpSourceShape::Path(PathBuf::from("./servers/weather"))
        );
        // Even a registry-shaped token, when a folder of that name is actually here.
        assert_eq!(
            classify_mcp_source("io.github.acme/weather", None, &everything_exists),
            McpSourceShape::Path(PathBuf::from("io.github.acme/weather"))
        );
        // A path SPELLING that names nothing is a missing folder, never a registry name.
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
    }

    /// `--mcp` never re-labels a governed reference: the workspace sugar, this machine's own host,
    /// and github.com all refuse toward the plain `add`.
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
            McpSourceShape::Registry("topos.sh/acme".to_owned())
        );
    }

    /// A bare word names no server; so does nothing at all.
    #[test]
    fn a_bare_word_is_unreadable() {
        assert_eq!(
            classify_mcp_source("weather", Some("topos.sh"), &nothing_exists),
            McpSourceShape::Unreadable
        );
        assert_eq!(
            classify_mcp_source("", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
        assert_eq!(
            classify_mcp_source("   ", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
    }

    /// The registry read endpoint: the name's slash is ONE segment, percent-encoded.
    #[test]
    fn the_registry_url_encodes_the_name_as_a_single_segment() {
        assert_eq!(
            registry_url("io.github.acme/weather"),
            "https://registry.modelcontextprotocol.io/v0.1/servers/io.github.acme%2Fweather/versions/latest"
        );
        // The unreserved set rides through untouched; everything else is escaped.
        assert_eq!(
            registry_url("a-b.c_d~e/f"),
            "https://registry.modelcontextprotocol.io/v0.1/servers/a-b.c_d~e%2Ff/versions/latest"
        );
        // A hostile name cannot splice a path: `.` is unreserved and rides through, but every
        // separator that would traverse is escaped, so the whole name stays ONE segment.
        let hostile = registry_url("../../admin/x");
        assert!(hostile.contains("..%2F..%2Fadmin%2Fx"), "{hostile}");
        assert_eq!(
            hostile.matches('/').count(),
            // https:// (2) + /v0.1 + /servers + /<name> + /versions + /latest
            7,
            "the name must not add a path segment: {hostile}"
        );
    }

    /// The registry's `{server, _meta}` envelope and a bare document converge on the SAME stored
    /// bytes — pretty-printed with a trailing newline — so one bundle, not two.
    #[test]
    fn the_registry_envelope_and_a_bare_document_store_identical_bytes() {
        let bare = br#"{"name":"io.github.a/b","version":"1.0.0"}"#;
        let wrapped = br#"{"server":{"name":"io.github.a/b","version":"1.0.0"},"_meta":{"x":1}}"#;
        let from_bare = unwrap_server_document(bare);
        let from_wrapped = unwrap_server_document(wrapped);
        assert_eq!(from_bare, from_wrapped);
        let text = String::from_utf8(from_bare).expect("utf-8");
        assert!(text.ends_with("}\n"), "{text:?}");
        assert!(text.contains("\n  \"name\""), "{text:?}");
        assert!(
            !text.contains("_meta"),
            "the envelope is not the document: {text:?}"
        );
    }

    /// Bytes the parse cannot read are handed to the GATE untouched, so the refusal names "that is
    /// not JSON" instead of a mangled document.
    #[test]
    fn unparseable_bytes_reach_the_gate_verbatim() {
        let raw = b"name: not json\n";
        assert_eq!(unwrap_server_document(raw), raw.to_vec());
    }

    /// A `server` key that is not an object is not an envelope — the whole document stands.
    #[test]
    fn a_non_object_server_key_is_not_an_envelope() {
        let raw = br#"{"name":"io.github.a/b","server":"nope"}"#;
        let text = String::from_utf8(unwrap_server_document(raw)).expect("utf-8");
        assert!(text.contains("\"server\": \"nope\""), "{text}");
        assert!(text.contains("io.github.a/b"), "{text}");
    }
}
