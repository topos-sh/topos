//! `add --mcp <name|url|path>` — bring a REMOTE MCP server into this machine's agents.
//!
//! Three spellings, one act. The flag declares the intent, and the SOURCE's shape decides which
//! door it goes through:
//!
//! - a **local folder** holding a `server.json` at its root — the dir IS the bundle. The gate
//!   runs, the folder is adopted exactly as `topos add ./dir` adopts one, and the row it records
//!   gains `kind = "mcp"`.
//! - a **registry-shaped name** (`io.github.<owner>/<server>`) — resolved WORKSPACE-FIRST: the
//!   catalogs of the workspaces this machine is connected to are consulted before the official
//!   registry, on each bundle's EMBEDDED server name. Exactly one workspace publishing it
//!   subscribes to that bundle by its catalog name — the same act as `topos add <catalog-name>`
//!   on a workspace mcp bundle, ungated, with the source disclosed on the receipt; several
//!   refuse toward `--workspace`; none falls through to the registry.
//! - an **official-registry name** no workspace publishes, or an **https URL** to a
//!   `server.json` — fetched, gated, and APPLIED immediately: the canonical document is written
//!   out as a bundle folder, the row is recorded, the scope's configs converge, and the receipt
//!   LEADS with the undo (`topos remove <ref>`), which restores the whole prior state — row,
//!   config entries, and the folder this import itself wrote (see [`McpImports`]). `--yes` stays
//!   an accepted no-op on every door.
//!
//! Everything both doors accept comes from [`crate::mcp_validate`] — the same rules, refusal
//! codes, and credential shapes the web tier enforces, from the same vectors at
//! `tests/fixtures/mcp/`. The VALIDATION gate stands on every door: a document carrying a
//! credential is refused BEFORE a byte is written, so the refusal is the whole outcome and
//! nothing lands half-trusted.
//!
//! ## What the fetched arm does NOT do
//!
//! It does not mint a version history. A fetched registry document is a POINTER, not a draft you
//! edit and publish; the person-scope destination lives under `~/.topos/`, which the adopt path
//! refuses by construction (so `uninstall` can never delete somebody's own bytes). The row plus
//! the config converge is the whole act — which is exactly what the reconcile already understands:
//! a local `kind = "mcp"` row whose dir no store tracks carries the `local:<name>` custody
//! identity, and every later `update` re-reads the same folder. Authoring a server to SHARE with
//! the team is the local-folder door, which does adopt and therefore does publish.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use topos_types::results::{AddData, McpServerSummary};

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::mcp_validate::{self, McpSummary};
use crate::sidecar::Layout;

use super::add::PublishedName;
use super::manifest_edit::{self as medit, EditTarget};
use super::reconcile::SessionConnect;
use super::reference::{AddRefOutcome, add_reference};

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

// =================================================================================================
// The source shape
// =================================================================================================

/// Which door a `--mcp` positional goes through. Decided by SHAPE alone, so the decision is
/// testable without a filesystem, a network, or a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpSourceShape {
    /// A PATH-SPELLED folder on this machine that exists (`./x`, `../x`, `~/x`, `/abs`, `.`,
    /// `..`) — the one spelling that opens the local door.
    Path(PathBuf),
    /// An official-registry server name (`<reverse.dns>/<server>`).
    Registry(String),
    /// An https URL to a `server.json` document.
    Url(String),
    /// A registry-shaped token that ALSO names a real directory here: refused, naming both
    /// spellings — a folder must never silently turn a registry import into a local adopt.
    DirShadowsRegistry(String),
    /// A workspace / channel / feed / forge reference. `--mcp` has nothing to add to one: a
    /// workspace bundle already carries its kind in the catalog.
    Reference,
    /// Nothing this flag can read.
    Unreadable,
}

/// Classify a `--mcp` positional. `host` is this machine's default manifest host (the workspace
/// reference grammar's), `exists` answers whether a token names a real directory.
///
/// The order is deliberate: an `@`-led token is unambiguously a workspace sugar; ONLY a path
/// SPELLING opens the Path door (a bare token never silently adopts a folder — a directory
/// literally named like a registry server must not hijack the import); a scheme is a URL; and
/// only then does the registry's own name grammar get to claim a `<a.b>/<c>` token — except when
/// its first segment is a server this machine talks to (or `github.com`), which would make
/// `--mcp` a way to mislabel a governed reference. A registry-shaped token that ALSO names a real
/// directory refuses toward both explicit spellings instead of picking one.
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
            // A path spelling that names nothing is a missing folder, not a registry name.
            McpSourceShape::Unreadable
        };
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
        // A real directory wearing the registry name cannot silently claim the import — the
        // refusal names the `./` spelling for the folder and keeps the bare token the registry's.
        return if exists(Path::new(token)) {
            McpSourceShape::DirShadowsRegistry(token.to_owned())
        } else {
            McpSourceShape::Registry(token.to_owned())
        };
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

/// `topos add --mcp <name|url|path> [-g]`. Every door applies immediately with an undo-led
/// receipt; `--yes` is parsed by the CLI and changes nothing here. A registry-shaped name
/// resolves workspace-first (see the module docs); `workspace` is the invocation's global
/// `--workspace` selector (an id), narrowing that probe to one workspace.
///
/// # Errors
/// [`ClientError::McpRefused`] when the gate refuses the document (the shared, typed vocabulary);
/// [`ClientError::InvalidArgument`] for a source shape `--mcp` cannot read, or a bundle folder
/// name already taken; [`ClientError::NoManifest`] when no `topos.toml` covers this folder and
/// `-g` was not asked for; [`ClientError::AmbiguousMcpWorkspace`] when several connected
/// workspaces publish the embedded name; [`ClientError::RemoteFetch`] from the registry/URL
/// read; the `add`-family errors from the local adopt; a filesystem failure.
pub(crate) fn add_mcp(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    docs: Option<&dyn McpDocSource>,
    source: &str,
    global: bool,
    workspace: Option<&str>,
    selection: &super::dest_select::Selection,
) -> Result<Box<AddData>, ClientError> {
    let host = medit::manifest_host(ctx);
    let exists = |p: &Path| ctx.fs.exists(p);
    match classify_mcp_source(source, host.as_deref(), &exists) {
        McpSourceShape::Path(dir) => adopt_local(ctx, &dir, global, selection),
        McpSourceShape::Registry(name) => {
            // The scope resolves BEFORE any network dial — the same rule the fetch arm enforces
            // internally — so a folder with no `topos.toml` refuses without consulting a
            // catalog or the registry.
            if medit::edit_target(ctx, global)?.is_none() {
                return Err(ClientError::NoManifest);
            }
            // WORKSPACE-FIRST, absolutely: a workspace hit wins and the registry is never
            // dialed after one — the team's copy is governed, delivered, and kept current,
            // which is everything the raw registry pointer is not.
            let hits = workspace_server_matches(ctx, connect, &name, workspace);
            match hits.as_slice() {
                [] => fetch_arm(ctx, docs, source, &registry_url(&name), global, selection)
                    .map_err(|e| miss_both_sources(e, &name)),
                [one] => subscribe_workspace_hit(ctx, connect, one, global, selection),
                several => Err(ClientError::AmbiguousMcpWorkspace {
                    server: name.clone(),
                    workspaces: several.iter().map(|p| p.workspace.clone()).collect(),
                    global,
                }),
            }
        }
        McpSourceShape::Url(url) => fetch_arm(ctx, docs, source, &url.clone(), global, selection),
        McpSourceShape::DirShadowsRegistry(token) => Err(ClientError::InvalidArgument(format!(
            "`{token}` is both a folder here and an official-registry server name — say which: \
             `topos add --mcp ./{token}` imports the folder; the bare `{token}` is the registry \
             spelling, refused while the folder stands (move or rename it to fetch from the \
             registry)"
        ))),
        McpSourceShape::Reference => Err(ClientError::InvalidArgument(format!(
            "`{source}` names a workspace bundle — a workspace already records what each bundle \
             is, so `topos add {source}` gets it with its kind intact; `--mcp` imports a server \
             this workspace does not have yet (a registry name like `io.github.acme/weather`, an \
             https link to its server.json, or a folder holding one)"
        ))),
        McpSourceShape::Unreadable => Err(ClientError::InvalidArgument(format!(
            "`{source}` is not something `--mcp` can read — name an official-registry server \
             (`io.github.acme/weather`), an https link to its server.json, or a local folder \
             holding one; if a workspace you are connected to already publishes it, plain `topos \
             add {source}` gets it with its kind intact"
        ))),
    }
}

// -------------------------------------------------------------------------------------------------
// The WORKSPACE-FIRST resolution of a registry-shaped name
// -------------------------------------------------------------------------------------------------

/// Every connected workspace whose catalog holds an ACTIVE `kind = "mcp"` bundle EMBEDDING
/// `server` — resolved live (the offline delivery cache knows catalog names, never embedded
/// ones). A session that does not answer is SKIPPED, silently — the same hint-source doctrine as
/// the bare-name probe: folding a transport fault into "no workspace has it" would turn an
/// unreachable server into a not-found, and the registry arm still answers honestly.
/// `workspace` is the invocation's `--workspace` selector (an id): set, only that session's
/// catalog is asked.
fn workspace_server_matches(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    server: &str,
    workspace: Option<&str>,
) -> Vec<PublishedName> {
    let mut found = Vec::new();
    for s in super::add::active_sessions(ctx, workspace) {
        let transports = connect(&s);
        let Ok(index) = transports.directory.skills_index(&s.workspace_id) else {
            continue;
        };
        for e in index.skills {
            // The KIND guard: only an `mcp` bundle participates — a skill whose catalog name
            // happens to look registry-shaped never matches (and a well-behaved server never
            // stamps the embedded field on one; both ends hold the line).
            if BundleKind::parse(&e.kind) != Some(BundleKind::Mcp)
                || e.status != "active"
                || e.mcp_server_name.as_deref() != Some(server)
            {
                continue;
            }
            if let Some(p) = PublishedName::spelled(
                &s.host,
                &s.workspace_name,
                &e.name,
                Some(e.bundle_digest.clone()),
            ) {
                found.push(p);
            }
        }
    }
    // Sorted by the spelling the refusal prints, so it reads the same on every machine.
    found.sort_by(|a, b| a.reference.cmp(&b.reference));
    found
}

/// ONE workspace publishes the embedded name: subscribe to that bundle by its catalog name —
/// exactly the act `topos add <catalog-name>` performs on a workspace mcp bundle (the canonical
/// reference row, the kind from the catalog, the delivery + converge in the same invocation,
/// ungated) — and LEAD the receipt's note with the source, so the workspace-over-registry
/// precedence is disclosed rather than silent.
fn subscribe_workspace_hit(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    hit: &PublishedName,
    global: bool,
    selection: &super::dest_select::Selection,
) -> Result<Box<AddData>, ClientError> {
    match add_reference(ctx, connect, None, &hit.reference, global, true, selection)? {
        AddRefOutcome::Applied(mut data) => {
            let disclosure = format!("from {}'s catalog as '{}'", hit.workspace, hit.name);
            data.note = Some(match data.note.take() {
                Some(rest) => format!("{disclosure} · {rest}"),
                None => disclosure,
            });
            Ok(data)
        }
        // A workspace-bundle reference always applies (only a git source describes) — reaching
        // here means the reference arm changed shape under this caller.
        AddRefOutcome::Described { .. } => Err(ClientError::Corrupt(
            "a workspace bundle reference described instead of applying".into(),
        )),
    }
}

/// The registry's 404 with the OTHER consulted source folded in — one honest sentence: the
/// workspace catalogs were asked first and held nothing, so the miss says both. Every other
/// fetch fault passes through untouched (a transport error is not a consultation).
fn miss_both_sources(e: ClientError, server: &str) -> ClientError {
    match e {
        ClientError::RemoteFetch { msg, fault } if msg.ends_with("(HTTP 404)") => {
            ClientError::RemoteFetch {
                msg: format!(
                    "{msg}, and no workspace you are connected to publishes a server named \
                     {server}"
                ),
                fault,
            }
        }
        other => other,
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
    fold_receipt(data, &summary, None, agents, &[]);
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
) -> Result<Box<AddData>, ClientError> {
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
    let summary = mcp_validate::validate_candidate_files(&files)?;

    let scope = medit::add_scope(ctx, global)?;
    // The `-a`/`--dest` selection resolves to config FILES at this scope — refused whole before
    // the adopt when an entry names no known file.
    let dest_entries = selection.mcp_entries(scope.target.scope)?;
    let sctx = super::ctx_with_layout(ctx, &scope.layout);
    let mut data = super::adopt_path_any_kind(&sctx, &scope.target, dir, BundleKind::Mcp)?;
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
    let lines = converge_one(ctx, &scope.target, &bundle_id, &data.name);
    fold_receipt(&mut data, &summary, Some(dir), agents, &lines);
    Ok(Box::new(data))
}

// -------------------------------------------------------------------------------------------------
// The FETCHED-DOCUMENT door
// -------------------------------------------------------------------------------------------------

/// A registry name or URL: fetch, gate the bytes, and LAND them — the row plus the converge, with
/// the undo leading the receipt.
fn fetch_arm(
    ctx: &Ctx<'_>,
    docs: Option<&dyn McpDocSource>,
    source: &str,
    url: &str,
    global: bool,
    selection: &super::dest_select::Selection,
) -> Result<Box<AddData>, ClientError> {
    let Some(docs) = docs else {
        return Err(ClientError::InvalidArgument(
            "importing a server by name or URL needs a network source — this run has none".into(),
        ));
    };
    // The scope resolves BEFORE the fetch, so a folder with no `topos.toml` refuses without
    // dialing anything — and a refused scope never mints a project store.
    let Some(target) = medit::edit_target(ctx, global)? else {
        return Err(ClientError::NoManifest);
    };
    // The selection resolves (and refuses) before the dial too.
    let dest_entries = selection.mcp_entries(target.scope)?;

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
    if ctx.fs.exists(&bundle_dir) {
        if !resumable_leftover(ctx, &target, &bundle_dir, &document) {
            return Err(ClientError::InvalidArgument(format!(
                "{} already exists — {} would be written there; move or remove it, or import the \
                 folder directly (`topos add --mcp {}`)",
                bundle_dir.display(),
                summary.name,
                bundle_dir.display()
            )));
        }
        // A matching `server.json` proves the DOCUMENT is ours — not the folder. Resuming
        // registers the WHOLE standing dir, so the whole dir passes the same candidate gate the
        // adopt door runs: the exact allowed file set plus the per-file credential scan. A stray
        // script or a credential-bearing README beside our own document refuses by name instead
        // of riding the resume into the row.
        let scanned = crate::scan::scan(&bundle_dir)?;
        let files: Vec<(&str, &[u8])> = scanned
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.bytes.as_slice()))
            .collect();
        mcp_validate::validate_candidate_files(&files)?;
    }

    // The breadth line honors the scope's narrowing exactly as the converge below will. The row
    // is not written yet, so the selection (which the row will spell as `dest`) narrows here
    // directly; without one, a fresh row spells no `dest` and the reach is every MCP-capable
    // agent.
    let filter = if dest_entries.is_empty() {
        row_narrowing(ctx, &target, &slug).0
    } else {
        // The selection was validated against the grammar before this point, so an entry that
        // maps nowhere cannot reach here; the narrowing is read for reach alone.
        super::reconcile::mcp_dest_narrowing(Some(dest_entries.clone()), target.scope).filter
    };
    let agents = engaged_agents(ctx, &target, global, filter.as_deref());

    // ---- APPLY ----
    // The BYTES first (the row must never point at a folder that is not there), then the import
    // record (what lets `remove` prove the folder is still ours), then the row, then the converge
    // — each step convergent on its own, so a failure part-way leaves a state the next retry or
    // `update` finishes rather than a half-trusted one.
    // Whether the row write below is what BRINGS the manifest into existence — read before a
    // byte moves, so the record states the world this add found, not the one it made.
    let manifest_born = !ctx.fs.exists(&target.path);
    let scope = medit::add_scope(ctx, global)?;
    ctx.fs.create_dir_all(&bundle_dir)?;
    crate::atomic::atomic_write(ctx.fs, &bundle_dir.join("server.json"), &document)?;
    record_import(ctx, &scope.layout, &bundle_dir, &document, manifest_born)?;

    // The digest of what LANDED, over the bytes on disk — the same kernel function the adopt
    // door runs, so a consumer holding the folder can recompute it and get this answer back. A
    // scan that cannot read the folder it just wrote leaves the field absent; the add still
    // stands, and an absent digest claims nothing.
    let bundle_digest = crate::scan::scan(&bundle_dir)
        .ok()
        .map(|scanned| topos_core::digest::to_hex(&scanned.bundle_digest));
    let mut data = row_only_receipt(&slug, bundle_digest);
    medit::note_added_path_kind_dest_in(
        ctx,
        &mut data,
        &scope.target,
        &bundle_dir,
        Some(BundleKind::Mcp),
        &dest_entries,
    )?;
    if !dest_entries.is_empty() {
        data.dest = dest_entries.clone();
    }
    let bundle_id = format!("local:{slug}");
    let lines = converge_one(ctx, &scope.target, &bundle_id, &slug);
    fold_receipt(&mut data, &summary, Some(&bundle_dir), agents, &lines);
    Ok(Box::new(data))
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

/// The receipt shape a row-only add returns (the fetched arm mints no version history — see the
/// module docs), carrying the ONE identity such an import can honestly state: `bundle_digest`,
/// the kernel digest of the bytes it just wrote. The sidecar id and the version stay ABSENT
/// rather than zeroed — no store here holds either, and a receipt that stated them would be
/// naming a history nothing can be asked for.
fn row_only_receipt(name: &str, bundle_digest: Option<String>) -> AddData {
    AddData {
        skill_id: None,
        name: name.to_owned(),
        version_id: None,
        bundle_digest,
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
        mcp: None,
        dest: Vec::new(),
        display: None,
    }
}

// -------------------------------------------------------------------------------------------------
// The import record — what makes `remove` the fetched arm's verifiable FULL inverse
// -------------------------------------------------------------------------------------------------

/// `state/mcp_imports.json` — the machine-local custody record of the folders the FETCHED arm
/// itself wrote: canonical bundle dir → the sha256 of the exact `server.json` bytes the import
/// landed there. The `remove` path deletes such a folder ONLY when it still holds exactly that
/// one file at exactly those bytes; anything else — an edited document, a sibling file, no record
/// at all (an adopted folder is the person's own material) — is KEPT and disclosed. No undo beats
/// a wrong one: the add receipt may present `topos remove <ref>` as the full inverse only because
/// this record makes the folder half verifiable.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct McpImports {
    #[serde(default)]
    pub schema_version: u32,
    /// Canonical bundle-dir path → sha256 hex of the written `server.json` bytes.
    #[serde(default)]
    pub imports: BTreeMap<String, String>,
    /// The imports whose add ALSO brought this scope's `topos.toml` into existence — the file was
    /// not there when the add started. It is the other half of "the add left nothing behind": a
    /// birth this import caused is a birth its inverse may take back (only while the file still
    /// holds exactly what it was born with — see [`remove_imported_bundle`]'s caller).
    #[serde(default)]
    pub manifest_born: BTreeSet<String>,
}

/// The record's path in one scope's store.
fn imports_path(layout: &Layout) -> PathBuf {
    layout.state_dir().join("mcp_imports.json")
}

/// Record the document this import just wrote at `dir` (read-modify-write; an unreadable document
/// starts fresh — the worst outcome of a lost record is a KEPT folder, disclosed). `manifest_born`
/// says whether this same add is what brought the scope's manifest file into existence.
fn record_import(
    ctx: &Ctx<'_>,
    layout: &Layout,
    dir: &Path,
    document: &[u8],
    manifest_born: bool,
) -> Result<(), ClientError> {
    ctx.fs.create_dir_all(&layout.state_dir())?;
    let path = imports_path(layout);
    let mut doc: McpImports = crate::doc::read_doc(ctx.fs, &path)?.unwrap_or_default();
    doc.schema_version = topos_types::PERSISTED_SCHEMA_VERSION;
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let key = canon.display().to_string();
    doc.imports.insert(
        key.clone(),
        topos_core::digest::to_hex(&topos_core::digest::sha256(document)),
    );
    if manifest_born {
        doc.manifest_born.insert(key);
    }
    crate::doc::write_doc(ctx.fs, &path, &doc)
}

/// What one dropped `kind = "mcp"` row's folder half came to.
pub(crate) struct ImportRemoval {
    /// The disclosure line for the removal receipt, when the folder had something to say.
    pub note: Option<String>,
    /// This import's add is what brought the scope's manifest file into existence — so its
    /// inverse may take that file back, if nothing else has since been written into it.
    pub manifest_born: bool,
}

/// The `remove` path's half of the inverse: delete the folder a fetched import wrote at `dir` —
/// IF this scope's record proves it, and IF the folder still holds exactly the recorded bytes —
/// and prune the sidecar's own `mcp/` parent once the last import leaves it. `None` when the
/// folder was never a recorded import (an adopted folder — untouched, nothing to say).
///
/// Fail toward KEEPING: an unreadable record, extra files, different bytes — every indeterminate
/// answer keeps the folder and says so. The record row is dropped either way (the manifest row is
/// gone, so the custody question is closed).
pub(crate) fn remove_imported_bundle(
    ctx: &Ctx<'_>,
    layout: &Layout,
    dir: &Path,
) -> Option<ImportRemoval> {
    let path = imports_path(layout);
    let mut doc: McpImports = crate::doc::read_doc(ctx.fs, &path).ok().flatten()?;
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let key = canon.display().to_string();
    let recorded = doc.imports.remove(&key)?;
    let manifest_born = doc.manifest_born.remove(&key);
    let _ = crate::doc::write_doc(ctx.fs, &path, &doc);
    let removal = |note: Option<String>| {
        Some(ImportRemoval {
            note,
            manifest_born,
        })
    };
    if !ctx.fs.exists(&canon) {
        return removal(None); // already gone — nothing to delete, nothing to keep
    }
    if import_still_pristine(ctx, &canon, &recorded) {
        return match ctx.fs.remove_dir_all(&canon) {
            Ok(()) => {
                prune_person_mcp_dir(ctx, layout, &canon);
                removal(Some(format!(
                    "the folder this import wrote ({}) still held exactly the fetched document \
                     and was removed with the row",
                    canon.display()
                )))
            }
            Err(e) => removal(Some(format!(
                "the folder this import wrote ({}) could not be removed ({e}) — it is left in \
                 place",
                canon.display()
            ))),
        };
    }
    removal(Some(format!(
        "kept {} — its bytes no longer match what this import wrote, so nothing there is deleted",
        canon.display()
    )))
}

/// Take away the sidecar's own `mcp/` folder once the last import has left it. Narrow on purpose:
/// only the dir THIS binary creates for person-scope imports (`<home>/mcp`), and only while it is
/// empty. A project import writes beside the person, where the parent is their own checkout —
/// nothing there is topos's to prune.
fn prune_person_mcp_dir(ctx: &Ctx<'_>, layout: &Layout, removed: &Path) {
    let ours = layout.home().join(PERSON_MCP_DIR);
    let ours_canon = ours.canonicalize().unwrap_or_else(|_| ours.clone());
    if removed.parent() != Some(ours_canon.as_path()) {
        return;
    }
    // `read_dir` cannot tell an empty dir from an absent one, so existence is asked separately —
    // and a read that fails at all leaves the dir standing.
    if ctx.fs.exists(&ours) && ctx.fs.read_dir(&ours).is_ok_and(|e| e.is_empty()) {
        let _ = ctx.fs.remove_dir_all(&ours);
    }
}

/// Whether `dir` still holds EXACTLY what the import wrote: one entry, `server.json`, whose bytes
/// hash to the recorded digest. Any read failure answers `false` (kept, disclosed).
fn import_still_pristine(ctx: &Ctx<'_>, dir: &Path, recorded_sha: &str) -> bool {
    let Ok(entries) = ctx.fs.read_dir(dir) else {
        return false;
    };
    if entries.len() != 1 {
        return false;
    }
    let only = &entries[0];
    if only.file_name().is_none_or(|n| n != "server.json") {
        return false;
    }
    let Ok(Some(bytes)) = ctx.fs.read_opt(only) else {
        return false;
    };
    topos_core::digest::to_hex(&topos_core::digest::sha256(&bytes)) == recorded_sha
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
fn converge_one(ctx: &Ctx<'_>, target: &EditTarget, bundle_id: &str, name: &str) -> Vec<String> {
    let Some(roots) = ctx.roots.clone() else {
        return Vec::new();
    };
    let Some((layout, project_root)) = super::manifest_edit::mcp_scope_target(ctx, target) else {
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
    // The SAME narrowing the sweep resolves for this row (its `dest` entries mapped to the
    // config files they name) — one shared resolution, so the add can never place into a
    // harness the next sweep would claw back.
    let (filter, filter_warnings) = row_narrowing(ctx, target, name);
    let demand = crate::mcp_engine::McpDemand {
        bundle_id: bundle_id.to_owned(),
        name: name.to_owned(),
        workspace_slug: None,
        version_id: String::new(),
        server_json,
        harness_filter: filter,
    };
    let outcome = crate::mcp_engine::converge(
        &io,
        std::slice::from_ref(&demand),
        &topos_harness::mcp::descriptor::mcp_harnesses(),
        &detected,
        &HashSet::new(),
        false,
    );
    let mut lines: Vec<String> = filter_warnings;
    for bundle in outcome.bundles {
        let mut states = bundle.states;
        super::sync_engine::prettify_state_files(ctx, &mut states);
        lines.extend(states.iter().map(agent_line));
    }
    lines.extend(outcome.warnings.iter().cloned());
    lines
}

/// One config outcome as an add-receipt line, KEYED BY THE CONFIG FILE the entry landed in —
/// receipts speak in destinations, never agents; a state with no file to name (not supported,
/// unprovable) keys by the agent slug, the only place it exists. The reload/sign-in note stays —
/// teaching text about how the change goes live in that harness. Phrases come from the ONE
/// shared vocabulary ([`crate::mcp_engine::state_phrase`]) — the same words `update` prints,
/// letter for letter, so a person meets one name per state and not two.
fn agent_line(state: &topos_types::results::McpAgentState) -> String {
    let where_ = state.file.as_deref().unwrap_or("its config");
    match state.state.as_str() {
        // A fresh/updated placement carries the harness's reload note (how the change goes live —
        // "restart Cursor", "sign in with /mcp"), exactly as the update path's receipt does; an
        // already-current entry carries none. Both are a server entry sitting in that file, which
        // is what this receipt reports — the states differ only in whether THIS run wrote it.
        "placed" | "current" => match &state.note {
            Some(note) => format!("{where_}: server entry — {note}"),
            None => format!("{where_}: server entry"),
        },
        "drifted" => format!("{where_}: hand-edited entry left in place"),
        "conflicting" => format!("{where_}: an entry topos does not own already holds that name"),
        other => match &state.note {
            Some(note) => format!(
                "{}: {} — {note}",
                state.file.as_deref().unwrap_or(&state.agent),
                crate::mcp_engine::state_phrase(other)
            ),
            None => format!(
                "{}: {}",
                state.file.as_deref().unwrap_or(&state.agent),
                crate::mcp_engine::state_phrase(other)
            ),
        },
    }
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
/// `bundle_dir` is the folder the import wrote or adopted; `None` for a workspace-delivered
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
        auth: summary.auth_hint.map(|a| a.as_str().to_owned()),
        headers: summary.headers.iter().map(|h| h.name.clone()).collect(),
        bundle: bundle_dir.map(|d| d.display().to_string()),
        agents,
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
    lines: &[String],
) {
    let nobody = agents.is_empty();
    data.mcp = Some(summarize(summary, bundle_dir, agents));
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
    if nobody {
        medit::push_note(
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

    /// ITEM PAIR (registry-name hijack): ONLY a path spelling opens the Path door. A directory
    /// literally named `io.github.acme/weather` must not turn a registry import into an ungated
    /// local adopt — the collision refuses, naming both spellings out.
    #[test]
    fn only_a_path_spelling_opens_the_path_door() {
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &everything_exists),
            McpSourceShape::Path(PathBuf::from("./servers/weather"))
        );
        // A registry-shaped token that ALSO names a real directory refuses toward BOTH explicit
        // spellings — never a silent local adopt.
        assert_eq!(
            classify_mcp_source("io.github.acme/weather", None, &everything_exists),
            McpSourceShape::DirShadowsRegistry("io.github.acme/weather".to_owned())
        );
        // A path SPELLING that names nothing is a missing folder, never a registry name.
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &nothing_exists),
            McpSourceShape::Unreadable
        );
        // A bare word naming a real folder no longer adopts it — `./weather` is the spelling that
        // does.
        assert_eq!(
            classify_mcp_source("weather", None, &everything_exists),
            McpSourceShape::Unreadable
        );
        // A non-registry, non-path token naming a real folder reads as a reference, exactly as it
        // does when nothing exists — the bytes on disk never re-classify a spelling.
        assert_eq!(
            classify_mcp_source("io.github.acme/team/weather", None, &everything_exists),
            classify_mcp_source("io.github.acme/team/weather", None, &nothing_exists),
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

    /// ONE STATE VOCABULARY. A state that landed nowhere has no file to name, so `add` says about
    /// it exactly what `update` says — the same sentence, not a second spelling of it. The raw
    /// engine token (`not-supported`) is never what a person is handed.
    #[test]
    fn add_and_update_name_a_withheld_state_identically() {
        let withheld = topos_types::results::McpAgentState {
            agent: "openclaw".to_owned(),
            state: "not-supported".to_owned(),
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

        // Noteless, and an OPEN token this client has no phrase for: still one spelling on both
        // receipts, and an unknown state still says what it was told.
        for state in ["not-supported", "some-future-state"] {
            let s = topos_types::results::McpAgentState {
                agent: "openclaw".to_owned(),
                state: state.to_owned(),
                note: None,
                file: None,
            };
            assert_eq!(
                agent_line(&s),
                crate::render::mcp_agent_line(&s, false),
                "{state}"
            );
        }
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
