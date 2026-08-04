//! The Claude Code plugin-dir renderer — the [`McpDialect::ClaudePluginDir`] surface's constant
//! parts. The surface is a topos-OWNED directory (`<claude home>/skills/topos-mcp/`) holding two
//! files; its `.mcp.json` is an ordinary strict-JSON driver surface (`jsonc_edit`, top-level
//! `mcpServers` — route through [`apply`](super::apply)/[`observe`](super::observe) like any
//! dialect), while THIS module renders what the driver cannot know: the constant
//! `.claude-plugin/plugin.json` manifest, and the whole-dir shape a fresh placement creates. The
//! CLI owns all directory I/O (write, manifest upkeep, prune on the last entry's removal).
//!
//! Rendered files:
//! - `.claude-plugin/plugin.json` — the constant plugin manifest.
//! - `.mcp.json` — `{"mcpServers": {…}}`, entries in the Claude entry shape
//!   (`{"type": "http", "url": …, "headers": …}`), keys sorted, 2-space indent, trailing
//!   newline — byte-identical to what the JSON driver's fresh-file synthesis writes (asserted
//!   in the dispatcher tests).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{McpDialect, McpEntry, entry_value};

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

/// The constant `.claude-plugin/plugin.json` bytes — what the caller writes (and re-heals)
/// beside every `.mcp.json` it places.
#[must_use]
pub fn manifest_bytes() -> Vec<u8> {
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
    fn empty_entries_render_an_empty_servers_object() {
        let files = render_plugin_dir(&[]);
        assert_eq!(
            String::from_utf8(files[1].1.clone()).unwrap(),
            "{\n  \"mcpServers\": {}\n}\n"
        );
    }
}
