//! `add --kind mcp <registry-name|https-url>` — share an MCP server with a workspace, and get it
//! on this machine in the same command.
//!
//! ONE door, and it is a THIN one: the spelling a person typed goes to the workspace, the server
//! reads the document, rules on it, and answers with the name it shares the server as. Everything
//! that decides what may be shared — whether the catalog holds that name, whether the document
//! carries a credential, whether the caller may write a server down at all — is answered where the
//! workspace lives, once, for every surface that asks. A client that re-decided any of it would be
//! a second gate with its own vocabulary to drift.
//!
//! With the server shared, the rest is the ordinary workspace add: the row lands in the manifest
//! being edited and the scope's MCP configs converge, exactly as `topos add <name>` does for a
//! server somebody else already shared.
//!
//! A LOCAL server — one this machine keeps to itself — is a manifest row pointing at a folder whose
//! root holds `server.json`. It stays a hand-written line: nothing is shared, so there is nobody to
//! ask and no ruling to make, and the converge reads the file every run.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use topos_types::requests::McpAddRequest;
use topos_types::results::{AddData, McpPackageSummary, McpServerSummary};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::mcp_render::ServerDoc;

use super::manifest_edit::{self as medit, EditTarget};
use super::reconcile::SessionConnect;

// =================================================================================================
// The source shape
// =================================================================================================

/// What an MCP positional IS. Decided by SHAPE alone, so the decision is testable without a
/// filesystem, a network, or a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpSourceShape {
    /// An official-registry server name (`<reverse.dns>/<server>`) — the workspace resolves it
    /// against its catalog, and pulls it in when it does not hold it yet.
    RegistryName,
    /// An https link to a server document the workspace should hold as its own.
    DocumentUrl,
    /// A PATH-SPELLED folder on this machine (`./x`, `../x`, `~/x`, `/abs`, `.`, `..`).
    Path(PathBuf),
    /// A workspace / channel / feed / forge reference. The kind flag has nothing to add to one: a
    /// workspace bundle already carries its kind in the catalog.
    Reference,
    /// Nothing this door can read.
    Unreadable,
}

/// The registry's name grammar, hand-checked: `^[a-zA-Z0-9.-]+/[a-zA-Z0-9._-]+$` — exactly one
/// slash, both halves non-empty.
fn is_registry_name(name: &str) -> bool {
    let mut parts = name.split('/');
    let (Some(ns), Some(server), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !ns.is_empty()
        && !server.is_empty()
        && ns
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Classify an MCP positional. `host` is this machine's default manifest host (the workspace
/// reference grammar's), `exists` answers whether a token names a real directory.
///
/// The order is deliberate: an `@`-led token is unambiguously a workspace sugar; a path SPELLING
/// is a path, whether or not the folder is there; a scheme is a document link; and only then does
/// the registry's own name grammar get to claim a `<a.b>/<c>` token — except when its first segment
/// is a server this machine talks to (or `github.com`), which would make the flag a way to
/// mislabel a governed reference.
pub(crate) fn classify_mcp_source(
    source: &str,
    host: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> McpSourceShape {
    let _ = exists;
    let token = source.trim();
    if token.is_empty() {
        return McpSourceShape::Unreadable;
    }
    if token.starts_with('@') || token.starts_with('#') {
        return McpSourceShape::Reference;
    }
    let looks_path = token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
        || token == "."
        || token == "..";
    if looks_path {
        return McpSourceShape::Path(PathBuf::from(token));
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
    if is_registry_name(token) {
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
/// recorded — a durable row, true whatever the configs did, and it narrates every surface the
/// converge answered for. The MESSAGES say what could not be delivered, and they are what the
/// exit status and `ok` are computed from: an add that could not put the server where it was
/// asked to is not a success, whatever else landed, so an agent watching a loop learns that
/// something is not converging.
#[derive(Debug)]
pub(crate) struct McpAdded {
    pub data: Box<AddData>,
    /// One `failure` per surface the converge could not write.
    pub messages: Vec<topos_types::Message>,
}

/// `topos add --kind mcp <registry-name|https-url> [-g]`. The one door applies immediately with an
/// undo-led receipt; `--yes` is parsed by the CLI and changes nothing here.
///
/// # Errors
/// [`ClientError::InvalidArgument`] for a source this door cannot read, and for every refusal the
/// workspace answers with (rendered in the server's own words);
/// [`ClientError::SessionRequired`] when this machine is logged into no workspace at all; the
/// `add`-family errors from the row write; a filesystem failure.
pub(crate) fn add_mcp(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    source: &str,
    workspace: Option<&str>,
    global: bool,
    selection: &super::dest_select::Selection,
) -> Result<McpAdded, ClientError> {
    let host = medit::manifest_host(ctx);
    let exists = |p: &Path| ctx.fs.exists(p);
    let request = match classify_mcp_source(source, host.as_deref(), &exists) {
        McpSourceShape::RegistryName => McpAddRequest {
            schema_version: topos_types::WIRE_SCHEMA_VERSION,
            registry_name: Some(source.trim().to_owned()),
            document_url: None,
        },
        McpSourceShape::DocumentUrl => McpAddRequest {
            schema_version: topos_types::WIRE_SCHEMA_VERSION,
            registry_name: None,
            document_url: Some(source.trim().to_owned()),
        },
        // A folder on this machine is a server this machine keeps to itself. There is nothing to
        // share and nobody to ask, so it is a line in the file rather than a command — and the
        // refusal spells the line, because a person who typed this wants that.
        McpSourceShape::Path(dir) => {
            return Err(ClientError::InvalidArgument(format!(
                "`{source}` is a folder on this machine — a server only this machine runs is a \
                 line in your topos.toml, not something to share: add `\"{}\" = {{ kind = \"mcp\" \
                 }}` under [bundles], then run 'topos update'. To share this server with your \
                 workspace, `topos add --kind mcp <its registry name or the https link to its \
                 server.json>`",
                dir.display()
            )));
        }
        McpSourceShape::Reference => {
            return Err(ClientError::InvalidArgument(format!(
                "`{source}` names a workspace bundle — a workspace already records what each \
                 bundle is, so `topos add {source}` gets it with its kind intact, no `--kind` \
                 needed"
            )));
        }
        McpSourceShape::Unreadable => {
            return Err(ClientError::InvalidArgument(format!(
                "`{source}` is neither a registry server name (`io.github.acme/weather`) nor an \
                 https link to a server.json — those are what `--kind mcp` shares with your \
                 workspace"
            )));
        }
    };
    // REFUSAL-FIRST, and the refusals that are purely LOCAL come first of all: which file this
    // row would land in, and whether the `-a`/`--dest` selection names config files this scope
    // can edit. Both are answered without asking anybody. Asking the workspace first would leave
    // a server shared with the whole team by a command that then failed and recorded nothing —
    // a side effect nobody asked for and no receipt names.
    let scope = medit::add_scope(ctx, global)?;
    selection.mcp_entries(scope.target.scope)?;
    let session = pick_session(ctx, workspace)?;
    let added = (connect)(&session)
        .directory
        .add_mcp_server(&session.workspace_id, request)?;
    // The server shared it; the rest is the ordinary workspace add of the name it gave back.
    let reference = format!("{}/{}/{}", session.host, session.workspace_name, added.name);
    let outcome = super::reference::add_reference(
        ctx,
        connect,
        None,
        &reference,
        global,
        true,
        selection,
        Some(crate::bundle_kind::BundleKind::Mcp),
    )?;
    let (data, messages) = match outcome {
        super::reference::AddRefOutcome::Applied { data, messages } => (data, messages),
        // A workspace reference applies immediately; nothing here can produce a describe.
        super::reference::AddRefOutcome::Described { .. } => {
            return Err(ClientError::Corrupt(
                "the add described instead of applying".to_owned(),
            ));
        }
    };
    Ok(McpAdded { data, messages })
}

/// The workspace this add shares the server with: the one `--workspace` names, else the single
/// live session, else a refusal that names the choices. Guessing between two workspaces is the one
/// thing a share must never do — the server would be shared with the wrong team and everybody in
/// it would get it.
fn pick_session(
    ctx: &Ctx<'_>,
    workspace: Option<&str>,
) -> Result<crate::sessions::Session, ClientError> {
    let live = medit::live_sessions(ctx)?;
    if let Some(asked) = workspace {
        return live
            .into_iter()
            .find(|s| s.workspace_name == asked || s.workspace_id == asked)
            .ok_or_else(|| {
                ClientError::InvalidArgument(format!(
                    "this machine is not logged into `{asked}` — run 'topos login {asked}' first"
                ))
            });
    }
    match live.as_slice() {
        [] => Err(ClientError::SessionRequired {
            address: String::new(),
            message: "this machine is logged into no workspace — sharing a server needs one to \
                      share it with; run 'topos login' first"
                .to_owned(),
        }),
        [one] => Ok(one.clone()),
        several => Err(ClientError::InvalidArgument(format!(
            "this machine is logged into more than one workspace ({}) — name the one to share \
             this server with: `topos --workspace <name> add --kind mcp …`",
            several
                .iter()
                .map(|s| s.workspace_name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Fold the typed MCP block onto a WORKSPACE-DELIVERED bundle's add receipt — read back from the
/// scope store the add just recorded into, so the receipt states the document that actually
/// landed. Best-effort by construction: the row is the durable demand, so a scope the record has
/// not reached yet just leaves the receipt block-less and the next `update` lands it. No `bundle`
/// folder is named — a workspace server has no folder of its own.
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
    let Ok(Some(recorded)) = crate::mcp_engine::recorded_server(&sctx, &sid) else {
        return;
    };
    let Ok(doc) = crate::mcp_render::parse_server_json(&recorded.document) else {
        return;
    };
    let (filter, _) = row_narrowing(ctx, target, &data.name);
    let agents = engaged_agents(ctx, target, global, filter.as_deref());
    fold_receipt(data, &doc, agents);
}

// -------------------------------------------------------------------------------------------------
// The receipt
// -------------------------------------------------------------------------------------------------

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
        (row.shape.leaf_name() == name)
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

/// The publisher's auth word as a receipt says it — `None` when the document declared none, which
/// is NOT a claim that no credential is needed.
fn auth_word(hint: topos_harness::mcp::AuthHint) -> Option<&'static str> {
    match hint {
        topos_harness::mcp::AuthHint::None => Some("none"),
        topos_harness::mcp::AuthHint::Oauth => Some("oauth"),
        topos_harness::mcp::AuthHint::Manual => Some("manual"),
        topos_harness::mcp::AuthHint::Unknown => None,
    }
}

/// The typed receipt payload — derived facts only; the document is never echoed whole.
fn summarize(doc: &ServerDoc, agents: Vec<String>) -> McpServerSummary {
    let remote = doc.remote.as_ref();
    McpServerSummary {
        server: doc.name.clone(),
        description: doc.description.clone(),
        version: doc.version.clone(),
        url: remote.map(|r| r.url.clone()).unwrap_or_default(),
        transport: if remote.is_some() {
            "streamable-http".to_owned()
        } else {
            String::new()
        },
        packages: doc
            .packages
            .iter()
            .map(|p| McpPackageSummary {
                registry: p.registry.clone(),
                identifier: p.identifier.clone(),
                version: p.version.clone(),
            })
            .collect(),
        auth: auth_word(doc.auth).map(str::to_owned),
        headers: remote
            .map(|r| r.headers.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default(),
        bundle: None,
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
fn what_it_is(doc: &ServerDoc) -> String {
    if let Some(remote) = &doc.remote {
        return format!("{} over streamable-http", remote.url);
    }
    match crate::mcp_render::preferred_package(&doc.packages).or_else(|| doc.packages.first()) {
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

/// Fold what landed into the add receipt: the typed `mcp` block, then the prose note — WHAT this
/// row now points at. Where the entries went is the converge's own answer, and the add's receipt
/// already carries it per surface; this adds the one thing that answer cannot: which server.
fn fold_receipt(data: &mut AddData, doc: &ServerDoc, agents: Vec<String>) {
    let nobody = agents.is_empty();
    let auth = auth_word(doc.auth).map_or_else(String::new, |w| format!(", auth {w}"));
    data.mcp = Some(summarize(doc, agents));
    // EACH FACT ON ITS OWN LINE. These are sentences, not asides.
    medit::push_note_line(
        data,
        format!(
            "MCP server {} v{} — {}{auth}",
            doc.name,
            doc.version,
            what_it_is(doc)
        ),
    );
    // The one auth word that is a TASK. `oauth` and `none` describe something that happens by
    // itself; `manual` describes something a person has to go and do, and the entry stands here
    // doing nothing until they do.
    if doc.auth == topos_harness::mcp::AuthHint::Manual {
        medit::push_note_line(
            data,
            "Signing in to this server is a one-time manual step on this machine — a token you \
             create, or an app an administrator registers; no agent can complete it",
        );
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

    fn doc_with(packages: Vec<crate::mcp_render::PackageRef>) -> ServerDoc {
        ServerDoc {
            name: "io.github.acme/x".to_owned(),
            description: "d".to_owned(),
            version: "1.0.0".to_owned(),
            remote: None,
            packages,
            auth: topos_harness::mcp::AuthHint::Unknown,
        }
    }

    fn package(registry: &str, identifier: &str, transport: &str) -> crate::mcp_render::PackageRef {
        crate::mcp_render::PackageRef {
            registry: registry.to_owned(),
            identifier: identifier.to_owned(),
            version: "1.2.3".to_owned(),
            transport: transport.to_owned(),
            runtime_args: Vec::new(),
            package_args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// **The receipt names the package the placement takes, not the first one listed.** The
    /// selection prefers npm over pypi and passes over a package no entry can express; a line
    /// reading off the document's order would name a package no agent here was given.
    #[test]
    fn the_receipt_names_the_package_the_placement_would_take() {
        // pypi listed first, npm second: the placement takes npm, so the line says npm.
        assert_eq!(
            what_it_is(&doc_with(vec![
                package("pypi", "acme-x", "stdio"),
                package("npm", "@acme/x", "stdio"),
            ])),
            "the npm package @acme/x@1.2.3, run on this machine"
        );
        // A registry with no arm listed first is passed over the same way.
        assert_eq!(
            what_it_is(&doc_with(vec![
                package("nuget", "Acme.X", "stdio"),
                package("pypi", "acme-x", "stdio"),
            ])),
            "the pypi package acme-x@1.2.3, run on this machine"
        );
        // …and so is an npm package served over http, which no entry can express.
        assert_eq!(
            what_it_is(&doc_with(vec![
                package("npm", "@acme/served", "streamable-http"),
                package("pypi", "acme-x", "stdio"),
            ])),
            "the pypi package acme-x@1.2.3, run on this machine"
        );
        // Nothing this build can run: the first one, since there is no chosen one to name.
        assert_eq!(
            what_it_is(&doc_with(vec![package("nuget", "Acme.X", "stdio")])),
            "the nuget package Acme.X@1.2.3, run on this machine"
        );
        // AN IDENTIFIER THAT ALREADY PINS IS THE WHOLE COORDINATE.
        assert_eq!(
            what_it_is(&doc_with(vec![package(
                "oci",
                "ghcr.io/acme/x@sha256:8ba1",
                "stdio"
            )])),
            "the oci package ghcr.io/acme/x@sha256:8ba1, run on this machine"
        );
        // An address outranks every package — it is what the agent will actually use.
        let mut dialed = doc_with(vec![package("npm", "@acme/x", "stdio")]);
        dialed.remote = Some(crate::mcp_render::RemoteEndpoint {
            url: "https://mcp.example/x".to_owned(),
            headers: Vec::new(),
        });
        assert_eq!(
            what_it_is(&dialed),
            "https://mcp.example/x over streamable-http"
        );
    }

    /// A registry-shaped name and an https link are the two spellings this door shares — and a
    /// folder is neither, whether or not it exists.
    #[test]
    fn the_two_shared_spellings_and_the_folder_that_is_not_one() {
        let host = Some("topos.sh");
        assert_eq!(
            classify_mcp_source("io.github.acme/weather", host, &nothing_exists),
            McpSourceShape::RegistryName
        );
        // Whitespace around the token is the shell's, not the person's intent.
        assert_eq!(
            classify_mcp_source("  io.github.acme/weather  ", host, &nothing_exists),
            McpSourceShape::RegistryName
        );
        // Three segments cannot be a server name.
        assert_eq!(
            classify_mcp_source("io.github.acme/team/weather", host, &nothing_exists),
            McpSourceShape::Reference
        );
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
        assert_eq!(
            classify_mcp_source("./servers/weather", None, &nothing_exists),
            McpSourceShape::Path(PathBuf::from("./servers/weather"))
        );
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

    /// A bare word names neither a server nor a link; so does nothing at all.
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
}
