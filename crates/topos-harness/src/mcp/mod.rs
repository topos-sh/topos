//! `mcp` — pure MCP-server config placement for six harnesses. Bytes in → an [`EditPlan`] out;
//! the CLI owns ALL file I/O (read, crash-safe write, dir materialization) exactly as it does for
//! the trigger adapters. Nothing here touches a filesystem, a clock, or the environment (the one
//! exception: [`descriptor::user_surface_path`] reads the per-harness home-override env vars, the
//! same rule the registry uses).
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
//! Empty `headers` are omitted in every dialect (an empty map carries no information and strict
//! validators are the risk surface).

pub mod descriptor;
pub mod jsonc_edit;
pub mod plugin_dir;
pub mod toml_patch;
pub mod yaml_splice;

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use descriptor::{McpDialect, McpHarness, McpSurface, SurfaceRoot};

/// One desired managed entry (harness-neutral).
#[derive(Debug, Clone)]
pub struct McpEntry {
    /// The immutable config key, e.g. `topos-acme-linear`. Charset `[a-z0-9-]` and the `topos-`
    /// prefix are caller-guaranteed — and re-verified by every driver (fail-closed), because the
    /// prefix is what later runs key managed-LOOKING recognition on.
    pub key: String,
    pub url: String,
    /// Literal, non-secret header pairs (already gate-validated by the caller). Emitted sorted
    /// by name in every dialect so output bytes are deterministic.
    pub headers: Vec<(String, String)>,
    pub auth: AuthHint,
}

/// What the caller knows about the server's auth story. Only the Hermes dialect renders it
/// (`auth: oauth`, emitted for [`AuthHint::Oauth`] alone — Hermes needs the explicit opt-in but
/// must not be sent into OAuth for a no-auth server); everywhere else OAuth is the harness's own
/// on-401 behavior.
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

/// Compute MCP placements for one config surface (routes to the driver). For
/// [`McpDialect::ClaudePluginDir`] pass the plugin dir's `.mcp.json` bytes — the strict JSON
/// driver patches it like any other surface; the constant manifest beside it
/// ([`plugin_dir::render_plugin_dir`]) is the caller's I/O.
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
        | McpDialect::ClaudePluginDir => jsonc_edit::apply(dialect, current, desired, prior),
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
            "the config does not re-serialize byte-identical (a BOM or unusual line endings?); \
             refusing to edit",
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
        | McpDialect::ClaudePluginDir => jsonc_edit::round_trips(dialect, text),
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
        | McpDialect::ClaudePluginDir => jsonc_edit::observe(dialect, current),
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
        | McpDialect::ClaudePluginDir => jsonc_edit::holds_unmanaged(dialect, bytes),
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
/// are omitted in every dialect.
#[must_use]
pub fn entry_value(dialect: McpDialect, entry: &McpEntry) -> Value {
    let mut m = Map::new();
    match dialect {
        // `type` is MANDATORY here — Claude Code refuses a typeless remote entry.
        McpDialect::ClaudePluginDir | McpDialect::ClaudeProjectJson => {
            insert_headers(&mut m, entry);
            m.insert("type".to_owned(), Value::String("http".to_owned()));
            m.insert("url".to_owned(), Value::String(entry.url.clone()));
        }
        // NEVER a `type` key: a `type: "streamable-http"` makes cursor-agent silently drop the
        // ENTIRE file.
        McpDialect::CursorJson => {
            insert_headers(&mut m, entry);
            m.insert("url".to_owned(), Value::String(entry.url.clone()));
        }
        McpDialect::OpencodeJson => {
            m.insert("enabled".to_owned(), Value::Bool(true));
            insert_headers(&mut m, entry);
            m.insert("type".to_owned(), Value::String("remote".to_owned()));
            m.insert("url".to_owned(), Value::String(entry.url.clone()));
        }
        // `transport` EXPLICIT (OpenClaw's silent default is sse) and NEVER any other key
        // (OpenClaw strictly rejects unknown keys and a bad config bricks its gateway startup).
        McpDialect::OpenclawJson => {
            insert_headers(&mut m, entry);
            m.insert(
                "transport".to_owned(),
                Value::String("streamable-http".to_owned()),
            );
            m.insert("url".to_owned(), Value::String(entry.url.clone()));
        }
        // ONLY `url` and (when headers exist) `http_headers` — a wrong key hard-fails Codex's
        // whole config load. This is the FINGERPRINT shape; the TOML emission mirrors it.
        McpDialect::CodexToml => {
            if let Some(h) = headers_object(entry) {
                m.insert("http_headers".to_owned(), h);
            }
            m.insert("url".to_owned(), Value::String(entry.url.clone()));
        }
        // The parsed shape of the one-line flow mapping `yaml_splice` emits. `auth: oauth` only
        // on the explicit hint — Unknown/None omit it.
        McpDialect::HermesYaml => {
            if entry.auth == AuthHint::Oauth {
                m.insert("auth".to_owned(), Value::String("oauth".to_owned()));
            }
            insert_headers(&mut m, entry);
            m.insert("url".to_owned(), Value::String(entry.url.clone()));
        }
    }
    Value::Object(m)
}

fn insert_headers(m: &mut Map<String, Value>, entry: &McpEntry) {
    if let Some(h) = headers_object(entry) {
        m.insert("headers".to_owned(), h);
    }
}

/// The headers as a name-sorted JSON object, or `None` when there are none (empty headers are
/// omitted in every dialect). A duplicated name is last-wins, matching what any JSON/TOML/YAML
/// mapping would resolve to anyway.
fn headers_object(entry: &McpEntry) -> Option<Value> {
    if entry.headers.is_empty() {
        return None;
    }
    let sorted: BTreeMap<&str, &str> = entry
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut m = Map::new();
    for (k, v) in sorted {
        m.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    Some(Value::Object(m))
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
    use super::{AuthHint, McpEntry};

    pub(crate) fn entry(key: &str, url: &str) -> McpEntry {
        McpEntry {
            key: key.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            auth: AuthHint::Unknown,
        }
    }

    pub(crate) fn entry_with_headers(key: &str, url: &str, headers: &[(&str, &str)]) -> McpEntry {
        McpEntry {
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..entry(key, url)
        }
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

    #[test]
    fn entry_values_carry_exactly_the_dialect_keys() {
        let plain = entry("topos-x", "https://mcp.example/x");
        let keys = |d: McpDialect, e: &McpEntry| -> Vec<String> {
            entry_value(d, e)
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
            "openclaw: transport explicit, nothing else"
        );
        assert_eq!(keys(McpDialect::CodexToml, &plain), ["url"]);
        assert_eq!(
            keys(McpDialect::HermesYaml, &plain),
            ["url"],
            "hermes: Unknown auth omits the opt-in"
        );
        assert_eq!(
            entry_value(McpDialect::OpenclawJson, &plain)["transport"],
            "streamable-http"
        );
        assert_eq!(
            entry_value(McpDialect::OpencodeJson, &plain)["type"],
            "remote"
        );
        assert_eq!(
            entry_value(McpDialect::OpencodeJson, &plain)["enabled"],
            true
        );

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
            let v = entry_value(d, &with);
            let h = v["headers"].as_object().unwrap();
            assert_eq!(h.keys().collect::<Vec<_>>(), ["A", "Z"], "{d:?}");
        }
        let codex = entry_value(McpDialect::CodexToml, &with);
        assert!(codex.get("headers").is_none());
        assert_eq!(codex["http_headers"]["A"], "1");
    }

    #[test]
    fn hermes_auth_is_emitted_only_on_the_explicit_oauth_hint() {
        for (hint, expect_auth) in [
            (AuthHint::Oauth, true),
            (AuthHint::None, false),
            (AuthHint::Unknown, false),
        ] {
            let e = McpEntry {
                auth: hint,
                ..entry("topos-x", "https://u")
            };
            let v = entry_value(McpDialect::HermesYaml, &e);
            assert_eq!(v.get("auth").is_some(), expect_auth, "{hint:?}");
            if expect_auth {
                assert_eq!(v["auth"], "oauth");
            }
        }
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
            .map(|e| fingerprint_value(&entry_value(McpDialect::CursorJson, e)))
            .collect();
        let fp_a1 = fingerprint_value(&entry_value(
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
        assert!(reason.contains("byte-identical"), "{reason}");
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
            fingerprint_value(&entry_value(McpDialect::CodexToml, &placed)),
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
