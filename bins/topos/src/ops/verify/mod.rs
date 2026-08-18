//! `topos verify <name>` — **the live check of one MCP server bundle.**
//!
//! Delivery says what a machine HAS. This verb says what the server DOES, right now, from here: it
//! dials the address (or runs the program) the bundle names, holds a real MCP conversation with it,
//! and prints one sentence.
//!
//! ## What it is not
//!
//! **Nothing is stored.** No delivery state changes, no sidecar document is written, no catalog row
//! is touched. `current` keeps meaning "you have the followed version" and cannot be confused with
//! "it answered"; a verification is about this moment and this machine, and a moment is not a state.
//!
//! **No credential is ever supplied.** topos holds none, and this verb does not ask the harness for
//! the one it holds. A server that refuses without a sign-in is therefore the EXPECTED answer for
//! every OAuth-gated server, and it is reported as HEALTHY. WHO can complete that sign-in is a
//! second question the refusal itself cannot answer, so the sign-in walk ([`discovery`]) reads the
//! server's own discovery documents for it: a server that registers clients on demand keeps the
//! line that says the agent app signs in on first use, and a server that takes only pre-registered
//! clients or tokens says so instead. Neither changes the verdict or the exit code — a server
//! somebody has to set up by hand is still a healthy server.
//!
//! ## The shape a verification checks
//!
//! It checks the HARNESS-INDEPENDENT rendition: the address where the bundle offers one, else the
//! package this build can run. That is deliberate — verifying "what Cursor would get" would answer
//! a question about a config file, and the question here is about the server. The one selection
//! ([`crate::mcp_render::select`]) makes that choice, so the thing verified is the thing delivered.
//!
//! ## Exit codes
//!
//! `0` responding · `3` sign-in required (healthy) · `4` not reachable · `5` reachable but not
//! answering as an MCP server. They are the verb's contract for a script, and they are documented
//! in the CLI reference beside the general `1`/`2`.

mod discovery;
mod local;
mod remote;
mod wire;

use topos_harness::mcp::McpTarget;
use topos_types::results::VerifyData;

use super::StoreScope;
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::mcp_render::{self, HarnessCaps, Machine, PathRuntimes};

use wire::Verdict;

/// The `target` word a payload carries for each of the two shapes.
const TARGET_ADDRESS: &str = "address";
const TARGET_PROGRAM: &str = "program";

/// **The verb.** `scope` is the invocation's own (`-g` narrows to the machine store); the name
/// resolves STANDING FIRST like every other targeted verb, and an ambiguous one refuses through the
/// shared chooser rather than picking.
///
/// # Errors
/// - the name resolves to nothing, or to more than one bundle;
/// - the bundle is not an MCP server (a skill has no server to verify);
/// - this scope holds no received version of it yet, or its document cannot be read.
pub(crate) fn verify(
    ctx: &Ctx<'_>,
    name: &str,
    scope: StoreScope,
) -> Result<VerifyData, ClientError> {
    // A server only THIS MACHINE runs is a line in a recipe and nothing else — no version was ever
    // received for it and no record files it — so the recipe is where its name resolves. Asked
    // FIRST, because a row is the whole record of such a server and the store has nothing to say
    // about one; a shared server has no such row and falls through to the ordinary resolution.
    if let Some(bytes) = local_row_server(ctx, name, scope)? {
        let doc = mcp_render::parse_server_json(&bytes)
            .map_err(|e| ClientError::Corrupt(format!("{name}: {e}")))?;
        return Ok(data_for(name, &check(&doc)?));
    }
    let (layout, sid, lock) = super::resolve_skill_in_scope(ctx, name, None, scope)?;
    let sctx = super::pull::ctx_with_layout(ctx, &layout);
    // The kind is asked of the ONE classification chain; an indeterminate record over an MCP verb
    // is a refusal, never a guess (the same rule every delivery path keeps).
    let placements = crate::doc::read_map(sctx.fs, &sctx.layout.published(&sid).map)
        .ok()
        .flatten()
        .map(|m| m.placements)
        .unwrap_or_default();
    if !crate::bundle_kind::classify(&sctx, sid.as_str(), &placements).is_mcp() {
        return Err(not_a_server(&lock.name));
    }
    // A local row's folder holds its own document; a workspace server's is the record this scope
    // was last given. Either way there is one document, read from where that kind keeps it.
    let Some(recorded) = crate::mcp_engine::recorded_server(&sctx, &sid)? else {
        return Err(ClientError::InvalidArgument(format!(
            "'{}' has no server document on this machine yet — run `topos update` first",
            lock.name
        )));
    };
    let bytes = recorded.document;
    let doc = mcp_render::parse_server_json(&bytes)
        .map_err(|e| ClientError::Corrupt(format!("{}: {e}", lock.name)))?;
    Ok(data_for(&lock.name, &check(&doc)?))
}

/// A LOCAL row's document: the `server.json` at the root of the folder a `kind = "mcp"` row in
/// this invocation's recipe points at. `None` when no such row answers to `name`.
///
/// The recipe is read the way the sweep reads it — the machine's own file, and (bare) the project
/// file covering this folder — so the server this verifies is the one this scope's next `update`
/// would place. A row whose folder is gone, or holds no `server.json`, answers `None` and falls
/// through: the store's own refusal for an unknown name is the honest one.
///
/// # Errors
/// A recipe the grammar refuses, or a filesystem failure reading the folder.
fn local_row_server(
    ctx: &Ctx<'_>,
    name: &str,
    scope: StoreScope,
) -> Result<Option<Vec<u8>>, ClientError> {
    let mut plans = vec![(
        ctx.layout.home().to_path_buf(),
        crate::manifest::scopes::person_plan(ctx.fs, &ctx.layout)?,
    )];
    if scope == StoreScope::Here
        && let Some(cwd) = ctx.roots.as_ref().and_then(|r| r.cwd.clone())
        && let Some((dir, plan)) = crate::manifest::scopes::nearest_project_plan(
            ctx.fs,
            &cwd,
            ctx.roots.as_ref().map(|r| r.home.as_path()),
        )?
    {
        plans.insert(0, (dir, plan));
    }
    for (base, plan) in plans {
        for row in &plan.things {
            let crate::manifest::keys::KeyShape::LocalPath { raw } = &row.shape else {
                continue;
            };
            if row.value.declared_kind() != Some(crate::bundle_kind::BundleKind::Mcp)
                || row.display_name() != name
            {
                continue;
            }
            let dir = if let Some(rest) = raw.strip_prefix("~/") {
                ctx.roots
                    .as_ref()
                    .map_or_else(|| std::path::PathBuf::from(raw), |r| r.home.join(rest))
            } else if std::path::Path::new(raw).is_absolute() {
                std::path::PathBuf::from(raw)
            } else {
                base.join(raw.trim_start_matches("./"))
            };
            if let Some(bytes) = ctx.fs.read_opt(&dir.join("server.json"))? {
                return Ok(Some(bytes));
            }
        }
    }
    Ok(None)
}

/// A `verify` on a bundle that is not a server. The refusal names what the verb needs and what
/// `list` would show instead — never a fake "responding" over a bundle with nothing to dial.
fn not_a_server(name: &str) -> ClientError {
    ClientError::InvalidArgument(format!(
        "'{name}' is a skill, not an MCP server — `verify` dials a server and asks it for its \
         tools, and a skill is files an agent reads. `topos list {name}` shows what it is"
    ))
}

/// What was checked, and what it answered.
struct Checked {
    target: &'static str,
    address: Option<String>,
    command: Vec<String>,
    verdict: Verdict,
}

/// **The one selection, then the matching conversation.** The capabilities handed to the selection
/// are the AGENT-NEUTRAL ones — this machine can both dial an address and run a program, which is
/// exactly the question a verification asks — so a bundle offering an address is verified at its
/// address, and a package bundle is verified by running the package.
///
/// # Errors
/// A capability gap: this build or this machine cannot set the server up at all, so there is
/// nothing to verify and no verdict to give.
fn check(doc: &mcp_render::ServerDoc) -> Result<Checked, ClientError> {
    let caps = HarnessCaps {
        remote: true,
        stdio: true,
        env_ref: topos_harness::mcp::descriptor::EnvRef::default(),
    };
    let machine = Machine {
        windows: cfg!(windows),
        wsl_dest: false,
    };
    // No bridge: with `remote` true the address is dialed directly, so the bridge arm — which
    // exists for agents that cannot dial one — is never the shape under test.
    match mcp_render::select(doc, caps, None, &PathRuntimes, machine) {
        Ok(McpTarget::Remote { url, headers }) => {
            let verdict = remote::probe(&remote::agent(), &url, &headers);
            Ok(Checked {
                target: TARGET_ADDRESS,
                address: Some(url),
                command: Vec::new(),
                verdict,
            })
        }
        Ok(McpTarget::Local {
            command, args, env, ..
        }) => {
            let verdict = local::probe(&command, &args, &env);
            Ok(Checked {
                target: TARGET_PROGRAM,
                address: None,
                command: shown_command(&command, &args),
                verdict,
            })
        }
        // A capability this machine or this build does not have. Nothing was dialed and nothing
        // ran — so this is a REFUSAL, not a verdict about the server. Reported as `not reachable`
        // it took the exit code a script retries on, over a condition that no retry can change:
        // an unsupported package type and an uninstalled runtime stay exactly as they are until
        // the build or the machine does. It exits 1 with the gap's own code, like every other
        // "this command cannot be run as asked".
        Err(gap) => Err(ClientError::McpUnverifiable(gap.code(), gap.refusal())),
    }
}

/// The program as a payload SHOWS it: the command and its arguments, and NOTHING of the environment
/// it ran with. The environment is where a machine-local value would be — a slot the bundle left
/// open is filled from this machine, and a receipt that printed one would leak exactly the thing
/// the no-credentials rule exists to keep out of topos. The slot NAMES are already in the bundle,
/// so there is nothing here for them to add either.
fn shown_command(command: &str, args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(command.to_owned());
    out.extend(args.iter().cloned());
    out
}

/// Assemble the typed payload from a verdict.
fn data_for(name: &str, checked: &Checked) -> VerifyData {
    VerifyData {
        name: name.to_owned(),
        target: checked.target.to_owned(),
        address: checked.address.clone(),
        command: checked.command.clone(),
        state: checked.verdict.state(),
        sign_in: checked.verdict.sign_in(),
        tools: match &checked.verdict {
            Verdict::Responding { tools, .. } => Some(u32::try_from(*tools).unwrap_or(u32::MAX)),
            _ => None,
        },
        detail: checked.verdict.detail(),
        protocol_version: checked.verdict.protocol().map(ToOwned::to_owned),
        exit_code: checked.verdict.exit_code(),
        line: checked.verdict.line(),
    }
}

// The stub-server suites drive these directly — no network, no fixture store.
#[cfg(test)]
pub(crate) use local::probe as probe_local;
#[cfg(test)]
pub(crate) use local::spawn_env;
#[cfg(test)]
pub(crate) use remote::{
    agent as remote_agent, agent_with as remote_agent_with, probe as probe_remote,
};
#[cfg(test)]
pub(crate) use wire::Verdict as ProbeVerdict;
