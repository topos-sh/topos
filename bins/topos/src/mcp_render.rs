//! **What one `server.json` becomes on THIS machine, for THIS agent** — the document read once
//! ([`ServerDoc`]), then turned into the entry a given harness can actually use ([`select`]).
//!
//! A shared bundle may offer an ADDRESS every machine dials, a PACKAGE every machine runs, or
//! both. Which of the two an agent gets is decided here, deterministically, from three facts and
//! nothing else: what the document offers, what the harness's registry row says it can take, and
//! whether the runtime that would run the program is on this machine.
//!
//! ## The selection order (fixed — read it as a list of rules, in this order)
//!
//! 1. the document offers an address AND the harness dials addresses itself → **the address**;
//! 2. the document offers an address AND the harness does NOT dial one, but runs programs and
//!    this build pins a bridge → **the bridge** (`npx -y mcp-remote@<pinned> <url>`), because the
//!    address is still the thing every machine reaches, and the bridge is how this one reaches it;
//! 3. otherwise, the first package this build can set up, preferring **npm**, then **pypi** —
//!    only where the harness runs programs at all;
//! 4. otherwise, nothing is placed, and the reason is said in plain words ([`Gap`]).
//!
//! **A missing runtime never falls through to the next rule.** If the selected form needs `npx`
//! and this machine has no node, the answer is "not placed, install node" — not "run the pypi one
//! instead". A shared bundle is bytes a team agreed on; a fallback keyed on what each laptop
//! happens to have installed would make the same bundle a different server per machine, quietly.
//! The placement is re-decided on every sweep, so installing the runtime is all it takes.
//!
//! ## What never travels
//!
//! No credential, ever. A package's environment slot that carries a literal, non-secret value
//! renders that value; a slot the document leaves EMPTY renders as a reference to an environment
//! variable of that name, in the spelling the harness itself reads
//! ([`EnvRef`](topos_harness::mcp::descriptor::EnvRef)) — so the name travels in the bundle and
//! each machine's own environment fills it in. A value the document leaves for a person to
//! fill in (a template) is not something topos can supply, and the placement says so.

use std::path::{Path, PathBuf};

use serde_json::Value;
use topos_harness::mcp::descriptor::EnvRef;
use topos_harness::mcp::{AuthHint, McpTarget, canonical_address, windows_wrap};
use topos_harness::registry::McpBridge;

// =================================================================================================
// The document
// =================================================================================================

/// One `server.json`, read for PLACEMENT: the address it offers (if any), the packages it offers
/// (in document order), and the publisher's auth word. Harness-independent by construction — the
/// same document answers every agent, and [`select`] is where they differ.
#[derive(Debug, Clone)]
pub(crate) struct ServerDoc {
    /// The FIRST `streamable-http` remote with a url, and its literal headers.
    pub remote: Option<RemoteEndpoint>,
    /// Every package the document declares, in its own order.
    pub packages: Vec<PackageRef>,
    pub auth: AuthHint,
}

/// The address half of a document.
#[derive(Debug, Clone)]
pub(crate) struct RemoteEndpoint {
    pub url: String,
    /// Literal, non-secret header pairs (gate-validated, re-checked at parse).
    pub headers: Vec<(String, String)>,
}

/// One package the document declares — enough of it to run, and nothing more.
#[derive(Debug, Clone)]
pub(crate) struct PackageRef {
    /// The registry it comes from (`npm`, `pypi`, `oci`, `nuget`, `mcpb`, …) — OPEN-WORLD: the
    /// publish gate accepts any word the official format expresses, and this build renders the
    /// two it has runtimes for.
    pub registry: String,
    /// The package's name in that registry.
    pub identifier: String,
    /// The exact version the document pins (empty where the registry type carries it inside the
    /// identifier, as an OCI digest does).
    pub version: String,
    /// Arguments to the RUNTIME, before the package is named (`docker run <these> <image>`).
    pub runtime_args: Vec<Argument>,
    /// Arguments to the SERVER, after the package is named.
    pub package_args: Vec<Argument>,
    /// The environment the server is run with.
    pub env: Vec<EnvSlot>,
}

/// One structured argument, in the official format's two shapes.
#[derive(Debug, Clone)]
pub(crate) struct Argument {
    /// A named argument's flag (`--port`); empty for a positional one.
    pub name: String,
    /// The literal value the document states, or `None` when it states none.
    pub value: Option<String>,
}

/// One environment slot: a NAME the server reads, and the value the document states for it — if
/// it states one at all.
#[derive(Debug, Clone)]
pub(crate) struct EnvSlot {
    pub name: String,
    pub value: Option<String>,
}

impl ServerDoc {
    /// **The ONE server this document names**, for the purposes of a config key's reservation: the
    /// address when it offers one, else the first package's own identity. It is a document-level
    /// fact on purpose — the same bundle mints the same key whichever form a given agent ends up
    /// running.
    pub(crate) fn canonical_identity(&self) -> Option<String> {
        if let Some(remote) = &self.remote
            && let Some(address) = canonical_address(&remote.url)
        {
            return Some(address);
        }
        self.packages.first().map(|p| {
            if p.version.is_empty() {
                format!("{}:{}", p.registry, p.identifier)
            } else {
                format!("{}:{}@{}", p.registry, p.identifier, p.version)
            }
        })
    }
}

// =================================================================================================
// Parsing
// =================================================================================================

/// Parse a bundle's `server.json` for placement. Anything the publish gate should have refused —
/// a secret / templated / variable / value-less HEADER — fails the whole demand closed (never
/// place a suspect entry).
///
/// DISCIPLINE, unchanged from the day this was only about addresses: the checks here are a
/// MATCHING RE-CHECK of the shared validation gate ([`crate::mcp_validate`]), rule for rule. Every
/// refusal this parse could make BEYOND the gate either moves into the gate (with shared vectors,
/// both tiers) or is deleted — a bundle the gate publishes must never be permanently unplaceable
/// here. What a given MACHINE can set up is a different question, and it is [`select`]'s.
///
/// # Errors
/// One honest sentence naming what could not be read.
pub(crate) fn parse_server_json(bytes: &[u8]) -> Result<ServerDoc, String> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("its server.json is not valid JSON ({e})"))?;
    let remote = parse_remote(&root)?;
    let packages = parse_packages(&root)?;
    if remote.is_none() && packages.is_empty() {
        return Err(
            "its server.json offers neither an address to dial nor a package to run".to_owned(),
        );
    }
    let auth = match root
        .get("_meta")
        .and_then(|m| m.get("sh.topos/auth"))
        .and_then(Value::as_str)
    {
        Some("oauth") => AuthHint::Oauth,
        Some("none") => AuthHint::None,
        _ => AuthHint::Unknown,
    };
    Ok(ServerDoc {
        remote,
        packages,
        auth,
    })
}

/// The FIRST remote with `type == "streamable-http"` AND a url — ONE predicate, the same pick the
/// gate makes, so the engine can never resolve a different remote than the one the gate approved.
fn parse_remote(root: &Value) -> Result<Option<RemoteEndpoint>, String> {
    let Some(remotes) = root.get("remotes").and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(remote) = remotes.iter().find(|r| {
        r.get("type").and_then(Value::as_str) == Some("streamable-http")
            && r.get("url").and_then(Value::as_str).is_some()
    }) else {
        return Ok(None);
    };
    let url = remote
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| "its streamable-http remote has no url".to_owned())?
        .to_owned();
    let mut headers = Vec::new();
    if let Some(list) = remote.get("headers").and_then(Value::as_array) {
        for h in list {
            let name = h
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| "one of its headers has no name".to_owned())?;
            // The publish gate refused secret / variable / value-less headers. RE-CHECK here,
            // fail-closed: a suspect header fails the whole demand rather than placing a header
            // whose value the gate never validated as a shareable literal. (`isRequired` with a
            // plain literal value is fine — the value satisfies the requirement.)
            if h.get("isSecret").and_then(Value::as_bool) == Some(true) {
                return Err(format!("its {name} header is marked secret"));
            }
            if h.get("variables").is_some_and(|v| !v.is_null()) {
                return Err(format!("its {name} header carries variable substitutions"));
            }
            let value = h
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("its {name} header carries no literal value"))?;
            if is_templated(value) {
                return Err(format!("its {name} header carries a templated value"));
            }
            headers.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(Some(RemoteEndpoint { url, headers }))
}

/// Every `packages[]` entry, in document order. A package this build has no runtime for is still
/// READ — [`select`] is what decides whether it can run here, and its refusal names the type.
fn parse_packages(root: &Value) -> Result<Vec<PackageRef>, String> {
    let Some(list) = root.get("packages").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let string = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default();
        let registry = string("registryType");
        let identifier = string("identifier");
        if registry.is_empty() || identifier.is_empty() {
            return Err("one of its packages names no registry or no package".to_owned());
        }
        out.push(PackageRef {
            registry: registry.to_owned(),
            identifier: identifier.to_owned(),
            version: string("version").to_owned(),
            runtime_args: parse_arguments(entry.get("runtimeArguments"))?,
            package_args: parse_arguments(entry.get("packageArguments"))?,
            env: parse_env(entry.get("environmentVariables"))?,
        });
    }
    Ok(out)
}

/// The official `Argument` list: positional entries carry a `value`, named ones carry a `name`
/// (the flag) and optionally a `value`. A `default` stands in where no `value` is stated.
fn parse_arguments(found: Option<&Value>) -> Result<Vec<Argument>, String> {
    let Some(list) = found.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let string = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        let name = string("name").unwrap_or_default();
        let is_named = entry.get("type").and_then(Value::as_str) == Some("named");
        if is_named && name.trim().is_empty() {
            return Err("one of its package arguments is named but has no name".to_owned());
        }
        out.push(Argument {
            name: if is_named { name } else { String::new() },
            value: string("value").or_else(|| string("default")),
        });
    }
    Ok(out)
}

/// The official `KeyValueInput` list: a NAME the server reads, plus the value the document states
/// — if it states one. An empty slot is not a gap: it is the machine's own environment, referenced
/// by name.
fn parse_env(found: Option<&Value>) -> Result<Vec<EnvSlot>, String> {
    let Some(list) = found.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .ok_or_else(|| "one of its environment variables has no name".to_owned())?;
        let value = ["value", "default"]
            .iter()
            .find_map(|k| entry.get(*k).and_then(Value::as_str))
            .map(ToOwned::to_owned);
        out.push(EnvSlot {
            name: name.to_owned(),
            value,
        });
    }
    Ok(out)
}

/// Whether a value is a TEMPLATE — something the document leaves for a person to fill in. The
/// same test the header re-check has always used, applied to package values.
fn is_templated(value: &str) -> bool {
    value.contains('{') && value.contains('}')
}

// =================================================================================================
// Selection
// =================================================================================================

/// What a harness can be given — its registry row's capability columns, plus the reference
/// spelling its config reads. Passed rather than looked up so the decision is testable against
/// every shape a table can state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HarnessCaps {
    /// The harness dials an https MCP address itself.
    pub remote: bool,
    /// The harness runs a local program as a server.
    pub stdio: bool,
    /// How it spells a reference to an environment variable.
    pub env_ref: EnvRef,
}

/// The machine-local facts a rendering depends on: whether this is Windows (the `cmd /c` wrapper)
/// and whether the config file being written belongs to a WSL distribution (where it would be
/// nonsense).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Machine {
    pub windows: bool,
    pub wsl_dest: bool,
}

impl Machine {
    /// This machine, for a config file at `dest`.
    pub(crate) fn at(dest: &Path) -> Self {
        Self {
            windows: cfg!(windows),
            wsl_dest: topos_harness::mcp::is_wsl_unc(&dest.display().to_string()),
        }
    }
}

/// Whether a program can be found and run on this machine — injected so a missing runtime is a
/// TESTED outcome rather than a property of whoever runs the suite.
pub(crate) trait RuntimeProbe {
    /// Whether `program` is on this machine's `PATH`.
    fn on_path(&self, program: &str) -> bool;
}

/// The real probe: this process's `PATH`, plus `PATHEXT` on Windows (where `npx` is `npx.cmd`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PathRuntimes;

impl RuntimeProbe for PathRuntimes {
    fn on_path(&self, program: &str) -> bool {
        found_on_path(
            program,
            std::env::var_os("PATH")
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
            std::env::var_os("PATHEXT")
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
            cfg!(windows),
            &|path: &Path| path.is_file(),
        )
    }
}

/// [`PathRuntimes`]'s pure core: is `program` on `path_var`, under any of `pathext`'s suffixes?
///
/// Windows is the whole reason this is not one line: the thing `npx` names there is `npx.cmd`,
/// found by trying each extension in `PATHEXT` (and the bare name last, for a real `.exe`-less
/// file). Elsewhere the bare name is the only spelling.
fn found_on_path(
    program: &str,
    path_var: Option<&str>,
    pathext: Option<&str>,
    windows: bool,
    exists: &dyn Fn(&Path) -> bool,
) -> bool {
    if program.is_empty() {
        return false;
    }
    let separator = if windows { ';' } else { ':' };
    let mut suffixes: Vec<String> = vec![String::new()];
    if windows {
        let ext = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
        suffixes.extend(
            ext.split(';')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned),
        );
    }
    // The path is JOINED by the target platform's separator, not the host's: this function is
    // exercised for Windows shapes on machines that are not Windows, and `Path::join` would put a
    // `/` in the middle of `C:\node\npx.CMD`.
    let joiner = if windows { '\\' } else { '/' };
    path_var.unwrap_or_default().split(separator).any(|dir| {
        if dir.is_empty() {
            return false;
        }
        let dir = dir.trim_end_matches(['/', '\\']);
        suffixes
            .iter()
            .any(|suffix| exists(&PathBuf::from(format!("{dir}{joiner}{program}{suffix}"))))
    })
}

/// Why nothing was placed for one bundle in one agent — a capability, never a fault. Each carries
/// the whole sentence a person reads; the placement is re-decided every sweep, so every one of
/// these clears itself the moment the machine (or the build) can do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Gap {
    /// The document offers a package from a registry this build has no runtime arm for.
    UnsupportedPackage { registry: String },
    /// The form this agent would get needs a program that is not installed here.
    MissingRuntime { tool: &'static str },
    /// The document leaves a value for each machine to fill in, and topos has nowhere to get it.
    Unfillable { name: String },
    /// The agent cannot take the shape this document offers at all.
    Unsupported { program: bool },
}

impl Gap {
    /// The machine-branchable code the message channel carries.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedPackage { .. } | Self::Unsupported { .. } => "MCP_KIND_UNSUPPORTED",
            Self::MissingRuntime { .. } => "MCP_RUNTIME_MISSING",
            Self::Unfillable { .. } => "MCP_VALUE_UNFILLABLE",
        }
    }

    /// The whole sentence, for the agent named by `harness`.
    pub(crate) fn message(&self, harness: &str) -> String {
        match self {
            Self::UnsupportedPackage { registry } => format!(
                "not placed: this bundle is packaged as {registry}, which this version of topos \
                 cannot set up yet."
            ),
            Self::MissingRuntime { tool } => format!(
                "not placed: needs {tool}, which is not on this machine. Install {tool}, then run \
                 'topos update'."
            ),
            Self::Unfillable { name } => format!(
                "not placed: this bundle asks for {name}, a value this version of topos cannot \
                 fill in."
            ),
            Self::Unsupported { program: true } => format!(
                "not placed in {harness}: this server runs as a program on this machine, and this \
                 version of topos cannot set that up in {harness}."
            ),
            Self::Unsupported { program: false } => format!(
                "not placed in {harness}: this server is an address, and this version of topos \
                 cannot set that up in {harness}."
            ),
        }
    }

    /// The SHORT cause a per-agent receipt row carries after the outcome word.
    pub(crate) fn note(&self) -> String {
        match self {
            Self::UnsupportedPackage { registry } => {
                format!("packaged as {registry}, which topos cannot set up yet")
            }
            Self::MissingRuntime { tool } => format!("needs {tool}, which is not on this machine"),
            Self::Unfillable { name } => format!("{name} is a value topos cannot fill in"),
            Self::Unsupported { program: true } => {
                "this agent does not run a server from this machine".to_owned()
            }
            Self::Unsupported { program: false } => {
                "this agent does not dial an address".to_owned()
            }
        }
    }
}

/// The npm runner topos invokes, and the thing a person installs to get it.
const NPX: &str = "npx";
const NODE: &str = "node";
/// The python runner topos invokes — the thing a person installs is spelled the same.
const UVX: &str = "uvx";

/// **The one selection** — see the module doc for the order, which this function is the whole
/// implementation of.
///
/// # Errors
/// One [`Gap`] naming, in plain words, why this agent gets nothing for this bundle.
pub(crate) fn select(
    doc: &ServerDoc,
    caps: HarnessCaps,
    bridge: Option<&McpBridge>,
    runtimes: &dyn RuntimeProbe,
    machine: Machine,
) -> Result<McpTarget, Gap> {
    // 1. The address, where the agent dials one itself.
    if let Some(remote) = &doc.remote {
        if caps.remote {
            return Ok(McpTarget::Remote {
                url: remote.url.clone(),
                headers: remote.headers.clone(),
            });
        }
        // 2. The bridge, where it does not.
        if caps.stdio
            && let Some(bridge) = bridge
        {
            return bridged(remote, bridge, runtimes, machine);
        }
    }
    // 3. The first package this build can set up — npm, then pypi.
    if caps.stdio {
        if let Some(package) = doc
            .packages
            .iter()
            .find(|p| p.registry == "npm")
            .or_else(|| doc.packages.iter().find(|p| p.registry == "pypi"))
        {
            return packaged(package, caps.env_ref, runtimes, machine);
        }
        // A package this build has no arm for: the honest capability line, naming its registry.
        if let Some(first) = doc.packages.first() {
            return Err(Gap::UnsupportedPackage {
                registry: first.registry.clone(),
            });
        }
    }
    // 4. Nothing this agent can take.
    Err(Gap::Unsupported {
        program: doc.remote.is_none(),
    })
}

/// The BRIDGE rendering: `npx -y mcp-remote@<pinned> <url>`, plus one `--header` per literal
/// header.
///
/// The header spelling is the one mcp-remote itself documents for Cursor and for Windows, where
/// spaces inside an argument are mangled before the program ever sees them: the argument carries
/// `Name:${VAR}` with no space at all, and the VALUE — spaces included — travels in the
/// environment, which mcp-remote expands itself. So the bridge's env spelling is always `${VAR}`,
/// whatever the harness's own reference syntax is: the program reading it is mcp-remote, not the
/// harness.
fn bridged(
    remote: &RemoteEndpoint,
    bridge: &McpBridge,
    runtimes: &dyn RuntimeProbe,
    machine: Machine,
) -> Result<McpTarget, Gap> {
    if !runtimes.on_path(NPX) {
        return Err(Gap::MissingRuntime { tool: NODE });
    }
    let mut args = vec!["-y".to_owned(), bridge.spec(), remote.url.clone()];
    let mut env = Vec::with_capacity(remote.headers.len());
    for (name, value) in &remote.headers {
        let var = header_var(name);
        args.push("--header".to_owned());
        args.push(format!("{name}:${{{var}}}"));
        env.push((var, value.clone()));
    }
    let (command, args) = windows_wrap(NPX, &args, machine.windows, machine.wsl_dest);
    Ok(McpTarget::Local { command, args, env })
}

/// The environment variable one bridged header's value travels in — deterministic, and named so a
/// person reading the config can see which header it belongs to.
fn header_var(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("TOPOS_HEADER_{sanitized}")
}

/// The PACKAGE renderings — one arm per registry this build can run:
///
/// - `npm` → `npx -y <identifier>@<version>`
/// - `pypi` → `uvx <identifier>==<version>`
///
/// The version is ALWAYS the one the document pins (the publish gate refuses a range and refuses
/// `latest`, per registry type), so every machine in a workspace installs the same bytes.
fn packaged(
    package: &PackageRef,
    env_ref: EnvRef,
    runtimes: &dyn RuntimeProbe,
    machine: Machine,
) -> Result<McpTarget, Gap> {
    let (runner, tool, spec) = match package.registry.as_str() {
        "npm" => (
            NPX,
            NODE,
            format!("{}@{}", package.identifier, package.version),
        ),
        "pypi" => (
            UVX,
            UVX,
            format!("{}=={}", package.identifier, package.version),
        ),
        other => {
            return Err(Gap::UnsupportedPackage {
                registry: other.to_owned(),
            });
        }
    };
    if !runtimes.on_path(runner) {
        return Err(Gap::MissingRuntime { tool });
    }
    let mut args: Vec<String> = Vec::new();
    if runner == NPX {
        args.push("-y".to_owned());
    }
    for argument in &package.runtime_args {
        push_argument(&mut args, argument)?;
    }
    args.push(spec);
    for argument in &package.package_args {
        push_argument(&mut args, argument)?;
    }
    let mut env = Vec::with_capacity(package.env.len());
    for slot in &package.env {
        let value = match &slot.value {
            // A stated literal travels as it is…
            Some(value) if !is_templated(value) => value.clone(),
            // …a template is a value a person fills in, and topos has nowhere to get it…
            Some(_) => {
                return Err(Gap::Unfillable {
                    name: slot.name.clone(),
                });
            }
            // …and an EMPTY slot is the machine's own environment, referenced by name in the
            // spelling this harness reads.
            None => env_ref.render(&slot.name),
        };
        env.push((slot.name.clone(), value));
    }
    let (command, args) = windows_wrap(runner, &args, machine.windows, machine.wsl_dest);
    Ok(McpTarget::Local { command, args, env })
}

/// One structured argument as argv. A named argument is its flag and (when the document states
/// one) its value; a positional one is its value alone. An argument the document leaves for a
/// person to fill in stops the placement instead of being dropped — a command missing an argument
/// is a server that starts and does the wrong thing.
fn push_argument(args: &mut Vec<String>, argument: &Argument) -> Result<(), Gap> {
    let unfillable = |what: &str| Gap::Unfillable {
        name: what.to_owned(),
    };
    match (&argument.name, &argument.value) {
        (name, Some(value)) if is_templated(value) => {
            Err(unfillable(if name.is_empty() { value } else { name }))
        }
        (name, Some(value)) => {
            if !name.is_empty() {
                args.push(name.clone());
            }
            args.push(value.clone());
            Ok(())
        }
        // A flag with no value is a flag.
        (name, None) if !name.is_empty() => {
            args.push(name.clone());
            Ok(())
        }
        // A positional slot with nothing in it is a value only a person can supply.
        (_, None) => Err(unfillable("a value this bundle does not state")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topos_harness::mcp::BRIDGE_PACKAGE;

    /// A probe that answers for a stated set of programs — nothing here reads the machine.
    struct Installed(&'static [&'static str]);

    impl RuntimeProbe for Installed {
        fn on_path(&self, program: &str) -> bool {
            self.0.contains(&program)
        }
    }

    const EVERYTHING: Installed = Installed(&[NPX, UVX]);
    const NOTHING: Installed = Installed(&[]);

    const BRIDGE: McpBridge = McpBridge {
        package: BRIDGE_PACKAGE,
        version: "0.1.38",
    };

    fn caps(remote: bool, stdio: bool) -> HarnessCaps {
        HarnessCaps {
            remote,
            stdio,
            env_ref: EnvRef::DollarBrace,
        }
    }

    fn doc(json: &str) -> ServerDoc {
        parse_server_json(json.as_bytes()).expect("parses")
    }

    fn local(target: &McpTarget) -> (String, Vec<String>, Vec<(String, String)>) {
        match target {
            McpTarget::Local { command, args, env } => (command.clone(), args.clone(), env.clone()),
            McpTarget::Remote { .. } => panic!("expected a program, got an address"),
        }
    }

    const NPM_ONLY: &str = r#"{
      "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
      "packages": [{
        "registryType": "npm", "identifier": "@acme/server", "version": "2.1.0",
        "transport": {"type": "stdio"},
        "environmentVariables": [
          {"name": "ACME_REGION", "value": "eu"},
          {"name": "ACME_TOKEN", "isSecret": true}
        ]
      }]
    }"#;

    const REMOTE_ONLY: &str = r#"{
      "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
      "remotes": [{"type": "streamable-http", "url": "https://mcp.example/x"}]
    }"#;

    /// The order, rule by rule, over one document that offers BOTH shapes.
    #[test]
    fn the_selection_order_is_the_documents_offer_crossed_with_the_agents_reach() {
        let both = doc(&format!(
            r#"{{
              "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
              "remotes": [{{"type": "streamable-http", "url": "https://mcp.example/x"}}],
              "packages": {}
            }}"#,
            serde_json::to_string(&serde_json::from_str::<Value>(NPM_ONLY).unwrap()["packages"])
                .unwrap()
        ));

        // 1. An agent that dials gets the address, even though a package is on offer.
        let target = select(
            &both,
            caps(true, true),
            Some(&BRIDGE),
            &EVERYTHING,
            Machine::default(),
        )
        .expect("placed");
        assert!(matches!(target, McpTarget::Remote { .. }));

        // 2. An agent that does NOT dial gets the bridge — still the address, reached the one way
        //    this agent can reach it.
        let target = select(
            &both,
            caps(false, true),
            Some(&BRIDGE),
            &EVERYTHING,
            Machine::default(),
        )
        .expect("placed");
        let (command, args, env) = local(&target);
        assert_eq!(command, NPX);
        assert_eq!(args, ["-y", "mcp-remote@0.1.38", "https://mcp.example/x"]);
        assert!(env.is_empty());

        // 3. No address on offer: the package, for any agent that runs programs.
        let packaged = doc(NPM_ONLY);
        let target = select(
            &packaged,
            caps(true, true),
            Some(&BRIDGE),
            &EVERYTHING,
            Machine::default(),
        )
        .expect("placed");
        let (command, args, _) = local(&target);
        assert_eq!(command, NPX);
        assert_eq!(args, ["-y", "@acme/server@2.1.0"]);

        // 4. …and nothing at all for an agent that cannot run one.
        assert_eq!(
            select(
                &packaged,
                caps(true, false),
                Some(&BRIDGE),
                &EVERYTHING,
                Machine::default()
            ),
            Err(Gap::Unsupported { program: true })
        );
        // An address-only document meets an agent that neither dials nor runs.
        assert_eq!(
            select(
                &doc(REMOTE_ONLY),
                caps(false, false),
                Some(&BRIDGE),
                &EVERYTHING,
                Machine::default()
            ),
            Err(Gap::Unsupported { program: false })
        );
        // A bridge-less build cannot bridge, and says the same honest thing.
        assert_eq!(
            select(
                &doc(REMOTE_ONLY),
                caps(false, true),
                None,
                &EVERYTHING,
                Machine::default()
            ),
            Err(Gap::Unsupported { program: false })
        );
    }

    /// The two translation arms, with the pinned version and the structured arguments the official
    /// format states, and npm PREFERRED over pypi when a document offers both.
    #[test]
    fn the_npm_and_pypi_arms_render_the_pinned_version_and_the_stated_arguments() {
        let with_args = doc(r#"{
              "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
              "packages": [
                {"registryType": "pypi", "identifier": "acme-server", "version": "3.0.1",
                 "transport": {"type": "stdio"},
                 "packageArguments": [
                   {"type": "named", "name": "--mode", "value": "read"},
                   {"type": "named", "name": "--verbose"},
                   {"type": "positional", "value": "run"}
                 ],
                 "environmentVariables": [{"name": "ACME_HOME", "default": "/srv"}]}
              ]
            }"#);
        let target = select(
            &with_args,
            caps(false, true),
            None,
            &EVERYTHING,
            Machine::default(),
        )
        .expect("placed");
        let (command, args, env) = local(&target);
        assert_eq!(command, UVX);
        assert_eq!(
            args,
            ["acme-server==3.0.1", "--mode", "read", "--verbose", "run"]
        );
        assert_eq!(env, [("ACME_HOME".to_owned(), "/srv".to_owned())]);

        // npm first when both are offered, whatever the document's order.
        let both = doc(r#"{
              "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
              "packages": [
                {"registryType": "pypi", "identifier": "acme-server", "version": "3.0.1",
                 "transport": {"type": "stdio"}},
                {"registryType": "npm", "identifier": "@acme/server", "version": "2.1.0",
                 "transport": {"type": "stdio"},
                 "runtimeArguments": [{"type": "named", "name": "--node-option", "value": "x"}]}
              ]
            }"#);
        let (command, args, _) = local(
            &select(
                &both,
                caps(false, true),
                None,
                &EVERYTHING,
                Machine::default(),
            )
            .expect("placed"),
        );
        assert_eq!(command, NPX);
        assert_eq!(
            args,
            ["-y", "--node-option", "x", "@acme/server@2.1.0"],
            "runtime arguments precede the package, package arguments follow it"
        );
    }

    /// An empty environment slot is the machine's own environment, referenced in the spelling the
    /// harness reads — the name travels in the bundle, the value never does.
    #[test]
    fn an_empty_environment_slot_renders_as_this_agents_own_reference_spelling() {
        let packaged = doc(NPM_ONLY);
        let env_for = |env_ref| {
            let target = select(
                &packaged,
                HarnessCaps {
                    remote: false,
                    stdio: true,
                    env_ref,
                },
                None,
                &EVERYTHING,
                Machine::default(),
            )
            .expect("placed");
            local(&target).2
        };
        assert_eq!(
            env_for(EnvRef::DollarBrace),
            [
                ("ACME_REGION".to_owned(), "eu".to_owned()),
                ("ACME_TOKEN".to_owned(), "${ACME_TOKEN}".to_owned()),
            ]
        );
        assert_eq!(
            env_for(EnvRef::BraceEnv)[1].1,
            "{env:ACME_TOKEN}",
            "opencode spells it its own way"
        );
        assert_eq!(env_for(EnvRef::DollarBraceEnv)[1].1, "${env:ACME_TOKEN}");
    }

    /// The bridge carries headers the one way that survives Cursor and Windows: no space in the
    /// argument, the value in the environment, expanded by mcp-remote itself.
    #[test]
    fn a_bridged_remote_carries_its_headers_through_the_environment() {
        let with_headers = doc(r#"{
              "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
              "remotes": [{"type": "streamable-http", "url": "https://mcp.example/x",
                "headers": [
                  {"name": "X-Acme-Tenant", "value": "acme corp"},
                  {"name": "X-Acme-Mode", "value": "read"}
                ]}]
            }"#);
        let target = select(
            &with_headers,
            caps(false, true),
            Some(&BRIDGE),
            &EVERYTHING,
            Machine::default(),
        )
        .expect("placed");
        let (command, args, env) = local(&target);
        assert_eq!(command, NPX);
        assert_eq!(
            args,
            [
                "-y",
                "mcp-remote@0.1.38",
                "https://mcp.example/x",
                "--header",
                "X-Acme-Tenant:${TOPOS_HEADER_X_ACME_TENANT}",
                "--header",
                "X-Acme-Mode:${TOPOS_HEADER_X_ACME_MODE}",
            ]
        );
        assert_eq!(
            env,
            [
                (
                    "TOPOS_HEADER_X_ACME_TENANT".to_owned(),
                    "acme corp".to_owned()
                ),
                ("TOPOS_HEADER_X_ACME_MODE".to_owned(), "read".to_owned()),
            ],
            "the value — space and all — travels in the environment, never in the argument"
        );

        // An oauth-hinted remote bridges exactly the same way: mcp-remote runs the sign-in.
        let oauth = doc(r#"{
              "name": "io.github.acme/server", "description": "d", "version": "1.0.0",
              "_meta": {"sh.topos/auth": "oauth"},
              "remotes": [{"type": "streamable-http", "url": "https://mcp.example/x"}]
            }"#);
        assert_eq!(oauth.auth, AuthHint::Oauth);
        let target = select(
            &oauth,
            caps(false, true),
            Some(&BRIDGE),
            &EVERYTHING,
            Machine::default(),
        )
        .expect("placed");
        let (_, args, env) = local(&target);
        assert_eq!(args, ["-y", "mcp-remote@0.1.38", "https://mcp.example/x"]);
        assert!(
            env.is_empty(),
            "no credential, and nothing to stand in for one"
        );
    }

    /// The two capability lines the brief spells, byte for byte, plus the two this build adds for
    /// the shapes it cannot fill in or place.
    #[test]
    fn every_capability_line_says_what_a_person_can_do_about_it() {
        let packaged = doc(NPM_ONLY);
        // A runtime that is not here.
        let gap = select(
            &packaged,
            caps(false, true),
            None,
            &NOTHING,
            Machine::default(),
        )
        .expect_err("no node");
        assert_eq!(gap, Gap::MissingRuntime { tool: NODE });
        assert_eq!(
            gap.message("cursor"),
            "not placed: needs node, which is not on this machine. Install node, then run 'topos update'."
        );
        // …and the same for the bridge, which is a node program too.
        assert_eq!(
            select(
                &doc(REMOTE_ONLY),
                caps(false, true),
                Some(&BRIDGE),
                &NOTHING,
                Machine::default()
            ),
            Err(Gap::MissingRuntime { tool: NODE })
        );
        // A pypi document with no uvx names uvx.
        let pypi = doc(
            r#"{"name": "io.github.acme/server", "description": "d", "version": "1.0.0",
                "packages": [{"registryType": "pypi", "identifier": "acme-server",
                              "version": "3.0.1", "transport": {"type": "stdio"}}]}"#,
        );
        assert_eq!(
            select(&pypi, caps(false, true), None, &NOTHING, Machine::default()),
            Err(Gap::MissingRuntime { tool: UVX })
        );

        // A registry this build has no arm for.
        let oci = doc(
            r#"{"name": "io.github.acme/server", "description": "d", "version": "1.0.0",
                "packages": [{"registryType": "oci",
                              "identifier": "ghcr.io/acme/server@sha256:aaaa",
                              "transport": {"type": "stdio"}}]}"#,
        );
        let gap = select(
            &oci,
            caps(false, true),
            None,
            &EVERYTHING,
            Machine::default(),
        )
        .expect_err("no oci arm");
        assert_eq!(
            gap.message("cursor"),
            "not placed: this bundle is packaged as oci, which this version of topos cannot set up yet."
        );

        // A value only a person can supply.
        let templated = doc(
            r#"{"name": "io.github.acme/server", "description": "d", "version": "1.0.0",
                "packages": [{"registryType": "npm", "identifier": "@acme/server",
                              "version": "2.1.0", "transport": {"type": "stdio"},
                              "environmentVariables": [{"name": "ACME_DIR", "value": "{workspace}"}]}]}"#,
        );
        let gap = select(
            &templated,
            caps(false, true),
            None,
            &EVERYTHING,
            Machine::default(),
        )
        .expect_err("templated");
        assert_eq!(
            gap.message("cursor"),
            "not placed: this bundle asks for ACME_DIR, a value this version of topos cannot fill in."
        );

        // An agent that cannot take the shape at all.
        assert_eq!(
            select(
                &packaged,
                caps(true, false),
                None,
                &EVERYTHING,
                Machine::default()
            )
            .expect_err("no stdio")
            .message("hermes-agent"),
            "not placed in hermes-agent: this server runs as a program on this machine, and this \
             version of topos cannot set that up in hermes-agent."
        );
    }

    /// The Windows wrapper reaches the rendered command — and stops at a WSL destination.
    #[test]
    fn a_windows_machine_wraps_the_runner_unless_the_config_lives_in_wsl() {
        let packaged = doc(NPM_ONLY);
        let render = |machine| {
            let target =
                select(&packaged, caps(false, true), None, &EVERYTHING, machine).expect("placed");
            let (command, args, _) = local(&target);
            (command, args)
        };
        let (command, args) = render(Machine {
            windows: true,
            wsl_dest: false,
        });
        assert_eq!(command, "cmd");
        assert_eq!(args, ["/c", "npx", "-y", "@acme/server@2.1.0"]);
        let (command, args) = render(Machine {
            windows: true,
            wsl_dest: true,
        });
        assert_eq!(command, NPX);
        assert_eq!(args, ["-y", "@acme/server@2.1.0"]);
        // `Machine::at` reads the destination it is given.
        assert!(Machine::at(Path::new(r"\\wsl$\Ubuntu\home\me\.cursor\mcp.json")).wsl_dest);
        assert!(!Machine::at(Path::new("/home/me/.cursor/mcp.json")).wsl_dest);
    }

    /// The PATH lookup, including the Windows extension dance that makes `npx` findable at all.
    #[test]
    fn a_runtime_is_found_by_the_name_its_platform_spells_it_with() {
        let present = |paths: &'static [&'static str]| {
            move |p: &Path| paths.contains(&p.to_string_lossy().as_ref())
        };
        // POSIX: the bare name, in one of the PATH dirs.
        assert!(found_on_path(
            "npx",
            Some("/nope:/usr/local/bin"),
            None,
            false,
            &present(&["/usr/local/bin/npx"])
        ));
        assert!(!found_on_path(
            "npx",
            Some("/nope:/usr/local/bin"),
            None,
            false,
            &present(&["/usr/local/bin/uvx"])
        ));
        assert!(!found_on_path("npx", None, None, false, &present(&[])));
        assert!(!found_on_path("", Some("/usr/bin"), None, false, &|_| true));

        // Windows: `npx` is `npx.cmd`, found through PATHEXT, and the separator is `;`.
        assert!(found_on_path(
            "npx",
            Some(r"C:\nope;C:\node"),
            Some(".COM;.EXE;.BAT;.CMD"),
            true,
            &present(&[r"C:\node\npx.CMD"])
        ));
        assert!(!found_on_path(
            "npx",
            Some(r"C:\node"),
            Some(".EXE"),
            true,
            &present(&[r"C:\node\npx.CMD"])
        ));
        // A PATHEXT-less Windows still tries the defaults.
        assert!(found_on_path(
            "npx",
            Some(r"C:\node"),
            None,
            true,
            &present(&[r"C:\node\npx.CMD"])
        ));
    }

    /// The document read: both shapes, the auth word, and the header re-check that fails the whole
    /// demand closed rather than placing something the gate never validated.
    #[test]
    fn the_document_is_read_for_what_it_offers_and_refuses_what_the_gate_would_have() {
        let both = doc(
            r#"{"name": "io.github.acme/server", "description": "d", "version": "1.0.0",
                "remotes": [{"type": "sse", "url": "https://mcp.example/sse"},
                            {"type": "streamable-http", "url": "https://mcp.example/x"}],
                "packages": [{"registryType": "npm", "identifier": "@acme/server",
                              "version": "2.1.0", "transport": {"type": "stdio"}}]}"#,
        );
        assert_eq!(
            both.remote.as_ref().map(|r| r.url.as_str()),
            Some("https://mcp.example/x"),
            "the FIRST streamable-http remote, exactly as the gate picks it"
        );
        assert_eq!(both.packages.len(), 1);
        assert_eq!(
            both.canonical_identity().as_deref(),
            Some("https://mcp.example/x")
        );
        // A package-only document identifies itself by the package it pins.
        assert_eq!(
            doc(NPM_ONLY).canonical_identity().as_deref(),
            Some("npm:@acme/server@2.1.0")
        );

        for (bad, says) in [
            (
                r#"{"remotes": [{"type": "streamable-http", "url": "https://x",
                    "headers": [{"name": "X-T", "isSecret": true, "value": "v"}]}]}"#,
                "marked secret",
            ),
            (
                r#"{"remotes": [{"type": "streamable-http", "url": "https://x",
                    "headers": [{"name": "X-T", "value": "{tenant}"}]}]}"#,
                "templated value",
            ),
            (
                r#"{"remotes": [{"type": "streamable-http", "url": "https://x",
                    "headers": [{"name": "X-T"}]}]}"#,
                "no literal value",
            ),
            (
                r#"{"packages": [{"registryType": "npm"}]}"#,
                "names no registry or no package",
            ),
            (
                r#"{"name": "x"}"#,
                "neither an address to dial nor a package to run",
            ),
            ("not json", "not valid JSON"),
        ] {
            let e = parse_server_json(bad.as_bytes()).expect_err(bad);
            assert!(e.contains(says), "{bad}: {e}");
        }
    }
}
