//! The Claude Code plugin-dir renderer — [`McpDialect::ClaudePluginDir`]. Unlike every other
//! dialect this surface is a topos-OWNED directory (`<claude home>/skills/topos-mcp/`), so there
//! is no foreign content to preserve: the two files are rendered WHOLE, deterministically, and
//! the CLI owns all directory I/O (placement, swap, delete). Drift checking still runs — the
//! caller compares [`parse_plugin_mcp`]'s fingerprints against its ledger before overwriting, so
//! a hand-edited copy is detected exactly like any other dialect's drift.
//!
//! Rendered files:
//! - `.claude-plugin/plugin.json` — the constant plugin manifest.
//! - `.mcp.json` — `{"mcpServers": {…}}`, entries in the Claude entry shape
//!   (`{"type": "http", "url": …, "headers": …}`), keys sorted, 2-space indent, trailing
//!   newline.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{McpDialect, McpEntry, Observed, entry_value, fingerprint_value};

/// The manifest file's dir-relative path.
pub const PLUGIN_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";

/// The MCP config file's dir-relative path.
pub const PLUGIN_MCP_PATH: &str = ".mcp.json";

/// Render the whole plugin dir: `(dir-relative path, bytes)` for the manifest and the MCP
/// config. Deterministic — same entries, same bytes.
#[must_use]
pub fn render_plugin_dir(entries: &[McpEntry]) -> Vec<(String, Vec<u8>)> {
    vec![
        (PLUGIN_MANIFEST_PATH.to_owned(), manifest_bytes()),
        (PLUGIN_MCP_PATH.to_owned(), mcp_bytes(entries)),
    ]
}

/// The `key → fingerprint` view of a plugin `.mcp.json` for drift checking. `None` when the
/// bytes are not strict JSON with an object `mcpServers` (the dir is wholly ours, so any other
/// shape IS drift). Every key is reported — a topos-owned file holds nothing foreign.
#[must_use]
pub fn parse_plugin_mcp(bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let servers = root.as_object()?.get("mcpServers")?.as_object()?;
    Some(
        servers
            .iter()
            .map(|(key, value)| (key.clone(), fingerprint_value(value)))
            .collect(),
    )
}

/// What the ledger records after placing `entries` — `(key, fingerprint)` in the callers'
/// order, matching what [`parse_plugin_mcp`] reads back from the rendered bytes.
#[must_use]
pub fn plugin_dir_fingerprints(entries: &[McpEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|e| {
            (
                e.key.clone(),
                fingerprint_value(&entry_value(McpDialect::ClaudePluginDir, e)),
            )
        })
        .collect()
}

/// Observe a plugin `.mcp.json` (the dialect's status read).
#[must_use]
pub fn observe(current: Option<&[u8]>) -> Observed {
    match current {
        None => Observed {
            entries: BTreeMap::new(),
            parseable: true,
        },
        Some(bytes) => match parse_plugin_mcp(bytes) {
            Some(entries) => Observed {
                entries,
                parseable: true,
            },
            None => Observed {
                entries: BTreeMap::new(),
                parseable: false,
            },
        },
    }
}

fn manifest_bytes() -> Vec<u8> {
    // Inserted alphabetically so serialization is deterministic under either map backend.
    let mut m = Map::new();
    m.insert(
        "description".to_owned(),
        Value::String("Team-shared MCP servers delivered by Topos".to_owned()),
    );
    m.insert(
        "displayName".to_owned(),
        Value::String("Topos MCP".to_owned()),
    );
    m.insert("name".to_owned(), Value::String("topos-mcp".to_owned()));
    m.insert("version".to_owned(), Value::String("0.1.0".to_owned()));
    pretty(&Value::Object(m))
}

fn mcp_bytes(entries: &[McpEntry]) -> Vec<u8> {
    let sorted: BTreeMap<&str, &McpEntry> = entries.iter().map(|e| (e.key.as_str(), e)).collect();
    let mut servers = Map::new();
    for (key, entry) in sorted {
        servers.insert(
            key.to_owned(),
            entry_value(McpDialect::ClaudePluginDir, entry),
        );
    }
    let mut root = Map::new();
    root.insert("mcpServers".to_owned(), Value::Object(servers));
    pretty(&Value::Object(root))
}

fn pretty(value: &Value) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{entry, entry_with_headers};
    use super::*;

    #[test]
    fn renders_the_two_files_with_golden_bytes() {
        let files = render_plugin_dir(&[
            entry_with_headers("topos-b", "https://b.example", &[("X-T", "v")]),
            entry("topos-a", "https://a.example"),
        ]);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, ".claude-plugin/plugin.json");
        assert_eq!(
            String::from_utf8(files[0].1.clone()).unwrap(),
            "{\n  \"description\": \"Team-shared MCP servers delivered by Topos\",\n  \"displayName\": \"Topos MCP\",\n  \"name\": \"topos-mcp\",\n  \"version\": \"0.1.0\"\n}\n"
        );
        assert_eq!(files[1].0, ".mcp.json");
        // Keys sorted (topos-a before topos-b despite input order); the Claude entry shape with
        // mandatory `type` and name-sorted headers; 2-space indent; trailing newline.
        assert_eq!(
            String::from_utf8(files[1].1.clone()).unwrap(),
            "{\n  \"mcpServers\": {\n    \"topos-a\": {\n      \"type\": \"http\",\n      \"url\": \"https://a.example\"\n    },\n    \"topos-b\": {\n      \"headers\": {\n        \"X-T\": \"v\"\n      },\n      \"type\": \"http\",\n      \"url\": \"https://b.example\"\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn fingerprints_round_trip_through_the_rendered_bytes() {
        let entries = [
            entry("topos-a", "https://a.example"),
            entry_with_headers("topos-b", "https://b.example", &[("X-T", "v")]),
        ];
        let want: BTreeMap<String, String> =
            plugin_dir_fingerprints(&entries).into_iter().collect();
        let files = render_plugin_dir(&entries);
        let got = parse_plugin_mcp(&files[1].1).expect("rendered bytes parse");
        assert_eq!(got, want, "what we render is what the ledger records");
    }

    #[test]
    fn parse_and_observe_fail_closed_on_foreign_shapes() {
        assert!(parse_plugin_mcp(b"not json").is_none());
        assert!(parse_plugin_mcp(b"{\"mcpServers\": 3}").is_none());
        assert!(parse_plugin_mcp(b"{}").is_none(), "no mcpServers key");
        let o = observe(Some(b"not json"));
        assert!(!o.parseable && o.entries.is_empty());
        let o = observe(None);
        assert!(o.parseable && o.entries.is_empty());
        // A hand-edited entry shows up as a fingerprint mismatch against the ledger.
        let placed = [entry("topos-a", "https://a")];
        let ledger = plugin_dir_fingerprints(&placed);
        let edited = "{\n  \"mcpServers\": {\n    \"topos-a\": {\n      \"type\": \"http\",\n      \"url\": \"https://hand-edited\"\n    }\n  }\n}\n";
        let got = parse_plugin_mcp(edited.as_bytes()).unwrap();
        assert_ne!(got.get("topos-a"), Some(&ledger[0].1), "drift is visible");
    }

    #[test]
    fn empty_entries_render_an_empty_servers_object() {
        let files = render_plugin_dir(&[]);
        assert_eq!(
            String::from_utf8(files[1].1.clone()).unwrap(),
            "{\n  \"mcpServers\": {}\n}\n"
        );
    }
}
