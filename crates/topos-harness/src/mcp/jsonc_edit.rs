//! The guarded JSON(C) patcher — ONE driver behind five dialects:
//! [`McpDialect::CursorJson`], [`McpDialect::ClaudeProjectJson`], [`McpDialect::OpencodeJson`],
//! [`McpDialect::ClaudePluginDir`] (strict JSON; the plugin dir's `.mcp.json` — the constant
//! manifest beside it is the caller's I/O) and [`McpDialect::OpenclawJson`] (JSONC: comments +
//! trailing commas legal).
//!
//! Editing goes through `jsonc-parser`'s lossless CST so every byte outside our managed entries
//! — user formatting, key order, and (where legal) comments — is preserved; upserts and removes
//! touch ONLY `topos-`-prefixed entries under the dialect's key path (`mcpServers` /
//! `mcp` / `mcp.servers`), with missing parent objects created on write and sibling keys never
//! disturbed.
//!
//! **Strictness is a trust property, not a convenience.** A strict-dialect file must parse as
//! strict JSON (the harness itself would reject extensions — writing "valid JSONC" into
//! Cursor's `mcp.json` would brick it), and the OpenClaw file must parse as JSONC (features
//! beyond it — single quotes, unquoted keys — fail the CST parse). Anything else is
//! [`EditPlan::Unprovable`] with zero byte changes.
//!
//! **Verification, both modes:** the input must round-trip the CST losslessly before any edit;
//! the output must re-parse in the dialect's mode; every entry the plan did not touch — user
//! servers, drifted/foreign topos entries — must be structurally identical between input and
//! output; the touched entries must carry exactly the desired values; and everything OUTSIDE the
//! key path must be structurally unchanged. Any surprise downgrades the plan to `Unprovable` —
//! a failed verification never writes. (Byte preservation outside our entries is additionally
//! pinned by the golden tests; the CST guarantees it by construction.)
//!
//! When the file is absent (or holds only whitespace) a minimal document is synthesized —
//! 2-space indent, trailing newline — and the outcome reports `created_file` so the caller can
//! record whole-file ownership.

use std::collections::{BTreeMap, BTreeSet};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Map, Value};

use super::{
    ApplyOutcome, EditPlan, EntryState, FoundEntry, MANAGED_KEY_PREFIX, McpDialect, McpEntry,
    Observed, Reconcile, effectively_absent, entry_value, fingerprint_value, outcome, reconcile,
    unprovable, validate_desired,
};

/// How strictly the file's syntax must match what the harness itself accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strictness {
    /// Strict JSON only — the harness rejects comments/trailing commas, so we must too.
    Strict,
    /// JSONC — comments + trailing commas legal; anything beyond fails the parse.
    Jsonc,
}

/// OpenCode's own fresh-file sibling — the ONE key a topos-created document carries beside the
/// entries slot.
const OPENCODE_SCHEMA: &str = "https://opencode.ai/config.json";

/// One dialect's parameters: the key path to the entries object, the syntax mode, and the
/// harness display name for honest `Unprovable` reasons.
struct DialectSpec {
    path: &'static [&'static str],
    strictness: Strictness,
    name: &'static str,
}

fn dialect_spec(dialect: McpDialect) -> Option<DialectSpec> {
    Some(match dialect {
        McpDialect::ClaudeProjectJson => DialectSpec {
            path: &["mcpServers"],
            strictness: Strictness::Strict,
            name: "Claude Code",
        },
        // The topos-owned plugin dir's `.mcp.json`: the exact `mcpServers` document topos writes.
        // The caller (the engine) treats ANY unmanaged content there as a back-off, renders the
        // constant manifest beside every write, and prunes the dir when the last entry leaves.
        McpDialect::ClaudePluginDir => DialectSpec {
            path: &["mcpServers"],
            strictness: Strictness::Strict,
            name: "Claude Code plugin",
        },
        McpDialect::CursorJson => DialectSpec {
            path: &["mcpServers"],
            strictness: Strictness::Strict,
            name: "Cursor",
        },
        McpDialect::OpencodeJson => DialectSpec {
            path: &["mcp"],
            strictness: Strictness::Strict,
            name: "OpenCode",
        },
        McpDialect::OpenclawJson => DialectSpec {
            path: &["mcp", "servers"],
            strictness: Strictness::Jsonc,
            name: "OpenClaw",
        },
        _ => return None,
    })
}

/// Compute the placement plan for one JSON(C) surface. See the module doc for the contract.
#[must_use]
pub fn apply(
    dialect: McpDialect,
    current: Option<&[u8]>,
    desired: &[McpEntry],
    prior: &BTreeMap<String, String>,
) -> ApplyOutcome {
    let Some(spec) = dialect_spec(dialect) else {
        return unprovable(format!("{dialect:?} is not a JSON(C) dialect"));
    };
    if let Err(reason) = validate_desired(desired) {
        return unprovable(reason);
    }
    let desired_fps: Vec<String> = desired
        .iter()
        .map(|e| fingerprint_value(&entry_value(dialect, e)))
        .collect();

    if effectively_absent(current) {
        if desired.is_empty() {
            return outcome(EditPlan::Leave, Reconcile::default(), false);
        }
        let mut rec = Reconcile::default();
        for (i, e) in desired.iter().enumerate() {
            rec.states.push((e.key.clone(), EntryState::PlacedNew));
            rec.fingerprints
                .push((e.key.clone(), desired_fps[i].clone()));
        }
        return outcome(
            EditPlan::Write(fresh_doc(dialect, &spec, desired)),
            rec,
            true,
        );
    }
    let bytes = current.unwrap_or_default();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return unprovable(format!("{} config is not UTF-8", spec.name));
    };
    let view = match parse_view(&spec, text) {
        Ok(v) => v,
        Err(reason) => return unprovable(reason),
    };
    let entries_in = match entries_view(&view, spec.path) {
        Ok(map) => map,
        Err(reason) => return unprovable(reason),
    };
    let mut found: BTreeMap<String, FoundEntry> = BTreeMap::new();
    if let Some(map) = entries_in {
        for (key, value) in map {
            if key.starts_with(MANAGED_KEY_PREFIX) {
                found.insert(key.clone(), FoundEntry::Value(fingerprint_value(value)));
            }
        }
    }
    let rec = reconcile(desired, &desired_fps, &found, prior);
    if rec.is_noop() {
        return outcome(EditPlan::Leave, rec, false);
    }

    // The lossless edit: parse the CST in the dialect's mode, prove it round-trips, mutate only
    // our entries, then verify the result before offering any bytes.
    let Ok(root) = CstRootNode::parse(text, &parse_options(spec.strictness)) else {
        return unprovable(format!("{} config failed to parse for editing", spec.name));
    };
    if root.to_string() != text {
        return unprovable(format!(
            "{} config did not round-trip losslessly; refusing to edit",
            spec.name
        ));
    }
    if let Err(reason) = edit(&root, &spec, dialect, desired, &rec) {
        return unprovable(reason);
    }
    let out_text = root.to_string();
    if let Err(reason) = verify(&spec, text, &out_text, desired, &desired_fps, &rec) {
        return unprovable(reason);
    }
    outcome(EditPlan::Write(out_text.into_bytes()), rec, false)
}

/// Read the surface without writing: `key → fingerprint` for every `topos-`-prefixed entry.
#[must_use]
pub fn observe(dialect: McpDialect, current: Option<&[u8]>) -> Observed {
    let unreadable = Observed {
        entries: BTreeMap::new(),
        parseable: false,
    };
    let Some(spec) = dialect_spec(dialect) else {
        return unreadable;
    };
    if effectively_absent(current) {
        return Observed {
            entries: BTreeMap::new(),
            parseable: true,
        };
    }
    let Ok(text) = std::str::from_utf8(current.unwrap_or_default()) else {
        return unreadable;
    };
    let Ok(view) = parse_view(&spec, text) else {
        return unreadable;
    };
    let Ok(entries) = entries_view(&view, spec.path) else {
        return unreadable;
    };
    let mut out = BTreeMap::new();
    if let Some(map) = entries {
        for (key, value) in map {
            if key.starts_with(MANAGED_KEY_PREFIX) {
                out.insert(key.clone(), fingerprint_value(value));
            }
        }
    }
    Observed {
        entries: out,
        parseable: true,
    }
}

/// Whether the file holds content beyond a topos-created document: any top-level key outside the
/// dialect's own fresh-file shape, any non-`topos-` entry in the managed slot, or a syntax
/// (comments, extensions, garbage) topos never writes — a topos-created file is STRICT JSON in
/// every dialect, even OpenClaw's, which merely tolerates more. Fail-closed: anything
/// unreadable answers `true` (the caller's whole-file-ownership reasoning fails TOWARD keeping
/// the file).
#[must_use]
pub fn holds_unmanaged(dialect: McpDialect, bytes: &[u8]) -> bool {
    let Some(spec) = dialect_spec(dialect) else {
        return true;
    };
    let Ok(root) = serde_json::from_slice::<Value>(bytes) else {
        return true;
    };
    walk_unmanaged(&root, spec.path, dialect)
}

/// The recursive half of [`holds_unmanaged`]: at each level of the key path only the slot key
/// (and, at OpenCode's top level, its own `$schema` line at the exact fresh-file value) may
/// exist; at the leaf every entry key must be managed-looking.
fn walk_unmanaged(value: &Value, path: &[&str], dialect: McpDialect) -> bool {
    let Some(obj) = value.as_object() else {
        return true;
    };
    match path.split_first() {
        None => obj.keys().any(|k| !k.starts_with(MANAGED_KEY_PREFIX)),
        Some((slot, rest)) => obj.iter().any(|(k, v)| {
            if k == *slot {
                walk_unmanaged(v, rest, dialect)
            } else {
                !(dialect == McpDialect::OpencodeJson
                    && path.len() == 1
                    && k == "$schema"
                    && v.as_str() == Some(OPENCODE_SCHEMA))
            }
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing: the structural value view (classification + verification) and the CST (editing).
// ---------------------------------------------------------------------------------------------

fn parse_options(strictness: Strictness) -> ParseOptions {
    match strictness {
        Strictness::Strict => ParseOptions {
            allow_comments: false,
            allow_loose_object_property_names: false,
            allow_trailing_commas: false,
        },
        // Comments + trailing commas only — loose property names (JSON5) stay rejected.
        Strictness::Jsonc => ParseOptions {
            allow_comments: true,
            allow_loose_object_property_names: false,
            allow_trailing_commas: true,
        },
    }
}

/// The whole document as a structural [`Value`], in the dialect's syntax mode. The reason
/// distinguishes "carries extensions the harness itself rejects" from "not JSON at all".
fn parse_view(spec: &DialectSpec, text: &str) -> Result<Value, String> {
    match spec.strictness {
        Strictness::Strict => serde_json::from_str::<Value>(text).map_err(|_| {
            if lenient_view(text).is_ok() {
                format!(
                    "config contains JSON extensions (comments or trailing commas) {} itself rejects",
                    spec.name
                )
            } else {
                format!("config is not valid JSON ({})", spec.name)
            }
        }),
        Strictness::Jsonc => lenient_view(text)
            .map_err(|()| format!("config is not valid JSON(C) ({})", spec.name)),
    }
}

fn lenient_view(text: &str) -> Result<Value, ()> {
    let options = parse_options(Strictness::Jsonc);
    // The library's scanner tolerates single-quoted strings unconditionally — that is JSON5,
    // beyond JSONC, and the harness itself rejects it — so the CST walk refuses them explicitly.
    let root = CstRootNode::parse(text, &options).map_err(|_| ())?;
    if root.value().is_some_and(|v| has_single_quoted_string(&v)) {
        return Err(());
    }
    match jsonc_parser::parse_to_value(text, &options) {
        Ok(Some(v)) => Ok(jsonc_to_value(v)),
        // A value-less (e.g. comment-only) file has no root object to reason about.
        Ok(None) | Err(_) => Err(()),
    }
}

/// Any string literal (a value or a property name) not spelled with double quotes.
fn has_single_quoted_string(node: &jsonc_parser::cst::CstNode) -> bool {
    if let Some(lit) = node.as_string_lit() {
        return !lit.raw_value().starts_with('"');
    }
    node.children().iter().any(has_single_quoted_string)
}

fn jsonc_to_value(v: jsonc_parser::JsonValue<'_>) -> Value {
    use jsonc_parser::JsonValue as J;
    match v {
        J::Null => Value::Null,
        J::Boolean(b) => Value::Bool(b),
        // The raw number text; a rendering serde_json cannot represent falls back to the raw
        // string (still a deterministic fingerprint input).
        J::Number(raw) => raw
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .unwrap_or_else(|_| Value::String(raw.to_owned())),
        J::String(s) => Value::String(s.into_owned()),
        J::Array(arr) => Value::Array(arr.take_inner().into_iter().map(jsonc_to_value).collect()),
        J::Object(obj) => {
            let mut m = Map::new();
            for (k, child) in obj {
                m.insert(k, jsonc_to_value(child));
            }
            Value::Object(m)
        }
    }
}

/// The entries object at the dialect's key path in the structural view. `Ok(None)` = the path
/// does not exist yet (created on write); `Err` = something along it is present but not an
/// object (never coerced).
fn entries_view<'v>(
    root: &'v Value,
    path: &[&str],
) -> Result<Option<&'v Map<String, Value>>, String> {
    let mut obj = root
        .as_object()
        .ok_or_else(|| "config root is not a JSON object".to_owned())?;
    for key in path {
        match obj.get(*key) {
            None => return Ok(None),
            Some(v) => {
                obj = v
                    .as_object()
                    .ok_or_else(|| format!("config key `{key}` is not an object"))?;
            }
        }
    }
    Ok(Some(obj))
}

// ---------------------------------------------------------------------------------------------
// The CST edit — upsert/remove ONLY our entries; create missing parents; prune only what our
// removal emptied.
// ---------------------------------------------------------------------------------------------

/// Navigate the CST to the object at `path` (no creation). `None` when absent or wrong-typed.
fn navigate(root: &CstRootNode, path: &[&str]) -> Option<CstObject> {
    let mut obj = root.object_value()?;
    for key in path {
        obj = obj.get(key)?.object_value()?;
    }
    Some(obj)
}

fn edit(
    root: &CstRootNode,
    spec: &DialectSpec,
    dialect: McpDialect,
    desired: &[McpEntry],
    rec: &Reconcile,
) -> Result<(), String> {
    let mut obj = root
        .object_value()
        .ok_or_else(|| "config root is not a JSON object".to_owned())?;
    for key in spec.path {
        obj = match obj.get(key) {
            Some(prop) => prop
                .object_value()
                .ok_or_else(|| format!("config key `{key}` is not an object"))?,
            None => {
                obj.append(key, CstInputValue::Object(Vec::new()));
                obj.get(key)
                    .and_then(|p| p.object_value())
                    .ok_or_else(|| format!("could not create config key `{key}`"))?
            }
        };
    }
    for key in &rec.removes {
        if let Some(prop) = obj.get(key) {
            prop.remove();
        }
    }
    for &i in &rec.updates {
        let entry = &desired[i];
        let prop = obj
            .get(&entry.key)
            .ok_or_else(|| format!("entry `{}` vanished during edit", entry.key))?;
        prop.set_value(to_input(&entry_value(dialect, entry)));
    }
    for &i in &rec.inserts {
        let entry = &desired[i];
        obj.append(&entry.key, to_input(&entry_value(dialect, entry)));
    }
    if !rec.removes.is_empty() {
        prune_emptied_path(root, spec.path);
    }
    Ok(())
}

/// After removals, drop the entries object (and then each parent along the path) — but only
/// when it is empty of properties AND comments, so a user's empty-but-annotated object is never
/// swallowed. Runs only after OUR removals emptied it.
fn prune_emptied_path(root: &CstRootNode, path: &[&str]) {
    for depth in (1..=path.len()).rev() {
        let Some(obj) = navigate(root, &path[..depth]) else {
            return;
        };
        let has_comments = obj
            .children()
            .iter()
            .any(jsonc_parser::cst::CstNode::is_comment);
        if !obj.properties().is_empty() || has_comments {
            return;
        }
        let Some(parent) = navigate(root, &path[..depth - 1]) else {
            return;
        };
        if let Some(prop) = parent.get(path[depth - 1]) {
            prop.remove();
        }
    }
}

fn to_input(v: &Value) -> CstInputValue {
    match v {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_input).collect()),
        // serde_json map iteration is sorted (BTreeMap backend) or our own alphabetical
        // insertion order (preserve_order backend) — deterministic either way.
        Value::Object(m) => CstInputValue::Object(
            m.iter()
                .map(|(k, child)| (k.clone(), to_input(child)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Synthesis + verification.
// ---------------------------------------------------------------------------------------------

/// The minimal fresh document for an absent file — 2-space indent, trailing newline, entries
/// sorted by key.
fn fresh_doc(dialect: McpDialect, spec: &DialectSpec, desired: &[McpEntry]) -> Vec<u8> {
    let mut entries = Map::new();
    let sorted: BTreeMap<&str, &McpEntry> = desired.iter().map(|e| (e.key.as_str(), e)).collect();
    for (key, entry) in sorted {
        entries.insert(key.to_owned(), entry_value(dialect, entry));
    }
    // Build outside-in along the path, then add the dialect's fresh-file siblings. Keys are
    // inserted alphabetically so serialization is deterministic under either map backend.
    let mut value = Value::Object(entries);
    for key in spec.path.iter().rev() {
        let mut wrap = Map::new();
        wrap.insert((*key).to_owned(), value);
        value = Value::Object(wrap);
    }
    if dialect == McpDialect::OpencodeJson
        && let Some(obj) = value.as_object_mut()
    {
        let mut with_schema = Map::new();
        with_schema.insert(
            "$schema".to_owned(),
            Value::String(OPENCODE_SCHEMA.to_owned()),
        );
        for (k, v) in std::mem::take(obj) {
            with_schema.insert(k, v);
        }
        *obj = with_schema;
    }
    let mut text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text.into_bytes()
}

/// Post-edit verification (see the module doc). Any surprise is an `Err` and the caller
/// downgrades to `Unprovable` — a failed verification never writes.
fn verify(
    spec: &DialectSpec,
    input: &str,
    output: &str,
    desired: &[McpEntry],
    desired_fps: &[String],
    rec: &Reconcile,
) -> Result<(), String> {
    let surprise =
        |what: &str| format!("post-edit verification failed ({what}); refusing to write");
    // The output must re-parse in the dialect's own mode (strict output for a strict harness).
    let out_view = parse_view(spec, output).map_err(|_| surprise("output does not re-parse"))?;
    let in_view = parse_view(spec, input).map_err(|_| surprise("input re-parse"))?;

    let touched: BTreeSet<&str> = rec
        .inserts
        .iter()
        .chain(rec.updates.iter())
        .map(|&i| desired[i].key.as_str())
        .chain(rec.removes.iter().map(String::as_str))
        .collect();

    let empty = Map::new();
    let out_entries = entries_view(&out_view, spec.path)
        .map_err(|_| surprise("output key path"))?
        .unwrap_or(&empty);
    let in_entries = entries_view(&in_view, spec.path)
        .map_err(|_| surprise("input key path"))?
        .unwrap_or(&empty);

    // Touched entries landed exactly as planned.
    for &i in rec.inserts.iter().chain(rec.updates.iter()) {
        let key = desired[i].key.as_str();
        let placed = out_entries
            .get(key)
            .ok_or_else(|| surprise("entry missing"))?;
        if fingerprint_value(placed) != desired_fps[i] {
            return Err(surprise("entry bytes are not the desired value"));
        }
    }
    for key in &rec.removes {
        if out_entries.contains_key(key) {
            return Err(surprise("a removed entry is still present"));
        }
    }
    // Untouched entries — user servers, drifted/foreign topos entries — are identical, and no
    // entry appeared from nowhere.
    for (key, value) in in_entries {
        if !touched.contains(key.as_str()) && out_entries.get(key) != Some(value) {
            return Err(surprise("an untouched entry changed"));
        }
    }
    for key in out_entries.keys() {
        if !touched.contains(key.as_str()) && !in_entries.contains_key(key) {
            return Err(surprise("an unexpected entry appeared"));
        }
    }
    // Everything OUTSIDE the key path is structurally unchanged (the path itself is stripped
    // from both sides, normalizing the remove-emptied prune).
    if strip_path(in_view, spec.path) != strip_path(out_view, spec.path) {
        return Err(surprise("content outside the managed path changed"));
    }
    Ok(())
}

/// Remove the entries object at `path` (and then each parent the removal left empty) from a
/// structural view, so views before/after an edit compare equal outside the managed path.
fn strip_path(mut view: Value, path: &[&str]) -> Value {
    fn obj_at<'v>(root: &'v mut Value, path: &[&str]) -> Option<&'v mut Map<String, Value>> {
        let mut obj = root.as_object_mut()?;
        for key in path {
            obj = obj.get_mut(*key)?.as_object_mut()?;
        }
        Some(obj)
    }
    for depth in (1..=path.len()).rev() {
        let key = path[depth - 1];
        let Some(parent) = obj_at(&mut view, &path[..depth - 1]) else {
            break;
        };
        match parent.get(key) {
            // The leaf is always stripped; a parent is stripped only once emptied.
            Some(v) if depth == path.len() => {
                if v.is_object() {
                    parent.remove(key);
                }
            }
            Some(Value::Object(m)) if m.is_empty() => {
                parent.remove(key);
            }
            _ => break,
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{entry, entry_with_headers};
    use super::*;

    fn fp(dialect: McpDialect, e: &McpEntry) -> String {
        fingerprint_value(&entry_value(dialect, e))
    }

    fn ledger(pairs: &[(String, String)]) -> BTreeMap<String, String> {
        pairs.iter().cloned().collect()
    }

    /// apply + expect a Write, returning the new bytes as text.
    fn write_of(out: &ApplyOutcome) -> String {
        match &out.plan {
            EditPlan::Write(bytes) => String::from_utf8(bytes.clone()).unwrap(),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn fresh_file_creation_golden_bytes() {
        let e = entry("topos-acme-linear", "https://mcp.example/linear");
        let none: BTreeMap<String, String> = BTreeMap::new();

        let out = apply(
            McpDialect::CursorJson,
            None,
            std::slice::from_ref(&e),
            &none,
        );
        assert!(out.created_file);
        assert_eq!(
            out.states,
            vec![("topos-acme-linear".to_owned(), EntryState::PlacedNew)]
        );
        assert_eq!(
            write_of(&out),
            "{\n  \"mcpServers\": {\n    \"topos-acme-linear\": {\n      \"url\": \"https://mcp.example/linear\"\n    }\n  }\n}\n"
        );

        let out = apply(
            McpDialect::ClaudeProjectJson,
            None,
            std::slice::from_ref(&e),
            &none,
        );
        assert_eq!(
            write_of(&out),
            "{\n  \"mcpServers\": {\n    \"topos-acme-linear\": {\n      \"type\": \"http\",\n      \"url\": \"https://mcp.example/linear\"\n    }\n  }\n}\n"
        );

        let out = apply(
            McpDialect::OpencodeJson,
            None,
            std::slice::from_ref(&e),
            &none,
        );
        assert_eq!(
            write_of(&out),
            "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {\n    \"topos-acme-linear\": {\n      \"enabled\": true,\n      \"type\": \"remote\",\n      \"url\": \"https://mcp.example/linear\"\n    }\n  }\n}\n"
        );

        let out = apply(McpDialect::OpenclawJson, None, &[e], &none);
        assert_eq!(
            write_of(&out),
            "{\n  \"mcp\": {\n    \"servers\": {\n      \"topos-acme-linear\": {\n        \"transport\": \"streamable-http\",\n        \"url\": \"https://mcp.example/linear\"\n      }\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn a_whitespace_only_file_counts_as_a_creation() {
        let e = entry("topos-a", "https://a");
        let out = apply(
            McpDialect::CursorJson,
            Some(b"  \n"),
            std::slice::from_ref(&e),
            &BTreeMap::new(),
        );
        assert!(out.created_file, "whitespace carries no user content");
        assert!(matches!(out.plan, EditPlan::Write(_)));
    }

    /// The full lifecycle against a user config: add preserves user bytes; update touches only
    /// ours; remove returns the file to user-only content — byte-for-byte at every step.
    #[test]
    fn add_update_remove_lifecycle_preserves_user_bytes() {
        const USER: &str = "{\n  \"mcpServers\": {\n    \"linear\": {\n      \"url\": \"https://user.example\"\n    }\n  },\n  \"theme\": \"dark\"\n}\n";
        let v1 = entry("topos-x", "https://one.example");
        let v2 = entry("topos-x", "https://two.example");

        // Add.
        let out = apply(
            McpDialect::CursorJson,
            Some(USER.as_bytes()),
            std::slice::from_ref(&v1),
            &BTreeMap::new(),
        );
        assert!(!out.created_file);
        let after_add = write_of(&out);
        assert!(
            after_add.contains("\"linear\": {\n      \"url\": \"https://user.example\"\n    }"),
            "user entry bytes survive: {after_add}"
        );
        assert!(after_add.contains("\"theme\": \"dark\""));
        assert!(after_add.contains("\"topos-x\""));
        let ledger1 = ledger(&out.fingerprints);

        // Idempotent re-apply → Leave, byte-identical file.
        let again = apply(
            McpDialect::CursorJson,
            Some(after_add.as_bytes()),
            std::slice::from_ref(&v1),
            &ledger1,
        );
        assert_eq!(again.plan, EditPlan::Leave);
        assert_eq!(
            again.states,
            vec![("topos-x".to_owned(), EntryState::Current)]
        );

        // Update (url change): ours replaced, user bytes still byte-identical.
        let out = apply(
            McpDialect::CursorJson,
            Some(after_add.as_bytes()),
            std::slice::from_ref(&v2),
            &ledger1,
        );
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Updated)]
        );
        let after_update = write_of(&out);
        assert!(after_update.contains("https://two.example"));
        assert!(!after_update.contains("https://one.example"));
        assert!(
            after_update.contains("\"linear\": {\n      \"url\": \"https://user.example\"\n    }")
        );
        let ledger2 = ledger(&out.fingerprints);

        // Remove (desired now empty): the file returns EXACTLY to the user-only original.
        let out = apply(
            McpDialect::CursorJson,
            Some(after_update.as_bytes()),
            &[],
            &ledger2,
        );
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Removed)]
        );
        assert!(out.fingerprints.is_empty(), "removed → out of the ledger");
        assert_eq!(
            write_of(&out),
            USER,
            "user-only content restored byte-for-byte"
        );
    }

    #[test]
    fn opencode_lifecycle_under_the_mcp_key_preserves_user_content() {
        // OpenCode's entries live under top-level `mcp`; the user has their own server + other
        // top-level keys.
        const USER: &str = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {\n    \"mine\": {\n      \"type\": \"remote\",\n      \"url\": \"https://user\"\n    }\n  },\n  \"model\": \"claude\"\n}\n";
        let e = entry("topos-x", "https://x");
        let out = apply(
            McpDialect::OpencodeJson,
            Some(USER.as_bytes()),
            std::slice::from_ref(&e),
            &BTreeMap::new(),
        );
        let after_add = write_of(&out);
        assert!(after_add.contains(
            "\"mine\": {\n      \"type\": \"remote\",\n      \"url\": \"https://user\"\n    }"
        ));
        assert!(after_add.contains("\"model\": \"claude\""));
        assert!(after_add.contains("\"topos-x\""));
        assert!(
            after_add.contains("\"enabled\": true"),
            "opencode entry shape"
        );

        // Remove returns exactly to the user-only original (the user's `mcp` object survives —
        // it still holds their server, so no prune fires).
        let ledger1: BTreeMap<String, String> = out.fingerprints.iter().cloned().collect();
        let out = apply(
            McpDialect::OpencodeJson,
            Some(after_add.as_bytes()),
            &[],
            &ledger1,
        );
        assert_eq!(write_of(&out), USER);
    }

    #[test]
    fn drift_is_left_byte_identical_and_removal_skips_it() {
        // topos-x was placed, then hand-edited (url changed by the user).
        const DRIFTED: &str = "{\n  \"mcpServers\": {\n    \"topos-x\": {\n      \"url\": \"https://hand-edited.example\"\n    }\n  }\n}\n";
        let placed = entry("topos-x", "https://placed.example");
        let prior = ledger(&[("topos-x".to_owned(), fp(McpDialect::CursorJson, &placed))]);

        // Desired at yet another value → Drifted, Leave, zero byte changes.
        let newer = entry("topos-x", "https://newer.example");
        let out = apply(
            McpDialect::CursorJson,
            Some(DRIFTED.as_bytes()),
            &[newer],
            &prior,
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
        assert_eq!(
            out.fingerprints,
            vec![("topos-x".to_owned(), fp(McpDialect::CursorJson, &placed))],
            "the ledger keeps the PRIOR fingerprint so drift survives re-runs"
        );

        // Not desired at all → removal SKIPS the drifted entry (never destroys user edits).
        let out = apply(
            McpDialect::CursorJson,
            Some(DRIFTED.as_bytes()),
            &[],
            &prior,
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-x".to_owned(), EntryState::Drifted)]
        );
    }

    #[test]
    fn a_foreign_topos_entry_is_never_touched() {
        // topos-y exists but topos never wrote it (no prior record).
        const FOREIGN: &str = "{\n  \"mcpServers\": {\n    \"topos-y\": {\n      \"url\": \"https://theirs\"\n    }\n  }\n}\n";
        // Not desired: reported Foreign, untouched.
        let out = apply(
            McpDialect::CursorJson,
            Some(FOREIGN.as_bytes()),
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-y".to_owned(), EntryState::Foreign)]
        );
        assert!(out.fingerprints.is_empty());

        // Desired under the same key: STILL Foreign (we cannot prove it ours) — never replaced.
        let out = apply(
            McpDialect::CursorJson,
            Some(FOREIGN.as_bytes()),
            &[entry("topos-y", "https://ours")],
            &BTreeMap::new(),
        );
        assert_eq!(out.plan, EditPlan::Leave);
        assert_eq!(
            out.states,
            vec![("topos-y".to_owned(), EntryState::Foreign)]
        );
    }

    #[test]
    fn strict_dialects_refuse_extensions_and_garbage() {
        let e = [entry("topos-a", "https://a")];
        let none = BTreeMap::new();
        // A comment in a Cursor config: valid JSONC, but Cursor itself rejects it.
        let out = apply(
            McpDialect::CursorJson,
            Some(b"// mine\n{\"mcpServers\": {}}\n"),
            &e,
            &none,
        );
        let EditPlan::Unprovable(reason) = &out.plan else {
            panic!("expected Unprovable")
        };
        assert!(reason.contains("Cursor itself rejects"), "{reason}");
        assert!(out.states.is_empty() && out.fingerprints.is_empty());

        // Plain garbage.
        let out = apply(McpDialect::CursorJson, Some(b"not json"), &e, &none);
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
        // A non-object root.
        let out = apply(McpDialect::CursorJson, Some(b"[1,2]\n"), &e, &none);
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
        // The key path present but not an object.
        let out = apply(
            McpDialect::CursorJson,
            Some(b"{\"mcpServers\": 3}\n"),
            &e,
            &none,
        );
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
        // Non-UTF-8 bytes.
        let out = apply(McpDialect::CursorJson, Some(&[0xff, 0xfe]), &e, &none);
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
    }

    #[test]
    fn openclaw_accepts_jsonc_but_refuses_json5() {
        let e = [entry("topos-a", "https://a")];
        let none = BTreeMap::new();
        // Comments + trailing commas are legal JSONC — the edit proceeds and preserves them
        // byte-for-byte.
        const JSONC: &str = "{\n  // gateway config\n  \"mcp\": {\n    \"servers\": {\n      \"mine\": {\n        \"url\": \"https://user\",\n      },\n    },\n  },\n}\n";
        let out = apply(McpDialect::OpenclawJson, Some(JSONC.as_bytes()), &e, &none);
        let text = write_of(&out);
        assert!(
            text.contains("// gateway config"),
            "comment preserved: {text}"
        );
        assert!(text.contains("\"mine\": {\n        \"url\": \"https://user\",\n      }"));
        assert!(text.contains("\"topos-a\""));

        // Single quotes are JSON5, beyond JSONC — Unprovable with an honest reason.
        let out = apply(McpDialect::OpenclawJson, Some(b"{'mcp': {}}\n"), &e, &none);
        let EditPlan::Unprovable(reason) = &out.plan else {
            panic!("expected Unprovable")
        };
        assert!(reason.contains("JSON(C)"), "{reason}");
    }

    #[test]
    fn openclaw_nested_path_is_created_without_disturbing_siblings() {
        let e = [entry("topos-a", "https://a")];
        let none = BTreeMap::new();

        // `mcp` exists (with a sibling key) but `servers` does not.
        const HAS_MCP: &str =
            "{\n  \"mcp\": {\n    \"port\": 9000\n  },\n  \"theme\": \"dark\"\n}\n";
        let out = apply(
            McpDialect::OpenclawJson,
            Some(HAS_MCP.as_bytes()),
            &e,
            &none,
        );
        let text = write_of(&out);
        assert!(text.contains("\"port\": 9000"), "{text}");
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"servers\""));
        assert!(text.contains("\"topos-a\""));

        // Neither `mcp` nor `servers` exists.
        const NO_MCP: &str = "{\n  \"theme\": \"dark\"\n}\n";
        let out = apply(McpDialect::OpenclawJson, Some(NO_MCP.as_bytes()), &e, &none);
        let text = write_of(&out);
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"mcp\""));
        assert!(text.contains("\"servers\""));

        // And removing ours prunes what our write created, returning to the original.
        let ledger1 = ledger(&out.fingerprints);
        let out = apply(
            McpDialect::OpenclawJson,
            Some(text.as_bytes()),
            &[],
            &ledger1,
        );
        assert_eq!(write_of(&out), NO_MCP, "created parents pruned on remove");
    }

    #[test]
    fn header_rendering_lands_in_the_file() {
        let e = [entry_with_headers(
            "topos-h",
            "https://h",
            &[("X-T", "v"), ("A-B", "c")],
        )];
        let none = BTreeMap::new();
        for dialect in [
            McpDialect::ClaudeProjectJson,
            McpDialect::CursorJson,
            McpDialect::OpencodeJson,
            McpDialect::OpenclawJson,
        ] {
            let out = apply(dialect, None, &e, &none);
            let text = write_of(&out);
            assert!(text.contains("\"headers\""), "{dialect:?}: {text}");
            assert!(text.contains("\"X-T\": \"v\""), "{dialect:?}: {text}");
            // Name-sorted: A-B before X-T.
            assert!(
                text.find("\"A-B\"").unwrap() < text.find("\"X-T\"").unwrap(),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn observe_reads_without_writing() {
        // Absent → readable, empty.
        let o = observe(McpDialect::CursorJson, None);
        assert!(o.parseable && o.entries.is_empty());
        // A user + managed mix → only the topos- entries, fingerprinted.
        let placed = entry("topos-x", "https://one");
        let text = write_of(&apply(
            McpDialect::CursorJson,
            Some(b"{\n  \"mcpServers\": {\n    \"linear\": {\n      \"url\": \"https://u\"\n    }\n  }\n}\n"),
            std::slice::from_ref(&placed),
            &BTreeMap::new(),
        ));
        let o = observe(McpDialect::CursorJson, Some(text.as_bytes()));
        assert!(o.parseable);
        assert_eq!(
            o.entries,
            ledger(&[("topos-x".to_owned(), fp(McpDialect::CursorJson, &placed))])
        );
        // Unparsable → parseable: false, never a guess.
        let o = observe(McpDialect::CursorJson, Some(b"{broken"));
        assert!(!o.parseable && o.entries.is_empty());
        // A strict surface carrying comments is unreadable FOR THAT HARNESS.
        let o = observe(McpDialect::CursorJson, Some(b"// c\n{}"));
        assert!(!o.parseable);
    }

    #[test]
    fn desired_key_contract_violations_fail_closed() {
        let out = apply(
            McpDialect::CursorJson,
            None,
            &[entry("linear", "https://x")],
            &BTreeMap::new(),
        );
        assert!(matches!(out.plan, EditPlan::Unprovable(_)));
    }
}
