//! `mcp` — pure MCP-server config placement for six harnesses. Bytes in → an [`EditPlan`] out;
//! the CLI owns ALL file I/O (read, crash-safe write, dir materialization) exactly as it does for
//! the trigger adapters. Nothing here touches a filesystem, a clock, or the environment (the one
//! exception: resolving a harness's user surface reads the per-harness home-override env vars,
//! through the registry's one resolver).
//!
//! ## The ownership model — a fingerprint ledger, not a marker comment
//!
//! JSON and TOML have no safe comment slot next to an entry, so ownership is keyed on TWO facts
//! together: the entry key's `topos-` prefix (managed-LOOKING) and the caller's ledger of
//! `key → fingerprint` for what topos LAST WROTE (`prior`). A **fingerprint** is the sha256 hex
//! of a canonical structural rendering of the entry's parsed value (keys sorted, serialized
//! compact) — whitespace/key-order reflow never changes it; any added/changed/removed field does.
//! The Hermes YAML dialect additionally carries the ` # topos:mcp` line sentinel (YAML has a
//! comment slot; see `yaml_splice`).
//!
//! ## The drift rules (uniform across every dialect — [`EntryState`])
//!
//! - Desired + absent → **PlacedNew** (inserted).
//! - Desired + present, fingerprint already the desired one → **Current** (byte-untouched).
//! - Desired + present + prior-matched + different desired value → **Updated** (replaced).
//! - Present + prior-MISmatched → **Drifted**: user-edited since topos wrote it — never
//!   overwritten, never removed, left byte-identical. The returned ledger keeps the OLD prior
//!   fingerprint so the drift classification survives re-runs.
//! - Present, `topos-`-looking, NO prior record → **Foreign**: someone else's — untouched, and
//!   never entered into the ledger.
//! - Present + prior-matched + NOT desired → **Removed** (how removal converges).
//! - A prior key absent from the file: reported nowhere (the caller notices via the returned
//!   `fingerprints` no longer carrying it).
//!
//! Applying the same desired set twice yields [`EditPlan::Leave`] the second time with the file
//! byte-identical — every driver is idempotent by construction and asserts it in tests.
//!
//! ## Fail-closed
//!
//! Any input that cannot be PROVEN safe to edit yields [`EditPlan::Unprovable`] with ZERO byte
//! changes — an unparsable file, a wrong-typed key path, a JSON extension the harness itself
//! rejects, a YAML shape outside the provable grammar. On `Unprovable` the `states` and
//! `fingerprints` are empty and the caller MUST keep its existing ledger unchanged. Every driver
//! also post-verifies its own edit (re-parse + structural comparison outside the managed
//! entries) and downgrades to `Unprovable` on ANY surprise — a verification failure never writes.
//!
//! ## The per-dialect wire shapes (exact — a wrong key can brick a harness)
//!
//! Rendering lives in [`entry_value`]; the per-driver module docs carry the placement mechanics.
//! Empty `headers` (and an empty `env`) are omitted in every dialect (an empty map carries no
//! information and strict validators are the risk surface).
//!
//! An entry has one of TWO targets ([`McpTarget`]): an ADDRESS the harness dials, or a PROGRAM it
//! runs on this machine. Both are ordinary managed entries — same key contract, same fingerprint
//! ledger, same drift rules — and the CALLER decides which one a given harness gets, rendering
//! every machine-local spelling (the bridge argv, the Windows `cmd /c` wrapper) before it builds
//! the entry. What it does NOT resolve is an environment slot the machine fills: that arrives as a
//! name ([`EnvValue::Inherited`]) and is spelled HERE, because the form differs by more than
//! syntax — most harnesses read a reference inside the value, Codex names inherited variables in a
//! list of its own. [`entry_value`] answers `None` for the one pair no vendor evidence covers — a
//! program in Hermes's YAML — and every driver turns that into a refusal rather than a guess.

#[cfg(test)]
mod breadth;
pub mod descriptor;
pub mod jsonc_edit;
pub mod plugin_dir;
pub mod toml_patch;
pub mod yaml_splice;

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use descriptor::McpDialect;

/// One desired managed entry — harness-neutral in its OWNERSHIP half (the key), already
/// harness-DECIDED in its target: the caller picks between the two shapes per harness (an address
/// the harness dials itself, or the program this machine runs), and renders any per-harness
/// spelling — the bridge argv, a Windows wrapper, an env-var reference — before building one.
#[derive(Debug, Clone)]
pub struct McpEntry {
    /// The immutable config key, e.g. `topos-acme-linear`. Charset `[a-z0-9-]` and the `topos-`
    /// prefix are caller-guaranteed — and re-verified by every driver (fail-closed), because the
    /// prefix is what later runs key managed-LOOKING recognition on.
    pub key: String,
    /// What the entry points the harness at.
    pub target: McpTarget,
    pub auth: AuthHint,
}

/// The two shapes an MCP entry can have. A bundle's `server.json` may offer an ADDRESS every
/// machine dials, a PACKAGE every machine runs, or both; which of the two a given harness gets is
/// the caller's decision, and this is where it is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTarget {
    /// An endpoint the harness connects to over https.
    Remote {
        url: String,
        /// Literal, non-secret header pairs (already gate-validated by the caller). Emitted
        /// sorted by name in every dialect so output bytes are deterministic.
        headers: Vec<(String, String)>,
    },
    /// A program the harness RUNS on this machine, talking MCP over stdio. `command` and `args`
    /// are exactly what gets spawned (any Windows `cmd /c` wrapper is already applied); `env`
    /// carries a literal or a NAME to inherit — never a credential.
    Local {
        command: String,
        args: Vec<String>,
        /// Environment slots, emitted sorted by name so output bytes are deterministic.
        env: Vec<(String, EnvValue)>,
        /// How THIS harness spells a reference to a variable of the machine's own environment,
        /// for the dialects that spell one in the value at all. It travels with the entry because
        /// it is a fact about the harness, not about the dialect: two harnesses can share a
        /// dialect and read different spellings.
        env_ref: descriptor::EnvRef,
    },
}

/// **What one environment slot holds** — the distinction a config file has to keep, because the
/// two are not the same word to every harness.
///
/// A LITERAL is a value the bundle states and every machine gets. An INHERITED slot is a name and
/// nothing else: the value is whatever this machine's own environment holds under it, which is how
/// a shared bundle names a credential slot without ever carrying a credential.
///
/// Most harnesses spell inheritance IN the value — `${VAR}`, `{env:VAR}` — and for them the
/// difference is a rendering. Codex does not: its `env` map is literal text passed to the child
/// verbatim, and inheritance is a separate `env_vars` list of names it forwards from its own
/// environment. Writing `${VAR}` into its `env` would hand the server those six characters. So the
/// distinction is carried to the renderer rather than resolved before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    /// The value the bundle states, verbatim.
    Literal(String),
    /// This machine's own value for the slot's name.
    Inherited,
}

impl McpEntry {
    /// The endpoint this entry dials, when it dials one at all.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match &self.target {
            McpTarget::Remote { url, .. } => Some(url.as_str()),
            McpTarget::Local { .. } => None,
        }
    }

    /// The ONE spelling of the server this entry names — [`canonical_address`] for a remote,
    /// [`local_address`] for a program (which unwraps a bridged remote back to its URL). It is
    /// what a collision question compares, and it is answered the same way for an entry topos
    /// would write and an entry it merely found.
    #[must_use]
    pub fn address(&self) -> Option<String> {
        match &self.target {
            McpTarget::Remote { url, .. } => canonical_address(url),
            McpTarget::Local { command, args, .. } => local_address(command, args),
        }
    }
}

/// What the caller knows about the server's auth story. Two dialects render it — Hermes and
/// OpenClaw, both of which gate their whole OAuth path on an explicit `auth: oauth` key and do
/// nothing on a 401 without it. It is emitted for [`AuthHint::Oauth`] ALONE: a no-auth server
/// must never be sent into a sign-in flow. Everywhere else OAuth is the harness's own on-401
/// behavior and the key does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthHint {
    None,
    Oauth,
    Unknown,
}

/// What an apply decided to do to the file. `Unprovable` carries an honest human reason and
/// guarantees zero byte changes were planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPlan {
    Write(Vec<u8>),
    Leave,
    Unprovable(String),
}

/// Per-key outcome of an apply. See the module doc for the full rules; `Removed` completes the
/// contract for a prior-matched entry the desired set no longer contains (nothing else can name
/// that outcome, and the receipt needs it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    PlacedNew,
    Current,
    Updated,
    Drifted,
    Foreign,
    Removed,
}

/// The outcome of one apply over one config surface.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub plan: EditPlan,
    /// `(key, state)` for every desired key AND every managed-looking key found in the file —
    /// desired keys first (in the caller's order), then the extra found keys (sorted). Empty on
    /// [`EditPlan::Unprovable`].
    pub states: Vec<(String, EntryState)>,
    /// `(key, fingerprint)` for every entry the plan leaves placed AS TOPOS'S — what the caller's
    /// ledger records next. A `Drifted` entry keeps its prior fingerprint (so drift persists); a
    /// `Foreign` entry never enters. Meaningless on [`EditPlan::Unprovable`] (keep the old
    /// ledger).
    pub fingerprints: Vec<(String, String)>,
    /// Whether the planned output is a WHOLE-FILE creation (the surface was absent, or held only
    /// whitespace) — the caller records whole-file ownership so it may delete the file later,
    /// but only while it still owns every byte.
    pub created_file: bool,
}

/// A read-only view of one config surface, for status/drift reads without writing.
#[derive(Debug, Clone)]
pub struct Observed {
    /// `key → fingerprint` for every managed-looking entry found (an absent surface is simply
    /// empty).
    pub entries: BTreeMap<String, String>,
    /// Whether the surface was readable in this dialect's provable shape. `false` means the
    /// entries are unknowable, not absent.
    pub parseable: bool,
}

/// ONE entry as a surface HOLDS it, whoever wrote it — the observation a collision question is
/// asked of. Deliberately two fields: a name to compare against the key topos would mint, and the
/// server it points at. Nothing else about a foreign entry is parsed, or would be honest to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenEntry {
    /// The entry's key in the surface's own slot.
    pub name: String,
    /// The server address the entry names, canonicalized ([`canonical_address`]). `None` when the
    /// entry carries no address this build can read — a command-run server, a shape nobody here
    /// models, a value that is not a URL. Two `None`s are never a match.
    pub address: Option<String>,
}

/// **The ONE spelling of a server address**, so "the same server" is a question with an answer:
/// scheme and host lowercased, a default port (80 on http, 443 on https) dropped, a bare root path
/// (`/`) equal to none at all. Query and fragment are SIGNIFICANT — plenty of MCP endpoints
/// distinguish tenants that way, and calling two of them one server would refuse a placement
/// nobody's entry actually conflicts with.
///
/// `None` for anything that is not an absolute `scheme://host` URL: an address nothing can be
/// compared against never matches, including another one just like it.
#[must_use]
pub fn canonical_address(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    // The authority runs to the first `/`, `?` or `#`; everything after it is kept verbatim.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() {
        return None;
    }
    // Userinfo keeps its case (it is a credential, not a name); the host does not.
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    if hostport.is_empty() {
        return None;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        // An IPv6 literal's colons live inside the brackets — a `]` after the split point means
        // this colon was one of them, not a port separator.
        Some((h, p)) if !p.contains(']') => (h, Some(p)),
        _ => (hostport, None),
    };
    let host = host.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some("80"),
        "https" => Some("443"),
        _ => None,
    };
    let port = match port {
        Some(p) if Some(p) == default_port || p.is_empty() => String::new(),
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    // A bare root is no path at all; a deeper path keeps every byte, trailing slash included.
    let tail = if tail == "/" { "" } else { tail };
    let userinfo = userinfo.map_or_else(String::new, |u| format!("{u}@"));
    Some(format!("{scheme}://{userinfo}{host}{port}{tail}"))
}

// ---------------------------------------------------------------------------------------------
// Machine-local rendering facts: the Windows wrapper, the bridge argv, and the local address.
// ---------------------------------------------------------------------------------------------

/// The package name of the remote-to-stdio bridge, for RECOGNITION — which argv is really a remote
/// server in disguise. The version topos itself runs is registry DATA (fenced to the bundled
/// table); recognition is version-blind on purpose, because a foreign entry may bridge the same
/// server through any version of it, or through none at all.
pub const BRIDGE_PACKAGE: &str = "mcp-remote";

/// The commands that are BATCH FILES on Windows (`npx.cmd`, `node.exe`'s shims, …) and therefore
/// cannot be spawned directly: Windows needs `cmd /c` in front of them. Ported from cc-switch
/// (MIT, see THIRD-PARTY-NOTICES.md), whose list is the one Claude Code's own doctor asks for.
pub const WINDOWS_WRAPPED_COMMANDS: &[&str] =
    &["npx", "npm", "yarn", "pnpm", "node", "bun", "deno"];

/// The Windows `cmd /c` wrapper: `npx -y x` → `cmd` `["/c", "npx", "-y", "x"]`.
///
/// Applied ONLY on Windows, only to [`WINDOWS_WRAPPED_COMMANDS`] (matched on the file stem, so a
/// `C:\…\npx.cmd` spelling wraps too), and never twice — a command that is already `cmd` is left
/// alone. `wsl` says the config file being written lives inside a WSL distribution
/// (`\\wsl$\…`, `\\wsl.localhost\…`): the harness reading it runs Linux, where the wrapper would
/// be nonsense.
///
/// **Everything after `/c` is read TWICE**, and that is what [`cmd_escape`] is for: the harness
/// spawns `cmd.exe` with these arguments, `cmd` re-parses the command line it receives, and only
/// then does the real program get its own argv. A `&` in a bridged URL's query would end the
/// command there and run the rest as another one. Each wrapped element is therefore escaped for
/// that middle parse; the elements the wrapper itself adds (`/c`, and the command's own name) are
/// left alone, since one is a switch and the other is a program name Windows would not accept
/// with a metacharacter in it anyway.
#[must_use]
pub fn windows_wrap(
    command: &str,
    args: &[String],
    windows: bool,
    wsl: bool,
) -> (String, Vec<String>) {
    if !windows || wsl || !needs_windows_wrap(command) {
        return (command.to_owned(), args.to_vec());
    }
    let mut wrapped = Vec::with_capacity(args.len() + 2);
    wrapped.push("/c".to_owned());
    wrapped.push(command.to_owned());
    wrapped.extend(args.iter().map(|a| cmd_escape(a)));
    ("cmd".to_owned(), wrapped)
}

/// The characters `cmd.exe` acts on instead of passing through, when it meets them OUTSIDE double
/// quotes: the four that redirect or chain (`&`, `|`, `<`, `>`), the two that group, and the caret
/// that escapes any of them (and so must escape itself).
///
/// `%` is deliberately NOT here. Its expansion happens inside quotes as well as outside, so no
/// positional rule can protect it, and the only known counter — the `%%cd:~,%` no-op substitution —
/// needs the whole command line to be built by the escaper (it is not: a harness builds it) and
/// command extensions to be on. What `%` costs in practice is narrow: on a command line, `%NAME%`
/// is left verbatim unless NAME names a variable that is actually set, so percent-encoding in a URL
/// (`%20`) passes through untouched.
const CMD_METACHARACTERS: [char; 7] = ['^', '&', '|', '<', '>', '(', ')'];

/// **One argument, made safe for the extra `cmd /c` parse** — the escape that survives the whole
/// chain a config entry goes through on Windows.
///
/// Three readers see these bytes, in order, and the escape has to be right for all three:
///
/// 1. the harness SPAWNS `cmd.exe` with this argument list, and the spawn API encodes it into one
///    command line by the C runtime's rules: an element containing a space, a tab or a quote is
///    wrapped in double quotes (an inner quote written `\"`), everything else is written bare;
/// 2. `cmd.exe` parses that line — acting on `^ & | < > ( )` wherever they fall outside
///    double quotes, and consuming a caret to take the next character literally;
/// 3. the real program parses what `cmd` passed on, by the C runtime's rules again, which undo
///    step 1.
///
/// So the only characters that need escaping are the metacharacters that end up OUTSIDE the quotes
/// step 1 writes — and where those fall is decidable from the argument alone: an element with no
/// space, tab or quote in it is written bare and is outside quotes throughout; an element that gets
/// wrapped starts INSIDE the quotes, and each `"` in it flips that. Everything inside is already
/// literal to `cmd`, and a caret there would reach the program as a caret, so this escapes exactly
/// the outside ones and nothing else.
///
/// [`cmd_unescape`] is its inverse, so an entry read back off disk answers the same address
/// question as the one that wrote it.
/// The one place the three-reader chain is not decidable: an argument carrying a `"` but no space
/// is quoted by some spawners (libuv's, and therefore every Node harness) and written bare by
/// others (Rust's, which quotes on whitespace alone), which puts the same metacharacter inside the
/// quotes for one and outside for the other. The C-runtime rule below is the one the majority of
/// MCP harnesses spawn by; under the other, such an argument keeps a literal caret. Documented
/// rather than papered over — there is no encoding that is right for both.
#[must_use]
pub fn cmd_escape(arg: &str) -> String {
    let quoted_by_the_spawner = arg.is_empty() || arg.contains([' ', '\t', '"']);
    let mut inside = quoted_by_the_spawner;
    let mut out = String::with_capacity(arg.len());
    for c in arg.chars() {
        if !inside && CMD_METACHARACTERS.contains(&c) {
            out.push('^');
        }
        out.push(c);
        if c == '"' {
            inside = !inside;
        }
    }
    out
}

/// [`cmd_escape`]'s inverse: drop one caret before each character it protects. Applied when a
/// `cmd /c` wrapper is stripped, so the argv two entries are compared on is the argv the program
/// actually receives — an escaped URL and a bare one name one server, not two.
#[must_use]
pub fn cmd_unescape(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars();
    while let Some(c) = chars.next() {
        if c == '^'
            && let Some(next) = chars.clone().next()
            && CMD_METACHARACTERS.contains(&next)
        {
            chars.next();
            out.push(next);
            continue;
        }
        out.push(c);
    }
    out
}

/// Whether `command` is one of the shim commands Windows cannot spawn directly. The comparison is
/// on the file STEM (`npx`, `npx.cmd`, `C:\Program Files\nodejs\npx.cmd` all match) and
/// case-insensitive, as Windows paths are.
#[must_use]
pub fn needs_windows_wrap(command: &str) -> bool {
    if command.eq_ignore_ascii_case("cmd") || command.eq_ignore_ascii_case("cmd.exe") {
        return false; // already the wrapper
    }
    let base = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .split('.')
        .next()
        .unwrap_or(command);
    WINDOWS_WRAPPED_COMMANDS
        .iter()
        .any(|c| base.eq_ignore_ascii_case(c))
}

/// Whether `path` is a WSL UNC path — the one place a Windows topos must NOT write the `cmd /c`
/// wrapper, because the harness reading that file runs Linux. Only a direct UNC spelling is
/// detectable; a mapped drive letter pointing into WSL is not, and is documented as such by the
/// source this rule is ported from (cc-switch, MIT).
#[must_use]
pub fn is_wsl_unc(path: &str) -> bool {
    let Some(rest) = path
        .strip_prefix(r"\\")
        .or_else(|| path.strip_prefix(r"//"))
    else {
        return false;
    };
    let server = rest.split(['\\', '/']).next().unwrap_or_default();
    server.eq_ignore_ascii_case("wsl$") || server.eq_ignore_ascii_case("wsl.localhost")
}

/// `command` + `args` with any Windows `cmd /c` wrapper removed — the spelling two machines can be
/// compared on, since one may have written the wrapper and the other not.
///
/// Removing the wrapper also undoes its escaping ([`cmd_unescape`]), because the arguments that
/// reach the PROGRAM are the ones the extra `cmd` parse leaves behind: an entry that bridges
/// `https://x?a=1^&b=2` and one that bridges `https://x?a=1&b=2` reach the same server, and a
/// collision question that said otherwise would place a second entry beside the first.
#[must_use]
fn unwrapped(command: &str, args: &[String]) -> (String, Vec<String>) {
    let is_cmd = command.eq_ignore_ascii_case("cmd") || command.eq_ignore_ascii_case("cmd.exe");
    if is_cmd
        && let [flag, inner, rest @ ..] = args
        && flag.eq_ignore_ascii_case("/c")
    {
        return (
            inner.clone(),
            rest.iter().map(|a| cmd_unescape(a)).collect(),
        );
    }
    (command.to_owned(), args.to_vec())
}

/// **The ONE spelling of a program-run server's address** — the twin of [`canonical_address`] for
/// an entry that runs something instead of dialing something:
///
/// - a BRIDGED remote (`npx -y mcp-remote@x https://…`) answers with the URL it bridges,
///   canonicalized, because the server it reaches is the server that URL names — an entry that
///   bridges it and an entry that dials it are the same server twice;
/// - anything else answers with the command and its arguments, after the Windows `cmd /c` wrapper
///   is stripped, rendered as one JSON array so no argument can be confused with a boundary.
///
/// `None` for an empty command: nothing comparable never matches, including another one just like
/// it.
#[must_use]
pub fn local_address(command: &str, args: &[String]) -> Option<String> {
    let (command, args) = unwrapped(command, args);
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    if let Some(url) = bridged_url(command, &args) {
        return canonical_address(&url);
    }
    let mut parts: Vec<&str> = Vec::with_capacity(args.len() + 1);
    parts.push(command);
    parts.extend(args.iter().map(String::as_str));
    Some(format!("run:{}", Value::from(parts)))
}

/// The URL a bridge invocation carries, if this argv IS one: a node runner, the
/// [`BRIDGE_PACKAGE`] (bare or `@`-versioned) among its arguments, and an absolute URL after it.
/// Deliberately loose about the version and the flags around it — recognizing somebody else's
/// bridge entry is the whole point.
fn bridged_url(command: &str, args: &[String]) -> Option<String> {
    let runner = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .split('.')
        .next()
        .unwrap_or(command);
    if !runner.eq_ignore_ascii_case("npx") {
        return None;
    }
    let at = args.iter().position(|a| {
        let name = a.rsplit_once('@').map_or(a.as_str(), |(name, _)| {
            // A scoped package's leading `@` is part of the name, not a version separator.
            if name.is_empty() { a.as_str() } else { name }
        });
        name == BRIDGE_PACKAGE
    })?;
    args[at + 1..]
        .iter()
        .find(|a| a.contains("://"))
        .map(ToString::to_string)
}

/// **Every entry a surface holds, foreign ones included** — the observation the ownership ledger
/// cannot make on its own ([`observe`] answers only for managed-LOOKING keys, so an entry under
/// somebody else's name pointing at the same server is invisible to it).
///
/// `selector` names the slot when it is not the dialect's own: a `.`-separated path whose `*`
/// stands for every key at that level (`projects.*.mcpServers`). `None` reads the dialect's
/// native slot.
///
/// `None` means UNREADABLE — the surface could not be parsed in its dialect, or the selector
/// cannot be honoured here. It is not an empty answer: a caller may not conclude that nothing is
/// there.
#[must_use]
pub fn observe_entries(
    dialect: McpDialect,
    current: Option<&[u8]>,
    selector: Option<&str>,
) -> Option<Vec<SeenEntry>> {
    match dialect {
        McpDialect::ClaudeProjectJson
        | McpDialect::CursorJson
        | McpDialect::OpencodeJson
        | McpDialect::OpenclawJson
        | McpDialect::ClaudePluginDir
        | McpDialect::VscodeJson
        | McpDialect::CopilotCliJson
        | McpDialect::LmStudioJson
        | McpDialect::ClaudeDesktopJson
        | McpDialect::GeminiJson
        | McpDialect::WindsurfJson
        | McpDialect::ClineJson
        | McpDialect::RooJson
        | McpDialect::ZedJson => jsonc_edit::observe_entries(dialect, current, selector),
        McpDialect::CodexToml => toml_patch::observe_entries(current, selector),
        McpDialect::HermesYaml => yaml_splice::observe_entries(current, selector),
    }
}

/// The address inside one entry's parsed value, canonicalized. The key is whichever address
/// spelling the entry carries — harnesses disagree about the name (`url` here, `serverUrl` or
/// `httpUrl` or `uri` elsewhere), and an entry a foreign tool wrote uses ITS spelling, not ours.
///
/// An entry that RUNS something instead of dialing something answers with its
/// [`local_address`] — including the two spellings of a command line the dialects disagree
/// about: `command` + `args` nearly everywhere, and OpenCode's single `command` array.
pub(crate) fn entry_address(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    let dialed = ["url", "serverUrl", "httpUrl", "uri"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .and_then(canonical_address);
    if dialed.is_some() {
        return dialed;
    }
    let strings = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array).map_or_else(Vec::new, |a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
    };
    match obj.get("command") {
        Some(Value::String(command)) => local_address(command, &strings(obj.get("args"))),
        Some(Value::Array(_)) => {
            let argv = strings(obj.get("command"));
            let (command, args) = argv.split_first()?;
            local_address(command, args)
        }
        _ => None,
    }
}

/// Compute MCP placements for one config surface (routes to the driver). For
/// [`McpDialect::ClaudePluginDir`] pass the plugin dir's `.mcp.json` bytes — the strict JSON
/// driver patches it like any other surface; the constant manifest beside it
/// ([`plugin_dir::manifest_bytes`]) is the caller's I/O.
///
/// THE DISPATCHER-ENFORCED byte-preservation precondition: before any edit lands, the parsed
/// input must re-serialize byte-identical to the original through the driver's own editor —
/// otherwise a rewrite would silently normalize bytes the driver never modeled (a BOM, CRLF
/// line endings), so the plan is downgraded to [`EditPlan::Unprovable`] with an honest reason.
/// The guarantee lives HERE, not in per-driver discretion: a driver may check earlier for a
/// better message, but no `Write` leaves this function against a non-round-tripping input.
#[must_use]
pub fn apply(
    dialect: McpDialect,
    current: Option<&[u8]>,
    desired: &[McpEntry],
    prior: &BTreeMap<String, String>,
) -> ApplyOutcome {
    let outcome = match dialect {
        McpDialect::ClaudeProjectJson
        | McpDialect::CursorJson
        | McpDialect::OpencodeJson
        | McpDialect::OpenclawJson
        | McpDialect::ClaudePluginDir
        | McpDialect::VscodeJson
        | McpDialect::CopilotCliJson
        | McpDialect::LmStudioJson
        | McpDialect::ClaudeDesktopJson
        | McpDialect::GeminiJson
        | McpDialect::WindsurfJson
        | McpDialect::ClineJson
        | McpDialect::RooJson
        | McpDialect::ZedJson => jsonc_edit::apply(dialect, current, desired, prior),
        McpDialect::CodexToml => toml_patch::apply(current, desired, prior),
        McpDialect::HermesYaml => yaml_splice::apply(current, desired, prior),
    };
    // A `Write` over EXISTING content must be provably lossless outside the edit. A creation
    // (absent or whitespace-only input) has nothing to preserve.
    if matches!(outcome.plan, EditPlan::Write(_))
        && !effectively_absent(current)
        && !input_round_trips(dialect, current.unwrap_or_default())
    {
        return unprovable(
            "topos cannot rewrite this file without changing bytes it does not own (a BOM, or \
             unusual line endings), so it did not edit it",
        );
    }
    outcome
}

/// Whether `bytes` re-serialize byte-identical through `dialect`'s editor — the [`apply`]
/// dispatcher's precondition for any `Write` over existing content.
fn input_round_trips(dialect: McpDialect, bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    match dialect {
        McpDialect::ClaudeProjectJson
        | McpDialect::CursorJson
        | McpDialect::OpencodeJson
        | McpDialect::OpenclawJson
        | McpDialect::ClaudePluginDir
        | McpDialect::VscodeJson
        | McpDialect::CopilotCliJson
        | McpDialect::LmStudioJson
        | McpDialect::ClaudeDesktopJson
        | McpDialect::GeminiJson
        | McpDialect::WindsurfJson
        | McpDialect::ClineJson
        | McpDialect::RooJson
        | McpDialect::ZedJson => jsonc_edit::round_trips(dialect, text),
        McpDialect::CodexToml => toml_patch::round_trips(text),
        // The Hermes splicer is line-surgical: its "parse" IS the input lines, untouched lines
        // are carried verbatim by construction, and its own verification asserts the output
        // minus sentinel lines is byte-identical, in order, to the input minus sentinel lines.
        McpDialect::HermesYaml => true,
    }
}

/// Observe one config surface in its dialect (routes to the driver). For
/// [`McpDialect::ClaudePluginDir`] pass the plugin dir's `.mcp.json` bytes.
#[must_use]
pub fn observe(dialect: McpDialect, current: Option<&[u8]>) -> Observed {
    match dialect {
        McpDialect::ClaudeProjectJson
        | McpDialect::CursorJson
        | McpDialect::OpencodeJson
        | McpDialect::OpenclawJson
        | McpDialect::ClaudePluginDir
        | McpDialect::VscodeJson
        | McpDialect::CopilotCliJson
        | McpDialect::LmStudioJson
        | McpDialect::ClaudeDesktopJson
        | McpDialect::GeminiJson
        | McpDialect::WindsurfJson
        | McpDialect::ClineJson
        | McpDialect::RooJson
        | McpDialect::ZedJson => jsonc_edit::observe(dialect, current),
        McpDialect::CodexToml => toml_patch::observe(current),
        McpDialect::HermesYaml => yaml_splice::observe(current),
    }
}

/// Whether the surface holds ANY content beyond what topos itself writes: content outside the
/// dialect's managed slot (comments and extra keys included), or a non-managed-looking entry
/// inside it. The caller's whole-file-ownership reasoning — "may the file be DELETED when the
/// last entry leaves?" — consults this, so the answer FAILS TOWARD KEEPING: anything
/// indeterminate (unparseable, wrong-shaped) answers `true`. An absent or whitespace-only
/// surface answers `false` (there is nothing there at all). Managed-LOOKING entries that are
/// not in the caller's ledger are NOT this question — the drivers' `Foreign` state names them.
#[must_use]
pub fn holds_unmanaged_content(dialect: McpDialect, current: Option<&[u8]>) -> bool {
    if effectively_absent(current) {
        return false;
    }
    let bytes = current.unwrap_or_default();
    match dialect {
        // The plugin dir's `.mcp.json` is wholly topos-owned by construction: the shared JSON
        // walk answers the same question there (any sibling key or non-managed entry), and the
        // CALLER turns a `true` into a whole-surface back-off instead of mere ownership loss.
        McpDialect::ClaudeProjectJson
        | McpDialect::CursorJson
        | McpDialect::OpencodeJson
        | McpDialect::OpenclawJson
        | McpDialect::ClaudePluginDir
        | McpDialect::VscodeJson
        | McpDialect::CopilotCliJson
        | McpDialect::LmStudioJson
        | McpDialect::ClaudeDesktopJson
        | McpDialect::GeminiJson
        | McpDialect::WindsurfJson
        | McpDialect::ClineJson
        | McpDialect::RooJson
        | McpDialect::ZedJson => jsonc_edit::holds_unmanaged(dialect, bytes),
        McpDialect::CodexToml => toml_patch::holds_unmanaged(bytes),
        McpDialect::HermesYaml => yaml_splice::holds_unmanaged(bytes),
    }
}

// ---------------------------------------------------------------------------------------------
// The fingerprint — ONE canonical structural rendering shared by every dialect.
// ---------------------------------------------------------------------------------------------

/// The sha256 hex of `value`'s canonical structural rendering: object keys sorted at every
/// depth, serialized compact. Whitespace/key-order reflow of the source bytes never changes it;
/// any added/changed/removed field does.
#[must_use]
pub fn fingerprint_value(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            // BTreeMap-sort the keys so the rendering is independent of the source order AND of
            // serde_json's map backend (`preserve_order` may be unified in by another crate).
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (key, child)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(child, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // Scalars use serde_json's own compact rendering (string escaping, number formatting).
        leaf => out.push_str(&leaf.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// The per-dialect entry rendering — EXACT (a wrong key can brick a harness).
// ---------------------------------------------------------------------------------------------

/// The entry's value in `dialect`'s wire shape, as a [`Value`]. Keys are inserted in sorted
/// order so serialization is deterministic under either serde_json map backend. Empty `headers`
/// (and an empty `env`) are omitted in every dialect.
///
/// `None` means the dialect CANNOT EXPRESS that target — the pairs no vendor evidence covers: a
/// program-run server in Hermes's YAML and in LM Studio's `mcp.json` (both documented only in
/// their address form), and an ADDRESS in Claude Desktop's config, which reaches a remote server
/// through the bridge and through no key of its own. It is not an error and never a guess: the
/// caller withholds the placement and says so, and every driver refuses the whole edit rather than
/// writing a shape it invented. The answer is on the SHAPE alone, never the contents — which is
/// what lets a caller ask it once, of an empty target, before it has anything to place.
#[must_use]
pub fn entry_value(dialect: McpDialect, entry: &McpEntry) -> Option<Value> {
    let mut m = Map::new();
    match (dialect, &entry.target) {
        // `type` is MANDATORY here — Claude Code refuses a typeless remote entry, VS Code
        // documents the key as required, and Gemini's own `mcp add` writes `url` + `type: http`
        // (its `httpUrl` is the deprecated spelling of the same thing).
        (
            McpDialect::ClaudePluginDir
            | McpDialect::ClaudeProjectJson
            | McpDialect::VscodeJson
            | McpDialect::GeminiJson,
            McpTarget::Remote { url, headers },
        ) => {
            insert_pairs(&mut m, "headers", headers);
            m.insert("type".to_owned(), Value::String("http".to_owned()));
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // The SAME `type` key, spelled differently by each — and left out it silently means SSE in
        // both, which is a server that answers nothing while the config looks right. Stated
        // always, and stated in each one's own spelling.
        (McpDialect::ClineJson | McpDialect::RooJson, McpTarget::Remote { url, headers }) => {
            insert_pairs(&mut m, "headers", headers);
            let word = if dialect == McpDialect::ClineJson {
                "streamableHttp"
            } else {
                "streamable-http"
            };
            m.insert("type".to_owned(), Value::String(word.to_owned()));
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // NEVER a `type` key: a `type: "streamable-http"` makes cursor-agent silently drop the
        // ENTIRE file. LM Studio and Zed join it for the opposite reason — no documented entry of
        // theirs carries a `type` at all, so the key that is not there is the grounded one. A Zed
        // entry with no headers is also how its own OAuth sign-in gets to run, which is the trust
        // moment topos wants anyway.
        (
            McpDialect::CursorJson | McpDialect::LmStudioJson | McpDialect::ZedJson,
            McpTarget::Remote { url, headers },
        ) => {
            insert_pairs(&mut m, "headers", headers);
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // Windsurf spells the address itself differently — `serverUrl`, and no type beside it.
        (McpDialect::WindsurfJson, McpTarget::Remote { url, headers }) => {
            insert_pairs(&mut m, "headers", headers);
            m.insert("serverUrl".to_owned(), Value::String(url.clone()));
        }
        // Copilot CLI names the tools it will expose from a server on the entry itself. `["*"]` is
        // the documented default AND what its own examples write — the same allowance a person
        // adding this server by hand would get, stated rather than left to be inferred.
        (McpDialect::CopilotCliJson, McpTarget::Remote { url, headers }) => {
            insert_pairs(&mut m, "headers", headers);
            m.insert("tools".to_owned(), all_tools());
            m.insert("type".to_owned(), Value::String("http".to_owned()));
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        (McpDialect::OpencodeJson, McpTarget::Remote { url, headers }) => {
            m.insert("enabled".to_owned(), Value::Bool(true));
            insert_pairs(&mut m, "headers", headers);
            m.insert("type".to_owned(), Value::String("remote".to_owned()));
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // `transport` EXPLICIT (OpenClaw's silent default is sse), and `auth: oauth` on the
        // explicit hint alone — OpenClaw's per-server options are open-world, `auth` is one of
        // its own declared keys, and its whole OAuth path (including `openclaw mcp login`) is
        // gated on that key: without it an oauth-hinted server can never be signed in to.
        // Nothing else is emitted — a key OpenClaw retired is refused, and a refused config
        // bricks its gateway startup.
        (McpDialect::OpenclawJson, McpTarget::Remote { url, headers }) => {
            if entry.auth == AuthHint::Oauth {
                m.insert("auth".to_owned(), Value::String("oauth".to_owned()));
            }
            insert_pairs(&mut m, "headers", headers);
            m.insert(
                "transport".to_owned(),
                Value::String("streamable-http".to_owned()),
            );
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // ONLY `url` and (when headers exist) `http_headers` — a wrong key hard-fails Codex's
        // whole config load. This is the FINGERPRINT shape; the TOML emission mirrors it.
        (McpDialect::CodexToml, McpTarget::Remote { url, headers }) => {
            insert_pairs(&mut m, "http_headers", headers);
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // The parsed shape of the one-line flow mapping `yaml_splice` emits. `auth: oauth` only
        // on the explicit hint — Unknown/None omit it.
        (McpDialect::HermesYaml, McpTarget::Remote { url, headers }) => {
            if entry.auth == AuthHint::Oauth {
                m.insert("auth".to_owned(), Value::String("oauth".to_owned()));
            }
            insert_pairs(&mut m, "headers", headers);
            m.insert("url".to_owned(), Value::String(url.clone()));
        }
        // The stdio shape most of the JSON families share verbatim (`command`/`args`/`env`), and
        // Codex's TOML spells identically. `type: "stdio"` where the harness declares the key
        // (Claude Code, VS Code, OpenClaw's `transport`, Copilot CLI's own word for it); never for
        // Cursor, which drops a whole file over a `type` it dislikes and infers stdio from the
        // command anyway, and never for Claude Desktop, whose every documented entry is the bare
        // triple. `auth` belongs to an address, so a program-run entry never carries it — a
        // bridged remote signs itself in.
        (
            McpDialect::ClaudePluginDir
            | McpDialect::ClaudeProjectJson
            | McpDialect::CursorJson
            | McpDialect::OpenclawJson
            | McpDialect::CodexToml
            | McpDialect::VscodeJson
            | McpDialect::CopilotCliJson
            | McpDialect::ClaudeDesktopJson
            | McpDialect::GeminiJson
            | McpDialect::WindsurfJson
            | McpDialect::ClineJson
            | McpDialect::RooJson
            | McpDialect::ZedJson,
            McpTarget::Local {
                command,
                args,
                env,
                env_ref,
            },
        ) => {
            m.insert(
                "args".to_owned(),
                Value::Array(args.iter().map(|a| Value::String(a.clone())).collect()),
            );
            m.insert("command".to_owned(), Value::String(command.clone()));
            // CODEX IS THE ONE THAT DOES NOT SPELL INHERITANCE IN THE VALUE. Its `env` map is
            // literal text handed to the child as it stands, and the names it forwards from its
            // own environment are a separate `env_vars` list — so a `${VAR}` written into `env`
            // would reach the server as those six characters and nothing else.
            if dialect == McpDialect::CodexToml {
                insert_pairs(&mut m, "env", &literals(env));
                let inherited: Vec<Value> = inherited_names(env)
                    .into_iter()
                    .map(Value::String)
                    .collect();
                if !inherited.is_empty() {
                    m.insert("env_vars".to_owned(), Value::Array(inherited));
                }
            } else {
                insert_pairs(&mut m, "env", &rendered(env, *env_ref));
            }
            match dialect {
                McpDialect::ClaudePluginDir
                | McpDialect::ClaudeProjectJson
                | McpDialect::VscodeJson
                | McpDialect::ClineJson
                | McpDialect::RooJson => {
                    m.insert("type".to_owned(), Value::String("stdio".to_owned()));
                }
                McpDialect::OpenclawJson => {
                    m.insert("transport".to_owned(), Value::String("stdio".to_owned()));
                }
                // Copilot CLI's word for a program it runs is `local` — the spelling its own
                // documentation writes, and it carries the same tools allowance an address does.
                McpDialect::CopilotCliJson => {
                    m.insert("tools".to_owned(), all_tools());
                    m.insert("type".to_owned(), Value::String("local".to_owned()));
                }
                _ => {}
            }
        }
        // OpenCode is the outlier twice over: the program and its arguments are ONE `command`
        // array, and the environment is spelled `environment`.
        (
            McpDialect::OpencodeJson,
            McpTarget::Local {
                command,
                args,
                env,
                env_ref,
            },
        ) => {
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(Value::String(command.clone()));
            argv.extend(args.iter().map(|a| Value::String(a.clone())));
            m.insert("command".to_owned(), Value::Array(argv));
            m.insert("enabled".to_owned(), Value::Bool(true));
            insert_pairs(&mut m, "environment", &rendered(env, *env_ref));
            m.insert("type".to_owned(), Value::String("local".to_owned()));
        }
        // The pairs no vendor evidence covers. LM Studio documents an address and only an
        // address; Claude Desktop documents a program and only a program — its remote servers are
        // added in the app, through a flow no config file takes part in, so the bridge is how a
        // shared address reaches it and there is no URL key to invent beside that.
        (McpDialect::HermesYaml | McpDialect::LmStudioJson, McpTarget::Local { .. })
        | (McpDialect::ClaudeDesktopJson, McpTarget::Remote { .. }) => return None,
    }
    Some(Value::Object(m))
}

/// The tools a topos-placed entry exposes, for the dialects that name them on the entry: all of
/// them. It is the documented default, so stating it changes nothing about what the agent can do —
/// it only spells the answer where the file expects one.
fn all_tools() -> Value {
    Value::Array(vec![Value::String("*".to_owned())])
}

/// Whether `dialect` can express `target` at all — [`entry_value`]'s question, asked before a
/// placement is planned rather than after it fails. The answer is on the target's SHAPE, never its
/// contents, so a caller may ask it of an empty one.
#[must_use]
pub fn dialect_expresses(dialect: McpDialect, target: &McpTarget) -> bool {
    !matches!(
        (dialect, target),
        (
            McpDialect::HermesYaml | McpDialect::LmStudioJson,
            McpTarget::Local { .. }
        ) | (McpDialect::ClaudeDesktopJson, McpTarget::Remote { .. })
    )
}

/// Every slot as the value a config file carries: a literal as itself, an inherited one as this
/// harness's own reference spelling — the form every dialect but Codex's reads.
fn rendered(env: &[(String, EnvValue)], env_ref: descriptor::EnvRef) -> Vec<(String, String)> {
    env.iter()
        .map(|(name, value)| {
            let text = match value {
                EnvValue::Literal(literal) => literal.clone(),
                EnvValue::Inherited => env_ref.render(name),
            };
            (name.clone(), text)
        })
        .collect()
}

/// Only the slots that carry a stated value — Codex's `env` map, which is literal text.
fn literals(env: &[(String, EnvValue)]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|(name, value)| match value {
            EnvValue::Literal(literal) => Some((name.clone(), literal.clone())),
            EnvValue::Inherited => None,
        })
        .collect()
}

/// Only the NAMES to inherit, sorted, so the emitted list is deterministic — Codex's `env_vars`.
fn inherited_names(env: &[(String, EnvValue)]) -> Vec<String> {
    let mut names: Vec<String> = env
        .iter()
        .filter(|(_, value)| *value == EnvValue::Inherited)
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Insert `pairs` under `key` as a name-sorted JSON object, or nothing at all when there are none
/// (an empty map carries no information and strict validators are the risk surface). A duplicated
/// name is last-wins, matching what any JSON/TOML/YAML mapping would resolve to anyway.
fn insert_pairs(m: &mut Map<String, Value>, key: &str, pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    let sorted: BTreeMap<&str, &str> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut out = Map::new();
    for (k, v) in sorted {
        out.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    m.insert(key.to_owned(), Value::Object(out));
}

// ---------------------------------------------------------------------------------------------
// Shared driver internals: desired-set validation, the one reconcile classifier, outcome
// helpers. pub(crate) — the drivers are the only consumers.
// ---------------------------------------------------------------------------------------------

/// The managed-key prefix — what makes an entry managed-LOOKING to every later run.
pub(crate) const MANAGED_KEY_PREFIX: &str = "topos-";

/// Re-verify the caller-guaranteed key contract (prefix + charset + uniqueness) — the prefix IS
/// the ownership recognizer, so a violating key would write an entry no later run could see.
/// Fail-closed with an honest reason instead.
pub(crate) fn validate_desired(desired: &[McpEntry]) -> Result<(), String> {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for entry in desired {
        let key = entry.key.as_str();
        let charset_ok = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !charset_ok || !key.starts_with(MANAGED_KEY_PREFIX) {
            return Err(format!(
                "desired key {key:?} violates the managed-key contract (charset [a-z0-9-], `topos-` prefix)"
            ));
        }
        if !seen.insert(key) {
            return Err(format!("desired key {key:?} appears twice"));
        }
    }
    Ok(())
}

/// The dialect fingerprint of every desired entry, in order — or the ONE honest reason a driver
/// refuses the whole edit: a target this dialect has no grammar for. Computing them together is
/// what makes "cannot express" a precondition of the edit rather than something discovered
/// halfway through it.
pub(crate) fn desired_fingerprints(
    dialect: McpDialect,
    desired: &[McpEntry],
) -> Result<Vec<String>, String> {
    desired
        .iter()
        .map(|e| {
            entry_value(dialect, e)
                .map(|v| fingerprint_value(&v))
                .ok_or_else(|| {
                    "this agent's MCP config cannot describe a server topos runs on this machine"
                        .to_owned()
                })
        })
        .collect()
}

/// One managed-looking entry as found in the file.
#[derive(Debug, Clone)]
pub(crate) enum FoundEntry {
    /// Parsed to a value — its structural fingerprint.
    Value(String),
    /// Sentinel-marked as ours but unparsable (a hand-mangled Hermes line) — forced `Drifted`,
    /// untouched, never removed.
    OpaqueDrifted,
    /// Occupies the key without any provable topos provenance — forced `Foreign`, untouched.
    OpaqueForeign,
}

/// What the classifier decided: per-key states + the ledger + the three mutation sets the
/// driver executes mechanically.
#[derive(Debug, Default)]
pub(crate) struct Reconcile {
    pub(crate) states: Vec<(String, EntryState)>,
    pub(crate) fingerprints: Vec<(String, String)>,
    /// Indices into `desired` to insert (PlacedNew).
    pub(crate) inserts: Vec<usize>,
    /// Indices into `desired` to replace in place (Updated).
    pub(crate) updates: Vec<usize>,
    /// Keys to delete (Removed).
    pub(crate) removes: Vec<String>,
}

impl Reconcile {
    pub(crate) fn is_noop(&self) -> bool {
        self.inserts.is_empty() && self.updates.is_empty() && self.removes.is_empty()
    }
}

/// The ONE drift/ownership classifier every driver runs (the module-doc rules, verbatim).
/// `desired_fps[i]` is the dialect fingerprint of `desired[i]`; `found` is every managed-looking
/// entry in the file; `prior` is the caller's ledger.
pub(crate) fn reconcile(
    desired: &[McpEntry],
    desired_fps: &[String],
    found: &BTreeMap<String, FoundEntry>,
    prior: &BTreeMap<String, String>,
) -> Reconcile {
    let mut out = Reconcile::default();
    for (i, entry) in desired.iter().enumerate() {
        let key = entry.key.clone();
        let want = &desired_fps[i];
        match found.get(&key) {
            None => {
                out.inserts.push(i);
                out.states.push((key.clone(), EntryState::PlacedNew));
                out.fingerprints.push((key, want.clone()));
            }
            Some(FoundEntry::Value(current)) if current == want => {
                out.states.push((key.clone(), EntryState::Current));
                out.fingerprints.push((key, want.clone()));
            }
            Some(FoundEntry::Value(current)) if prior.get(&key) == Some(current) => {
                out.updates.push(i);
                out.states.push((key.clone(), EntryState::Updated));
                out.fingerprints.push((key, want.clone()));
            }
            Some(FoundEntry::Value(_) | FoundEntry::OpaqueDrifted) if prior.contains_key(&key) => {
                // User-edited since topos wrote it: untouched, and the ledger keeps the OLD
                // fingerprint so the drift classification survives re-runs.
                let old = prior[&key].clone();
                out.states.push((key.clone(), EntryState::Drifted));
                out.fingerprints.push((key, old));
            }
            Some(FoundEntry::OpaqueDrifted) => {
                // Sentinel-marked but mangled AND no ledger record: still ours-looking enough
                // that inserting a duplicate would be worse — Drifted, untouched, no ledger row.
                out.states.push((key, EntryState::Drifted));
            }
            Some(_) => {
                out.states.push((key, EntryState::Foreign));
            }
        }
    }
    let desired_keys: std::collections::BTreeSet<&str> =
        desired.iter().map(|e| e.key.as_str()).collect();
    for (key, entry) in found {
        if desired_keys.contains(key.as_str()) {
            continue;
        }
        match entry {
            FoundEntry::Value(current) if prior.get(key) == Some(current) => {
                out.removes.push(key.clone());
                out.states.push((key.clone(), EntryState::Removed));
            }
            FoundEntry::Value(_) | FoundEntry::OpaqueDrifted if prior.contains_key(key) => {
                let old = prior[key].clone();
                out.states.push((key.clone(), EntryState::Drifted));
                out.fingerprints.push((key.clone(), old));
            }
            FoundEntry::OpaqueDrifted => {
                out.states.push((key.clone(), EntryState::Drifted));
            }
            _ => {
                out.states.push((key.clone(), EntryState::Foreign));
            }
        }
    }
    out
}

/// An `Unprovable` outcome: zero byte changes, empty states/ledger (the caller keeps its old
/// ledger).
pub(crate) fn unprovable(reason: impl Into<String>) -> ApplyOutcome {
    ApplyOutcome {
        plan: EditPlan::Unprovable(reason.into()),
        states: Vec::new(),
        fingerprints: Vec::new(),
        created_file: false,
    }
}

/// A `Leave`/`Write` outcome from a finished reconcile.
pub(crate) fn outcome(plan: EditPlan, reconcile: Reconcile, created_file: bool) -> ApplyOutcome {
    ApplyOutcome {
        plan,
        states: reconcile.states,
        fingerprints: reconcile.fingerprints,
        created_file,
    }
}

/// Whether bytes are absent-or-whitespace — the fresh-creation base every driver shares (a
/// whitespace-only file carries no user content; its replacement is a whole-file creation).
pub(crate) fn effectively_absent(current: Option<&[u8]>) -> bool {
    match current {
        None => true,
        Some(bytes) => bytes.iter().all(u8::is_ascii_whitespace),
    }
}

/// Shared fixture constructors for the driver test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use super::{AuthHint, EnvValue, McpEntry, McpTarget, descriptor};

    pub(crate) fn entry(key: &str, url: &str) -> McpEntry {
        McpEntry {
            key: key.to_owned(),
            target: McpTarget::Remote {
                url: url.to_owned(),
                headers: Vec::new(),
            },
            auth: AuthHint::Unknown,
        }
    }

    pub(crate) fn entry_with_headers(key: &str, url: &str, headers: &[(&str, &str)]) -> McpEntry {
        McpEntry {
            target: McpTarget::Remote {
                url: url.to_owned(),
                headers: pairs(headers),
            },
            ..entry(key, url)
        }
    }

    /// A program-run entry whose environment slots all carry stated values.
    pub(crate) fn local_entry(
        key: &str,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> McpEntry {
        let env: Vec<(String, EnvValue)> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), EnvValue::Literal((*v).to_owned())))
            .collect();
        local_entry_with(key, command, args, env, descriptor::EnvRef::DollarBrace)
    }

    /// The same, for the two facts a literal-only entry never exercises: a slot INHERITED from
    /// the machine, and the spelling this harness reads one in.
    pub(crate) fn local_entry_with(
        key: &str,
        command: &str,
        args: &[&str],
        env: Vec<(String, EnvValue)>,
        env_ref: descriptor::EnvRef,
    ) -> McpEntry {
        McpEntry {
            key: key.to_owned(),
            target: McpTarget::Local {
                command: command.to_owned(),
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                env,
                env_ref,
            },
            auth: AuthHint::Unknown,
        }
    }

    fn pairs(from: &[(&str, &str)]) -> Vec<(String, String)> {
        from.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{entry, entry_with_headers};
    use super::*;

    #[test]
    fn fingerprint_is_reflow_stable_and_value_sensitive() {
        // Key order and whitespace never matter…
        let a: Value = serde_json::from_str(r#"{"url":"https://x","type":"http"}"#).unwrap();
        let b: Value =
            serde_json::from_str("{ \"type\" : \"http\" ,\n \"url\" : \"https://x\" }").unwrap();
        assert_eq!(fingerprint_value(&a), fingerprint_value(&b));
        // …any field change does.
        let c: Value = serde_json::from_str(r#"{"url":"https://y","type":"http"}"#).unwrap();
        let d: Value =
            serde_json::from_str(r#"{"url":"https://x","type":"http","extra":1}"#).unwrap();
        assert_ne!(fingerprint_value(&a), fingerprint_value(&c));
        assert_ne!(fingerprint_value(&a), fingerprint_value(&d));
        // Nested objects sort too.
        let e: Value = serde_json::from_str(r#"{"headers":{"b":"2","a":"1"},"url":"u"}"#).unwrap();
        let f: Value = serde_json::from_str(r#"{"url":"u","headers":{"a":"1","b":"2"}}"#).unwrap();
        assert_eq!(fingerprint_value(&e), fingerprint_value(&f));
    }

    /// One entry's rendering in a dialect that HAS one — the test spelling of [`entry_value`],
    /// which answers `None` only for the pair `hermes_refuses_a_program_it_has_no_grammar_for`
    /// covers.
    fn rendered(dialect: McpDialect, e: &McpEntry) -> Value {
        entry_value(dialect, e).unwrap_or_else(|| panic!("{dialect:?} renders this target"))
    }

    #[test]
    fn entry_values_carry_exactly_the_dialect_keys() {
        let plain = entry("topos-x", "https://mcp.example/x");
        let keys = |d: McpDialect, e: &McpEntry| -> Vec<String> {
            rendered(d, e)
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(
            keys(McpDialect::ClaudeProjectJson, &plain),
            ["type", "url"],
            "claude: type is mandatory"
        );
        assert_eq!(keys(McpDialect::ClaudePluginDir, &plain), ["type", "url"]);
        assert_eq!(
            keys(McpDialect::CursorJson, &plain),
            ["url"],
            "cursor: NEVER a type key"
        );
        assert_eq!(
            keys(McpDialect::OpencodeJson, &plain),
            ["enabled", "type", "url"]
        );
        assert_eq!(
            keys(McpDialect::OpenclawJson, &plain),
            ["transport", "url"],
            "openclaw: transport explicit; `auth` rides the explicit hint alone"
        );
        assert_eq!(keys(McpDialect::CodexToml, &plain), ["url"]);
        assert_eq!(
            keys(McpDialect::HermesYaml, &plain),
            ["url"],
            "hermes: Unknown auth omits the opt-in"
        );
        assert_eq!(
            rendered(McpDialect::OpenclawJson, &plain)["transport"],
            "streamable-http"
        );
        assert_eq!(rendered(McpDialect::OpencodeJson, &plain)["type"], "remote");
        assert_eq!(rendered(McpDialect::OpencodeJson, &plain)["enabled"], true);

        // Headers ride every dialect (codex spells them http_headers), name-sorted.
        let with = entry_with_headers("topos-x", "https://u", &[("Z", "9"), ("A", "1")]);
        for d in [
            McpDialect::ClaudePluginDir,
            McpDialect::ClaudeProjectJson,
            McpDialect::CursorJson,
            McpDialect::OpencodeJson,
            McpDialect::OpenclawJson,
            McpDialect::HermesYaml,
        ] {
            let v = rendered(d, &with);
            let h = v["headers"].as_object().unwrap();
            assert_eq!(h.keys().collect::<Vec<_>>(), ["A", "Z"], "{d:?}");
        }
        let codex = rendered(McpDialect::CodexToml, &with);
        assert!(codex.get("headers").is_none());
        assert_eq!(codex["http_headers"]["A"], "1");
    }

    /// The two dialects whose OAuth path is gated on the key emit it on the explicit hint and
    /// nowhere else; every other dialect never carries it (OAuth there is the harness's own
    /// on-401 behavior).
    #[test]
    fn auth_is_emitted_only_on_the_explicit_oauth_hint_and_only_where_it_is_read() {
        for (hint, expect_auth) in [
            (AuthHint::Oauth, true),
            (AuthHint::None, false),
            (AuthHint::Unknown, false),
        ] {
            let e = McpEntry {
                auth: hint,
                ..entry("topos-x", "https://u")
            };
            for dialect in [McpDialect::HermesYaml, McpDialect::OpenclawJson] {
                let v = rendered(dialect, &e);
                assert_eq!(v.get("auth").is_some(), expect_auth, "{dialect:?} {hint:?}");
                if expect_auth {
                    assert_eq!(v["auth"], "oauth", "{dialect:?}");
                }
            }
            for dialect in [
                McpDialect::ClaudePluginDir,
                McpDialect::ClaudeProjectJson,
                McpDialect::CursorJson,
                McpDialect::OpencodeJson,
                McpDialect::CodexToml,
            ] {
                assert!(
                    rendered(dialect, &e).get("auth").is_none(),
                    "{dialect:?}: no auth key in this dialect"
                );
            }
        }
        // OpenClaw's oauth entry carries EXACTLY its four legal keys — `transport` stays
        // explicit and nothing OpenClaw retired rides along.
        let oauth = McpEntry {
            auth: AuthHint::Oauth,
            ..entry_with_headers("topos-x", "https://u", &[("A", "1")])
        };
        let v = rendered(McpDialect::OpenclawJson, &oauth);
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["auth", "headers", "transport", "url"]
        );
        // A PROGRAM is not an address: nothing carries `auth` for one, in any dialect. A bridged
        // remote signs itself in, and a package server has nobody to sign in to.
        let program = McpEntry {
            auth: AuthHint::Oauth,
            ..testutil::local_entry("topos-x", "npx", &["-y", "pkg@1.0.0"], &[])
        };
        for dialect in [
            McpDialect::ClaudePluginDir,
            McpDialect::ClaudeProjectJson,
            McpDialect::CursorJson,
            McpDialect::OpencodeJson,
            McpDialect::OpenclawJson,
            McpDialect::CodexToml,
        ] {
            assert!(
                rendered(dialect, &program).get("auth").is_none(),
                "{dialect:?}: a program never carries auth"
            );
        }
    }

    /// **An inherited slot is a NAME, and each harness is told about it in the form that harness
    /// actually resolves.** Most spell the reference inside the value; Codex's `env` map is
    /// literal text handed to the child as it stands, and the variables it forwards from its own
    /// environment are a separate `env_vars` list — so a `${VAR}` written into its `env` would
    /// reach the server as those six characters. (Grounded in Codex's source, cited in the PR.)
    #[test]
    fn an_inherited_slot_reaches_each_harness_in_the_form_that_harness_resolves() {
        let entry = |env_ref| {
            testutil::local_entry_with(
                "topos-x",
                "npx",
                &["-y", "@acme/server@1.2.3"],
                vec![
                    ("ACME_REGION".to_owned(), EnvValue::Literal("eu".to_owned())),
                    ("ACME_TOKEN".to_owned(), EnvValue::Inherited),
                ],
                env_ref,
            )
        };

        // Codex: the literal in `env`, the inherited NAME in `env_vars`.
        let codex = rendered(
            McpDialect::CodexToml,
            &entry(descriptor::EnvRef::DollarBrace),
        );
        assert_eq!(
            codex.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["args", "command", "env", "env_vars"]
        );
        assert_eq!(codex["env"], serde_json::json!({"ACME_REGION": "eu"}));
        assert_eq!(codex["env_vars"], serde_json::json!(["ACME_TOKEN"]));

        // Everywhere else: the harness's own reference spelling, inside the value.
        let claude = rendered(
            McpDialect::ClaudeProjectJson,
            &entry(descriptor::EnvRef::DollarBrace),
        );
        assert_eq!(claude["env"]["ACME_TOKEN"], "${ACME_TOKEN}");
        assert!(claude.get("env_vars").is_none());
        let opencode = rendered(
            McpDialect::OpencodeJson,
            &entry(descriptor::EnvRef::BraceEnv),
        );
        assert_eq!(opencode["environment"]["ACME_TOKEN"], "{env:ACME_TOKEN}");
        assert_eq!(opencode["environment"]["ACME_REGION"], "eu");

        // A Codex entry with nothing to inherit carries no list at all — an empty one is as
        // uninformative as an empty map, and strict validators are the risk surface.
        let literal_only = testutil::local_entry("topos-x", "npx", &[], &[("ACME_REGION", "eu")]);
        assert!(
            rendered(McpDialect::CodexToml, &literal_only)
                .get("env_vars")
                .is_none()
        );
        // …and one with nothing BUT inherited slots carries the list and no map.
        let inherited_only = testutil::local_entry_with(
            "topos-x",
            "npx",
            &[],
            vec![
                ("B_TOKEN".to_owned(), EnvValue::Inherited),
                ("A_TOKEN".to_owned(), EnvValue::Inherited),
            ],
            descriptor::EnvRef::DollarBrace,
        );
        let codex = rendered(McpDialect::CodexToml, &inherited_only);
        assert!(codex.get("env").is_none());
        assert_eq!(
            codex["env_vars"],
            serde_json::json!(["A_TOKEN", "B_TOKEN"]),
            "sorted, so the bytes are deterministic"
        );
    }

    /// The stdio shapes, per dialect, from the vendor evidence recorded in the module docs: the
    /// `command`/`args`/`env` triple almost everywhere, OpenCode's single `command` array with
    /// `environment` beside it, `type`/`transport` exactly where the harness declares the key.
    #[test]
    fn a_program_renders_in_each_dialects_own_stdio_shape() {
        let program = testutil::local_entry(
            "topos-x",
            "npx",
            &["-y", "@acme/server@1.2.3"],
            &[("ACME_REGION", "eu"), ("ACME_TOKEN", "${ACME_TOKEN}")],
        );
        let v = rendered(McpDialect::ClaudeProjectJson, &program);
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["args", "command", "env", "type"]
        );
        assert_eq!(v["command"], "npx");
        assert_eq!(v["args"], serde_json::json!(["-y", "@acme/server@1.2.3"]));
        assert_eq!(v["env"]["ACME_TOKEN"], "${ACME_TOKEN}");
        assert_eq!(v["type"], "stdio");
        assert_eq!(
            rendered(McpDialect::ClaudePluginDir, &program),
            v,
            "the plugin dir's .mcp.json is the same Claude shape"
        );

        let cursor = rendered(McpDialect::CursorJson, &program);
        assert_eq!(
            cursor.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["args", "command", "env"],
            "cursor: NEVER a type key, on either target"
        );

        let claw = rendered(McpDialect::OpenclawJson, &program);
        assert_eq!(
            claw.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["args", "command", "env", "transport"]
        );
        assert_eq!(claw["transport"], "stdio");

        let codex = rendered(McpDialect::CodexToml, &program);
        assert_eq!(
            codex.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["args", "command", "env"]
        );

        // OpenCode: the program and its arguments are ONE array, and the environment is
        // `environment`.
        let oc = rendered(McpDialect::OpencodeJson, &program);
        assert_eq!(
            oc.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["command", "enabled", "environment", "type"]
        );
        assert_eq!(
            oc["command"],
            serde_json::json!(["npx", "-y", "@acme/server@1.2.3"])
        );
        assert_eq!(oc["environment"]["ACME_REGION"], "eu");
        assert_eq!(oc["type"], "local");
        assert!(oc.get("args").is_none());

        // An empty environment is omitted everywhere, exactly as empty headers are.
        let bare = testutil::local_entry("topos-x", "uvx", &["pkg==1.0.0"], &[]);
        for dialect in [
            McpDialect::ClaudePluginDir,
            McpDialect::ClaudeProjectJson,
            McpDialect::CursorJson,
            McpDialect::OpenclawJson,
            McpDialect::CodexToml,
        ] {
            assert!(rendered(dialect, &bare).get("env").is_none(), "{dialect:?}");
        }
        assert!(
            rendered(McpDialect::OpencodeJson, &bare)
                .get("environment")
                .is_none()
        );
    }

    /// Hermes's one-line YAML grammar is evidenced for an ADDRESS and nothing else, so a
    /// program-run server has no spelling there — and the driver refuses the whole edit instead of
    /// inventing one.
    #[test]
    fn hermes_refuses_a_program_it_has_no_grammar_for() {
        let program = testutil::local_entry("topos-x", "npx", &["-y", "pkg@1.0.0"], &[]);
        assert_eq!(entry_value(McpDialect::HermesYaml, &program), None);
        assert!(!dialect_expresses(McpDialect::HermesYaml, &program.target));
        let out = apply(McpDialect::HermesYaml, None, &[program], &BTreeMap::new());
        let EditPlan::Unprovable(reason) = &out.plan else {
            panic!("a program in hermes must refuse: {:?}", out.plan);
        };
        assert!(
            reason.contains("cannot describe a server topos runs"),
            "{reason}"
        );
        assert!(out.states.is_empty() && out.fingerprints.is_empty());
        // Every other dialect expresses both targets.
        for dialect in [
            McpDialect::ClaudePluginDir,
            McpDialect::ClaudeProjectJson,
            McpDialect::CursorJson,
            McpDialect::OpencodeJson,
            McpDialect::OpenclawJson,
            McpDialect::CodexToml,
        ] {
            assert!(
                dialect_expresses(
                    dialect,
                    &McpTarget::Local {
                        command: "npx".to_owned(),
                        args: Vec::new(),
                        env: Vec::new(),
                        env_ref: descriptor::EnvRef::DollarBrace,
                    }
                ),
                "{dialect:?}"
            );
        }
    }

    /// A program-run server has ONE address too — the command line after any Windows wrapper is
    /// stripped, and the URL itself when the program is only a bridge to one.
    #[test]
    fn a_program_run_server_has_one_address_and_a_bridge_answers_with_its_url() {
        let argv =
            |args: &[&str]| -> Vec<String> { args.iter().map(|a| (*a).to_owned()).collect() };
        // The same invocation, wrapped and unwrapped, is one server.
        let plain = local_address("npx", &argv(&["-y", "pkg@1.0.0"]));
        assert_eq!(
            plain,
            local_address("cmd", &argv(&["/c", "npx", "-y", "pkg@1.0.0"]))
        );
        assert_eq!(
            plain,
            local_address("CMD.EXE", &argv(&["/C", "npx", "-y", "pkg@1.0.0"]))
        );
        assert_eq!(plain.as_deref(), Some(r#"run:["npx","-y","pkg@1.0.0"]"#));
        // Different arguments are a different server; an empty command is no address at all.
        assert_ne!(plain, local_address("npx", &argv(&["-y", "pkg@1.0.1"])));
        assert_eq!(local_address("", &[]), None);
        assert_eq!(local_address("   ", &[]), None);

        // A BRIDGE answers with the address it bridges — whatever version it pins, and through
        // the wrapper too, so an entry that bridges a server and an entry that dials it are one.
        let dialed = canonical_address("https://mcp.example/x");
        assert_eq!(
            local_address(
                "npx",
                &argv(&["-y", "mcp-remote@0.1.38", "https://mcp.example/x"])
            ),
            dialed
        );
        assert_eq!(
            local_address("npx", &argv(&["mcp-remote", "HTTPS://MCP.Example/x"])),
            dialed
        );
        assert_eq!(
            local_address(
                "cmd",
                &argv(&[
                    "/c",
                    "npx",
                    "-y",
                    "mcp-remote@0.1.38",
                    "https://mcp.example/x"
                ])
            ),
            dialed
        );
        // …and a bridge with no URL after it is just a command line.
        assert!(
            local_address("npx", &argv(&["-y", "mcp-remote"]))
                .is_some_and(|a| a.starts_with("run:"))
        );
        // A package that merely LOOKS like the bridge is not one.
        assert!(
            local_address("npx", &argv(&["-y", "mcp-remote-proxy", "https://x/y"]))
                .is_some_and(|a| a.starts_with("run:"))
        );
        // The entry model answers the same question for either target.
        let bridged = testutil::local_entry(
            "topos-x",
            "npx",
            &["-y", "mcp-remote@0.1.38", "https://mcp.example/x"],
            &[],
        );
        assert_eq!(bridged.address(), dialed);
        assert_eq!(bridged.url(), None);
        assert_eq!(entry("topos-x", "https://mcp.example/x").address(), dialed);
    }

    /// The Windows wrapper: applied only on Windows, only to the shim commands, never twice, and
    /// never inside WSL — where the harness reading the file runs Linux.
    #[test]
    fn the_windows_wrapper_is_applied_exactly_where_it_belongs() {
        let args = vec!["-y".to_owned(), "a b".to_owned()];
        // Not Windows: nothing is wrapped.
        assert_eq!(
            windows_wrap("npx", &args, false, false),
            ("npx".to_owned(), args.clone())
        );
        // Windows: the shim commands are wrapped, argument VALUES untouched (a value with a
        // space stays ONE argument — the list is the quoting).
        let (command, wrapped) = windows_wrap("npx", &args, true, false);
        assert_eq!(command, "cmd");
        assert_eq!(wrapped, ["/c", "npx", "-y", "a b"]);
        for shim in WINDOWS_WRAPPED_COMMANDS {
            assert!(needs_windows_wrap(shim), "{shim}");
        }
        assert!(needs_windows_wrap("npx.cmd"));
        assert!(needs_windows_wrap(r"C:\Program Files\nodejs\npx.cmd"));
        assert!(needs_windows_wrap("/usr/local/bin/npx"));
        // Never twice, and never something that is not a shim.
        assert!(!needs_windows_wrap("cmd"));
        assert!(!needs_windows_wrap("cmd.exe"));
        assert!(!needs_windows_wrap("uvx"));
        assert!(!needs_windows_wrap("python3"));
        let (command, unwrapped) = windows_wrap("uvx", &args, true, false);
        assert_eq!((command.as_str(), unwrapped.as_slice()), ("uvx", &args[..]));
        // WSL: the config belongs to a Linux harness, so the wrapper would be nonsense.
        assert_eq!(
            windows_wrap("npx", &args, true, true),
            ("npx".to_owned(), args.clone())
        );
        for wsl in [
            r"\\wsl$\Ubuntu\home\me\.cursor\mcp.json",
            r"\\WSL.LOCALHOST\Debian\home\me\.codex\config.toml",
            r"//wsl$/Ubuntu/home/me/.mcp.json",
        ] {
            assert!(is_wsl_unc(wsl), "{wsl}");
        }
        for local in [
            r"C:\Users\me\.cursor\mcp.json",
            r"\\server\share\mcp.json",
            "/home/me/.cursor/mcp.json",
        ] {
            assert!(!is_wsl_unc(local), "{local}");
        }
    }

    // ------------------------------------------------------------------------------------------
    // The `cmd /c` chain, simulated end to end.
    // ------------------------------------------------------------------------------------------

    /// Stage 1: what a spawn API writes as ONE command line for `cmd.exe`, by the C runtime's
    /// rules — quote an element that is empty or holds a space, a tab or a quote; write an inner
    /// quote as `\"`, doubling the backslashes that precede it. (libuv's `quote_cmd_arg`, which is
    /// what every Node harness spawns through.)
    fn c_runtime_encode(args: &[&str]) -> String {
        let mut line = String::new();
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            if !(arg.is_empty() || arg.contains([' ', '\t', '"'])) {
                line.push_str(arg);
                continue;
            }
            line.push('"');
            let mut backslashes = 0usize;
            for c in arg.chars() {
                match c {
                    '\\' => backslashes += 1,
                    '"' => {
                        line.extend(std::iter::repeat_n('\\', backslashes + 1));
                        backslashes = 0;
                        line.push('"');
                    }
                    _ => {
                        backslashes = 0;
                        line.push(c);
                    }
                }
            }
            line.extend(std::iter::repeat_n('\\', backslashes));
            line.push('"');
        }
        line
    }

    /// Stage 2: what `cmd.exe` hands on. It acts on its metacharacters outside quotes — here the
    /// test only needs to know WHETHER it would (a split means the tail was torn apart) — and
    /// consumes a caret to take the next character literally. Returns `None` when the line would
    /// be split or redirected, which is the bug this whole mechanism exists to prevent.
    fn cmd_parse(line: &str) -> Option<String> {
        let mut out = String::with_capacity(line.len());
        let mut inside = false;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    inside = !inside;
                    out.push(c);
                }
                '^' if !inside => match chars.next() {
                    Some(next) => out.push(next),
                    None => return None,
                },
                '&' | '|' | '<' | '>' | '(' | ')' if !inside => return None,
                _ => out.push(c),
            }
        }
        Some(out)
    }

    /// Stage 3: the real program's own argv, parsed off that line by the C runtime's rules.
    fn c_runtime_parse(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut inside = false;
        let mut backslashes = 0usize;
        let flush = |current: &mut String, started: &mut bool, out: &mut Vec<String>| {
            if *started {
                out.push(std::mem::take(current));
                *started = false;
            }
        };
        for c in line.chars() {
            match c {
                '\\' => {
                    backslashes += 1;
                    started = true;
                }
                '"' => {
                    current.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        current.push('"');
                    } else {
                        inside = !inside;
                    }
                    backslashes = 0;
                    started = true;
                }
                ' ' | '\t' if !inside => {
                    current.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    flush(&mut current, &mut started, &mut out);
                }
                _ => {
                    current.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    current.push(c);
                    started = true;
                }
            }
        }
        current.extend(std::iter::repeat_n('\\', backslashes));
        flush(&mut current, &mut started, &mut out);
        out
    }

    /// **The whole chain**: what topos writes → what the harness's spawn API encodes → what
    /// `cmd.exe` leaves → what the program parses. The program must get back exactly the argv the
    /// bundle stated, metacharacters and all.
    #[test]
    fn a_wrapped_tail_reaches_the_program_as_the_bundle_stated_it() {
        let survives = |argv: &[&str]| {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_owned()).collect();
            let (command, wrapped) = windows_wrap("npx", &args, true, false);
            assert_eq!(command, "cmd");
            let line: Vec<&str> = std::iter::once(command.as_str())
                .chain(wrapped.iter().map(String::as_str))
                .collect();
            let after_cmd = cmd_parse(&c_runtime_encode(&line))
                .unwrap_or_else(|| panic!("cmd tore the line apart: {argv:?}"));
            // Everything after `cmd /c npx` is what the program itself receives.
            let received = c_runtime_parse(&after_cmd);
            assert_eq!(&received[..3], ["cmd", "/c", "npx"], "{argv:?}");
            assert_eq!(&received[3..], argv, "{argv:?}");
        };

        // The case that made this necessary: a bridged URL whose query joins two parameters.
        survives(&[
            "-y",
            "mcp-remote@0.1.38",
            "https://mcp.example/x?team=eng&mode=read",
            "--header",
            "X-Team:${TOPOS_HEADER_X_TEAM}",
        ]);
        // Spaces, quotes, and both at once — the shapes the encoding stage treats differently.
        survives(&[
            "--note",
            "a b",
            "--json",
            r#"{"k": "v"}"#,
            "--say",
            "he said \"hi\"",
        ]);
        // …and a metacharacter that a quote inside the value pushes back out of the quoting.
        survives(&["--say", r#"say "a&b" now"#, "--also", r#"a"b&c"#]);
        // Every metacharacter, alone and in company.
        survives(&["a&b", "a|b", "a<b", "a>b", "a^b", "a(b)c", "a&b|c>d"]);
        survives(&["--root", "/srv/notes", "run"]);

        // …and the escaping is the ONLY thing that changed: strip it and the same tail is torn.
        let bare = [
            "cmd",
            "/c",
            "npx",
            "https://mcp.example/x?team=eng&mode=read",
        ];
        assert_eq!(
            cmd_parse(&c_runtime_encode(&bare)),
            None,
            "the unescaped tail is exactly what cmd splits"
        );
    }

    /// The escape is a RENDERING, not a state: rendering the same argv twice writes the same
    /// bytes (so an entry never rewrites itself), and stripping the wrapper undoes it (so a
    /// bridged entry names one server whether or not Windows wrapped it).
    #[test]
    fn the_wrapper_is_idempotent_and_its_escaping_reverses() {
        let argv =
            |args: &[&str]| -> Vec<String> { args.iter().map(|a| (*a).to_owned()).collect() };
        let args = argv(&[
            "-y",
            "mcp-remote@0.1.38",
            "https://mcp.example/x?team=eng&mode=read",
        ]);
        let (command, once) = windows_wrap("npx", &args, true, false);
        let (again_command, again) = windows_wrap("npx", &args, true, false);
        assert_eq!((&command, &once), (&again_command, &again));
        assert_eq!(
            once,
            [
                "/c",
                "npx",
                "-y",
                "mcp-remote@0.1.38",
                "https://mcp.example/x?team=eng^&mode=read"
            ]
        );
        // The wrapper is never applied to its own output, so nothing can escape twice.
        assert!(!needs_windows_wrap(&command));

        // One server, wrapped or not.
        assert_eq!(
            local_address(&command, &once),
            local_address("npx", &args),
            "the escaped bridge and the bare one name one address"
        );
        assert_eq!(
            local_address(&command, &once).as_deref(),
            Some("https://mcp.example/x?team=eng&mode=read")
        );
        // The same for a program that is not a bridge.
        let plain = argv(&["--url", "https://x/y?a=1&b=2"]);
        let (wrapped_command, wrapped) = windows_wrap("npx", &plain, true, false);
        assert_eq!(
            local_address(&wrapped_command, &wrapped),
            local_address("npx", &plain)
        );

        // The escape and its inverse, character by character.
        for (raw, escaped) in [
            ("plain", "plain"),
            ("a&b", "a^&b"),
            ("a|b^c", "a^|b^^c"),
            ("a<b>c", "a^<b^>c"),
            ("(x)", "^(x^)"),
            // A value the spawner will quote is already protected — a caret there would reach the
            // program as a caret.
            ("a & b", "a & b"),
            // …and a quote INSIDE it closes that protection, because the spawner writes an inner
            // quote as `\"` and cmd counts every quote it sees.
            (r#"say "a&b" now"#, r#"say "a^&b" now"#),
            (r#"a"b&c"#, r#"a"b^&c"#),
        ] {
            assert_eq!(cmd_escape(raw), escaped, "{raw}");
            assert_eq!(cmd_unescape(escaped), raw, "{escaped}");
        }
        // A caret in front of anything else is somebody's literal caret, and stays one.
        assert_eq!(cmd_unescape("a^b"), "a^b");
        assert_eq!(cmd_unescape("trailing^"), "trailing^");
    }

    #[test]
    fn one_server_has_one_canonical_address() {
        let same = |a: &str, b: &str| {
            let (ca, cb) = (canonical_address(a), canonical_address(b));
            assert!(ca.is_some(), "{a}");
            assert_eq!(ca, cb, "{a} vs {b}");
        };
        // Scheme and host are case-insensitive; the path is not.
        same("HTTPS://MCP.Example.COM/mcp", "https://mcp.example.com/mcp");
        // A default port is no port at all — and a non-default one is part of the address.
        same(
            "https://mcp.example.com:443/mcp",
            "https://mcp.example.com/mcp",
        );
        same(
            "http://mcp.example.com:80/mcp",
            "http://mcp.example.com/mcp",
        );
        assert_ne!(
            canonical_address("https://mcp.example.com:8443/mcp"),
            canonical_address("https://mcp.example.com/mcp")
        );
        // A bare root is no path; a deeper trailing slash is a different path.
        same("https://mcp.example.com/", "https://mcp.example.com");
        assert_ne!(
            canonical_address("https://mcp.example.com/mcp/"),
            canonical_address("https://mcp.example.com/mcp")
        );
        // http and https are two addresses, and the query is part of the server you reach.
        assert_ne!(
            canonical_address("http://mcp.example.com/mcp"),
            canonical_address("https://mcp.example.com/mcp")
        );
        assert_ne!(
            canonical_address("https://mcp.example.com/mcp?tenant=a"),
            canonical_address("https://mcp.example.com/mcp?tenant=b")
        );
        assert_ne!(
            canonical_address("https://mcp.example.com/mcp#a"),
            canonical_address("https://mcp.example.com/mcp")
        );
        // An IPv6 literal's own colons are not a port.
        same("https://[2001:DB8::1]/mcp", "https://[2001:db8::1]/mcp");
        assert_eq!(
            canonical_address("https://[2001:db8::1]:8443/mcp").as_deref(),
            Some("https://[2001:db8::1]:8443/mcp")
        );
        // Nothing comparable is never a match — not even with itself.
        for not_an_address in ["", "   ", "mcp.example.com/mcp", "https://", "://x", "npx"] {
            assert_eq!(
                canonical_address(not_an_address),
                None,
                "{not_an_address:?}"
            );
        }
    }

    /// The observation a collision question needs: EVERY entry, whoever wrote it, with the server
    /// it points at — including the address spellings other tools use.
    #[test]
    fn every_entry_is_seen_with_its_address_whoever_wrote_it() {
        // Sorted: the ORDER is whatever each dialect's parser hands back, and no caller reads it.
        let names = |seen: &[SeenEntry]| {
            let mut names = seen.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
            names.sort();
            names
        };
        let address = |seen: &[SeenEntry], name: &str| {
            seen.iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name}: {seen:?}"))
                .address
                .clone()
        };

        let cursor = b"{\n  \"mcpServers\": {\n    \"theirs\": { \"url\": \"HTTPS://MCP.Example/x/\" },\n    \"topos-eng-x\": { \"url\": \"https://mcp.example/x\" },\n    \"local\": { \"command\": \"npx\", \"args\": [\"-y\", \"pkg@1.0.0\"] },\n    \"opaque\": { \"note\": \"nothing here names a server\" }\n  }\n}\n";
        let seen = observe_entries(McpDialect::CursorJson, Some(cursor), None).expect("readable");
        assert_eq!(names(&seen), ["local", "opaque", "theirs", "topos-eng-x"]);
        assert_eq!(
            address(&seen, "theirs").as_deref(),
            Some("https://mcp.example/x/")
        );
        assert_eq!(
            address(&seen, "local").as_deref(),
            Some(r#"run:["npx","-y","pkg@1.0.0"]"#),
            "a command-run server is compared on the command it runs"
        );
        assert_eq!(
            address(&seen, "opaque"),
            None,
            "a shape that names no server has no address to compare"
        );

        // A foreign BRIDGE entry is seen as the address it bridges, and OpenCode's single
        // `command` array reads as the same command line as everyone else's `command` + `args`.
        let bridge = br#"{"mcpServers":{"a":{"command":"npx","args":["-y","mcp-remote","https://mcp.example/x"]}}}"#;
        let seen = observe_entries(McpDialect::CursorJson, Some(bridge), None).expect("readable");
        assert_eq!(
            address(&seen, "a"),
            canonical_address("https://mcp.example/x")
        );
        let oc = br#"{"mcp":{"a":{"type":"local","command":["npx","-y","pkg@1.0.0"]}}}"#;
        let seen = observe_entries(McpDialect::OpencodeJson, Some(oc), None).expect("readable");
        assert_eq!(
            address(&seen, "a").as_deref(),
            Some(r#"run:["npx","-y","pkg@1.0.0"]"#)
        );

        // The other spellings a foreign entry may use for the same fact.
        for (key, dialect, text) in [
            (
                "serverUrl",
                McpDialect::CursorJson,
                "{\"mcpServers\":{\"a\":{\"serverUrl\":\"https://mcp.example/x\"}}}",
            ),
            (
                "httpUrl",
                McpDialect::CursorJson,
                "{\"mcpServers\":{\"a\":{\"httpUrl\":\"https://mcp.example/x\"}}}",
            ),
            (
                "uri",
                McpDialect::CursorJson,
                "{\"mcpServers\":{\"a\":{\"uri\":\"https://mcp.example/x\"}}}",
            ),
        ] {
            let seen = observe_entries(dialect, Some(text.as_bytes()), None).expect("readable");
            assert_eq!(
                address(&seen, "a").as_deref(),
                Some("https://mcp.example/x"),
                "{key}"
            );
        }

        // Every other dialect answers for its own slot.
        let toml = b"[mcp_servers.theirs]\nurl = \"https://mcp.example/x\"\n";
        let seen = observe_entries(McpDialect::CodexToml, Some(toml), None).expect("readable");
        assert_eq!(names(&seen), ["theirs"]);
        assert_eq!(
            address(&seen, "theirs").as_deref(),
            Some("https://mcp.example/x")
        );
        let yaml = b"mcp_servers:\n  theirs: {url: \"https://mcp.example/x\"}\n  block:\n    url: https://mcp.example/y\n";
        let seen = observe_entries(McpDialect::HermesYaml, Some(yaml), None).expect("readable");
        assert_eq!(names(&seen), ["block", "theirs"]);
        assert_eq!(
            address(&seen, "theirs").as_deref(),
            Some("https://mcp.example/x")
        );
        assert_eq!(
            address(&seen, "block"),
            None,
            "a shape this parser does not read keeps its name and claims no address"
        );

        // An absent surface holds nothing; an unreadable one answers NOTHING KNOWN — the two are
        // never the same answer.
        assert_eq!(
            observe_entries(McpDialect::CursorJson, None, None),
            Some(Vec::new())
        );
        assert_eq!(
            observe_entries(McpDialect::CursorJson, Some(b"{ not json"), None),
            None
        );
    }

    /// A SELECTOR reads a slot that is not the dialect's own — the shape `~/.claude.json` keeps,
    /// where every project the file remembers holds its own `mcpServers` map.
    #[test]
    fn a_selector_reads_the_slot_it_names_including_a_wildcard_level() {
        const CLAUDE_JSON: &str = r#"{
  "mcpServers": { "user-scope": { "type": "http", "url": "https://mcp.example/one" } },
  "projects": {
    "/home/me/a": { "mcpServers": { "in-a": { "url": "https://mcp.example/two" } } },
    "/home/me/b": { "mcpServers": { "in-b": { "url": "https://mcp.example/three" } } },
    "/home/me/c": { "allowedTools": [] }
  }
}
"#;
        let at = |selector: &str| {
            observe_entries(
                McpDialect::ClaudeProjectJson,
                Some(CLAUDE_JSON.as_bytes()),
                Some(selector),
            )
            .expect("readable")
            .into_iter()
            .map(|e| (e.name, e.address.unwrap_or_default()))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            at("mcpServers"),
            [(
                "user-scope".to_owned(),
                "https://mcp.example/one".to_owned()
            )]
        );
        assert_eq!(
            at("projects.*.mcpServers"),
            [
                ("in-a".to_owned(), "https://mcp.example/two".to_owned()),
                ("in-b".to_owned(), "https://mcp.example/three".to_owned()),
            ],
            "every project's own map, and a project holding none is simply silent"
        );
        // A path that does not exist holds nothing; a path that is present but wrong-shaped is
        // unreadable, never empty.
        assert_eq!(at("nowhere.at.all"), Vec::new());
        assert_eq!(
            observe_entries(
                McpDialect::ClaudeProjectJson,
                Some(b"{\"mcpServers\": 7}"),
                Some("mcpServers")
            ),
            None
        );
        // The TOML dialect reads a dotted table path, and refuses a wildcard it cannot mean.
        assert_eq!(
            observe_entries(
                McpDialect::CodexToml,
                Some(b"[projects.a.mcp_servers.x]\nurl = \"https://mcp.example/x\"\n"),
                Some("projects.a.mcp_servers")
            )
            .expect("readable")
            .len(),
            1
        );
        assert_eq!(
            observe_entries(McpDialect::CodexToml, Some(b""), Some("a.*.b")),
            None
        );
        // The Hermes splicer reads exactly one block — a selector there is refused, not guessed.
        assert_eq!(
            observe_entries(McpDialect::HermesYaml, Some(b"mcp_servers:\n"), Some("x")),
            None
        );
    }

    #[test]
    fn validate_desired_enforces_the_key_contract() {
        assert!(validate_desired(&[entry("topos-a", "u"), entry("topos-b", "u")]).is_ok());
        for bad in ["linear", "Topos-a", "topos-a b", "topos_a", ""] {
            assert!(validate_desired(&[entry(bad, "u")]).is_err(), "{bad:?}");
        }
        assert!(
            validate_desired(&[entry("topos-a", "u"), entry("topos-a", "v")]).is_err(),
            "duplicate keys are refused"
        );
    }

    #[test]
    fn reconcile_applies_the_drift_rules() {
        let desired = [
            entry("topos-a", "https://a2"),
            entry("topos-b", "https://b"),
        ];
        let fps: Vec<String> = desired
            .iter()
            .map(|e| fingerprint_value(&rendered(McpDialect::CursorJson, e)))
            .collect();
        let fp_a1 = fingerprint_value(&rendered(
            McpDialect::CursorJson,
            &entry("topos-a", "https://a1"),
        ));
        let mut found = BTreeMap::new();
        // topos-a: present at the OLD value, prior-matched → Updated.
        found.insert("topos-a".to_owned(), FoundEntry::Value(fp_a1.clone()));
        // topos-c: present, prior-matched, not desired → Removed.
        found.insert("topos-c".to_owned(), FoundEntry::Value("cfp".to_owned()));
        // topos-d: present, prior MISmatched → Drifted (ledger keeps the old fingerprint).
        found.insert("topos-d".to_owned(), FoundEntry::Value("d-now".to_owned()));
        // topos-e: present, no prior → Foreign.
        found.insert("topos-e".to_owned(), FoundEntry::Value("efp".to_owned()));
        let mut prior = BTreeMap::new();
        prior.insert("topos-a".to_owned(), fp_a1);
        prior.insert("topos-c".to_owned(), "cfp".to_owned());
        prior.insert("topos-d".to_owned(), "d-was".to_owned());
        // topos-gone: in prior, absent from the file → reported nowhere.
        prior.insert("topos-gone".to_owned(), "gfp".to_owned());

        let r = reconcile(&desired, &fps, &found, &prior);
        let state = |k: &str| r.states.iter().find(|(key, _)| key == k).unwrap().1;
        assert_eq!(state("topos-a"), EntryState::Updated);
        assert_eq!(state("topos-b"), EntryState::PlacedNew);
        assert_eq!(state("topos-c"), EntryState::Removed);
        assert_eq!(state("topos-d"), EntryState::Drifted);
        assert_eq!(state("topos-e"), EntryState::Foreign);
        assert!(!r.states.iter().any(|(k, _)| k == "topos-gone"));

        assert_eq!(r.updates, vec![0]);
        assert_eq!(r.inserts, vec![1]);
        assert_eq!(r.removes, vec!["topos-c".to_owned()]);

        let fp_of = |k: &str| r.fingerprints.iter().find(|(key, _)| key == k);
        assert_eq!(fp_of("topos-a").unwrap().1, fps[0]);
        assert_eq!(fp_of("topos-b").unwrap().1, fps[1]);
        assert_eq!(
            fp_of("topos-d").unwrap().1,
            "d-was",
            "a drifted entry keeps the PRIOR fingerprint so drift survives re-runs"
        );
        assert!(fp_of("topos-c").is_none(), "removed → out of the ledger");
        assert!(
            fp_of("topos-e").is_none(),
            "foreign never enters the ledger"
        );
        assert!(fp_of("topos-gone").is_none());
    }

    #[test]
    fn holds_unmanaged_content_separates_topos_created_files_from_user_bytes() {
        let file_dialects = [
            McpDialect::ClaudeProjectJson,
            McpDialect::CursorJson,
            McpDialect::OpencodeJson,
            McpDialect::OpenclawJson,
            McpDialect::CodexToml,
            McpDialect::HermesYaml,
        ];
        for d in file_dialects {
            // Absent / whitespace-only: nothing there at all.
            assert!(!holds_unmanaged_content(d, None), "{d:?}");
            assert!(!holds_unmanaged_content(d, Some(b"  \n")), "{d:?}");
            // A file topos created from scratch holds nothing unmanaged.
            let created = apply(d, None, &[entry("topos-x", "https://x")], &BTreeMap::new());
            let EditPlan::Write(bytes) = &created.plan else {
                panic!("{d:?}: fresh apply writes");
            };
            assert!(!holds_unmanaged_content(d, Some(bytes)), "{d:?}");
            // Garbage is indeterminate → true (fail toward keeping).
            assert!(holds_unmanaged_content(d, Some(b"\xff\xfe")), "{d:?}");
        }
        // A NON-prefixed user entry inside the managed slot answers true in every dialect.
        for (d, text) in [
            (
                McpDialect::CursorJson,
                "{\n  \"mcpServers\": {\n    \"mine\": { \"url\": \"https://u\" }\n  }\n}\n",
            ),
            (
                McpDialect::OpencodeJson,
                "{\n  \"mcp\": {\n    \"mine\": { \"url\": \"https://u\" }\n  }\n}\n",
            ),
            (
                McpDialect::OpenclawJson,
                "{\n  \"mcp\": {\n    \"servers\": {\n      \"mine\": { \"url\": \"https://u\" }\n    }\n  }\n}\n",
            ),
            (
                McpDialect::CodexToml,
                "[mcp_servers.mine]\nurl = \"https://u\"\n",
            ),
            (
                McpDialect::HermesYaml,
                "mcp_servers:\n  mine: {url: \"https://u\"}\n",
            ),
        ] {
            assert!(holds_unmanaged_content(d, Some(text.as_bytes())), "{d:?}");
        }
        // Content OUTSIDE the managed slot answers true; OpenCode's own $schema line does not.
        assert!(holds_unmanaged_content(
            McpDialect::CursorJson,
            Some(b"{\n  \"mcpServers\": {},\n  \"theme\": \"dark\"\n}\n"),
        ));
        assert!(holds_unmanaged_content(
            McpDialect::CodexToml,
            Some(b"model = \"o5\"\n"),
        ));
        assert!(!holds_unmanaged_content(
            McpDialect::OpencodeJson,
            Some(b"{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {}\n}\n"),
        ));
        assert!(holds_unmanaged_content(
            McpDialect::OpencodeJson,
            Some(
                b"{\n  \"$schema\": \"https://elsewhere.example/schema.json\",\n  \"mcp\": {}\n}\n"
            ),
        ));
        // A comment is user content even where the harness tolerates it (topos writes strict
        // JSON everywhere).
        assert!(holds_unmanaged_content(
            McpDialect::OpenclawJson,
            Some(b"// mine\n{\n  \"mcp\": { \"servers\": {} }\n}\n"),
        ));
        // The plugin dir's .mcp.json: its own shape is managed; anything else is not.
        let rendered = plugin_dir::render_plugin_dir(&[entry("topos-a", "https://a")]);
        assert!(!holds_unmanaged_content(
            McpDialect::ClaudePluginDir,
            Some(&rendered[1].1),
        ));
        assert!(holds_unmanaged_content(
            McpDialect::ClaudePluginDir,
            Some(b"{\n  \"mcpServers\": {\n    \"mine\": {}\n  }\n}\n"),
        ));
    }

    /// The dispatcher's byte-preservation precondition: a `Write` over existing content is
    /// allowed ONLY when the input provably re-serializes byte-identical — a file the editor
    /// would silently normalize (CRLF → LF, a stripped BOM) is refused whole, zero byte changes.
    #[test]
    fn the_dispatcher_refuses_to_edit_input_that_does_not_round_trip() {
        let e = [entry("topos-a", "https://a")];
        let none: BTreeMap<String, String> = BTreeMap::new();

        // CRLF TOML: toml_edit re-serializes LF, so an insert would rewrite the user's whole
        // file with foreign line endings — refused at the dispatcher, honestly.
        const CRLF_TOML: &str = "# my codex config\r\nmodel = \"o5\"\r\n";
        let out = apply(McpDialect::CodexToml, Some(CRLF_TOML.as_bytes()), &e, &none);
        let EditPlan::Unprovable(reason) = &out.plan else {
            panic!("CRLF TOML must refuse the edit: {:?}", out.plan);
        };
        assert!(
            reason.contains("without changing bytes it does not own"),
            "{reason}"
        );
        assert!(out.states.is_empty() && out.fingerprints.is_empty());

        // A BOM-carrying TOML file: the BOM would be stripped on rewrite — refused.
        const BOM_TOML: &str = "\u{feff}model = \"o5\"\n";
        let out = apply(McpDialect::CodexToml, Some(BOM_TOML.as_bytes()), &e, &none);
        assert!(
            matches!(out.plan, EditPlan::Unprovable(_)),
            "BOM TOML must refuse the edit: {:?}",
            out.plan
        );

        // Reading is unaffected: a CRLF file whose managed entry is already at the desired
        // value needs no edit — Leave, states honest, zero byte changes.
        let placed = entry("topos-x", "https://u");
        let current_crlf = "[mcp_servers.topos-x]\r\nurl = \"https://u\"\r\n".to_owned();
        let prior: BTreeMap<String, String> = [(
            "topos-x".to_owned(),
            fingerprint_value(&rendered(McpDialect::CodexToml, &placed)),
        )]
        .into_iter()
        .collect();
        let out = apply(
            McpDialect::CodexToml,
            Some(current_crlf.as_bytes()),
            std::slice::from_ref(&placed),
            &prior,
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Current)]
        );

        // A whitespace-only CRLF file is a CREATION (no user content to preserve) — it writes.
        let out = apply(McpDialect::CodexToml, Some(b" \r\n"), &e, &none);
        assert!(out.created_file, "{:?}", out.plan);
        assert!(matches!(out.plan, EditPlan::Write(_)));
    }

    /// CRLF JSON round-trips the lossless CST, so it stays editable — and every untouched user
    /// line keeps its bytes, CRLF included.
    #[test]
    fn crlf_json_stays_editable_with_user_lines_byte_preserved() {
        const CRLF: &str = "{\r\n  \"mcpServers\": {\r\n    \"mine\": {\r\n      \"url\": \"https://user.example\"\r\n    }\r\n  }\r\n}\r\n";
        let out = apply(
            McpDialect::CursorJson,
            Some(CRLF.as_bytes()),
            &[entry("topos-a", "https://a")],
            &BTreeMap::new(),
        );
        let EditPlan::Write(bytes) = &out.plan else {
            panic!("CRLF JSON is losslessly editable: {:?}", out.plan);
        };
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            text.contains("\"mine\": {\r\n      \"url\": \"https://user.example\"\r\n    }"),
            "user CRLF lines byte-preserved: {text:?}"
        );
        assert!(text.contains("topos-a"));
    }

    /// CRLF YAML: the line splicer carries every untouched user line verbatim — CRLF included —
    /// and only adds its own sentinel line.
    #[test]
    fn crlf_yaml_user_lines_are_preserved_byte_for_byte() {
        const CRLF: &str =
            "model: gpt\r\nmcp_servers:\r\n  their: {url: \"https://user.example\"}\r\n";
        let out = apply(
            McpDialect::HermesYaml,
            Some(CRLF.as_bytes()),
            &[entry("topos-a", "https://a")],
            &BTreeMap::new(),
        );
        let EditPlan::Write(bytes) = &out.plan else {
            panic!("CRLF YAML is line-surgically editable: {:?}", out.plan);
        };
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            text.contains("model: gpt\r\n"),
            "user CRLF lines byte-preserved: {text:?}"
        );
        assert!(
            text.contains("  their: {url: \"https://user.example\"}\r\n"),
            "{text:?}"
        );
        assert!(text.contains("topos-a"));
    }

    #[test]
    fn the_dispatcher_routes_every_dialect_including_the_plugin_dir() {
        // A JSON dialect routes (absent + nothing desired = Leave).
        let out = apply(McpDialect::CursorJson, None, &[], &BTreeMap::new());
        assert_eq!(out.plan, EditPlan::Leave);
        // The plugin dir's `.mcp.json` is a real driver surface: a fresh apply writes exactly
        // what the whole-dir renderer would, and the standard drift/removal rules run on it.
        let e = entry("topos-a", "https://a");
        let out = apply(
            McpDialect::ClaudePluginDir,
            None,
            std::slice::from_ref(&e),
            &BTreeMap::new(),
        );
        assert!(out.created_file);
        let EditPlan::Write(bytes) = &out.plan else {
            panic!("fresh plugin apply writes: {:?}", out.plan);
        };
        let rendered = plugin_dir::render_plugin_dir(std::slice::from_ref(&e));
        assert_eq!(bytes, &rendered[1].1, "driver bytes == the renderer's");
        // Idempotent re-apply → Leave; removal returns the managed document to empty.
        let ledger: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();
        let again = apply(McpDialect::ClaudePluginDir, Some(bytes), &[e], &ledger);
        assert_eq!(again.plan, EditPlan::Leave);
        let removed = apply(McpDialect::ClaudePluginDir, Some(bytes), &[], &ledger);
        assert_eq!(
            removed.states,
            vec![("topos-a".to_owned(), EntryState::Removed)]
        );
    }
}
