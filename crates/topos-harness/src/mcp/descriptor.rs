//! The MCP CONFIG DIALECTS — which placement driver a harness's config surface speaks, plus the two
//! views that narrow the [`registry`] table to the harnesses that have an MCP surface
//! at all.
//!
//! WHERE each harness reads server config from, and how a change gets picked up, is a column on that
//! one table ([`registry::KnownHarness::mcp`]) — so the placement engine's detection probe and the
//! config file it must edit can never disagree about which harness is which. Path resolution is the
//! registry's ([`registry::resolve_spec`]): an env override read trimmed/non-empty, else a
//! home-relative default, so every surface stays testable against a temp home.

use crate::registry::{self, KnownHarness};

/// Which placement driver a surface's bytes go through (and the exact entry shape — see
/// [`entry_value`](super::entry_value)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDialect {
    /// A topos-OWNED Claude Code plugin directory: its `.mcp.json` is a strict-JSON driver
    /// surface (top-level `mcpServers`); `plugin_dir` renders the constant manifest beside it.
    ClaudePluginDir,
    /// A project `.mcp.json` (strict JSON, top-level `mcpServers`).
    ClaudeProjectJson,
    /// Cursor's `mcp.json` (strict JSON, top-level `mcpServers`).
    CursorJson,
    /// OpenCode's `opencode.json` (strict JSON, top-level `mcp`).
    OpencodeJson,
    /// OpenClaw's `openclaw.json` (JSONC — comments + trailing commas legal; nested
    /// `mcp.servers`).
    OpenclawJson,
    /// Codex's `config.toml` (`[mcp_servers.<key>]` tables).
    CodexToml,
    /// Hermes's `config.yaml` (one-line flow-mapping entries under `mcp_servers:`).
    HermesYaml,
}

/// Every MCP-capable harness row, in registry-table order — a filtered view over the one table.
#[must_use]
pub fn mcp_harnesses() -> Vec<&'static KnownHarness> {
    registry::known_harnesses()
        .iter()
        .filter(|h| h.mcp().is_some())
        .collect()
}

/// The row for a registry slug, or `None` when the slug is unknown or the harness has no MCP support.
#[must_use]
pub fn mcp_harness(slug: &str) -> Option<&'static KnownHarness> {
    registry::known_harness(slug).filter(|h| h.mcp().is_some())
}

/// The MCP view a TEARDOWN iterates — the same filter over
/// [`registry::teardown_harnesses`]'s loaded-∪-bundled union, so an entry topos placed under a
/// row a downloaded table later dropped is still found, named, and retired. Placement and
/// converge keep reading [`mcp_harnesses`]: a dropped row gains nothing new, it only stays
/// cleanable.
#[must_use]
pub fn mcp_harnesses_for_teardown() -> Vec<&'static KnownHarness> {
    registry::teardown_harnesses()
        .into_iter()
        .filter(|h| h.mcp().is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Root;
    use std::path::{Path, PathBuf};

    /// The teardown view never sees LESS than placement does — it is the same filter over the
    /// union that keeps a dropped row cleanable, so every placeable surface stays scrubbable.
    #[test]
    fn the_teardown_view_covers_every_placement_surface() {
        let placement: Vec<&str> = mcp_harnesses().iter().map(|h| h.slug).collect();
        let teardown: Vec<&str> = mcp_harnesses_for_teardown()
            .iter()
            .map(|h| h.slug)
            .collect();
        for slug in &placement {
            assert!(
                teardown.contains(slug),
                "{slug} placeable but not scrubbable"
            );
        }
    }

    /// The MCP view over the BUNDLED table — [`mcp_harnesses`]'s own filter, over the rows this
    /// BUILD ships. A shape pin answers for the commit under test, so it must not be movable by
    /// the `~/.topos/harness-registry/` of whoever is running the suite.
    fn bundled_mcp_rows() -> Vec<&'static KnownHarness> {
        registry::bundled_harnesses()
            .iter()
            .filter(|h| h.mcp().is_some())
            .collect()
    }

    /// [`mcp_harness`] over the bundled rows.
    fn bundled_mcp_row(slug: &str) -> Option<&'static KnownHarness> {
        bundled_mcp_rows().into_iter().find(|h| h.slug == slug)
    }

    #[test]
    fn the_mcp_capable_rows_are_the_six_verified_harnesses() {
        let slugs: Vec<&str> = bundled_mcp_rows().iter().map(|h| h.slug).collect();
        assert_eq!(
            slugs,
            [
                "claude-code",
                "openclaw",
                "codex",
                "cursor",
                "hermes-agent",
                "opencode"
            ],
            "registry-table order"
        );
        for h in bundled_mcp_rows() {
            assert!(
                !h.mcp().expect("mcp row").reload_note.is_empty(),
                "{}",
                h.slug
            );
        }
        assert!(bundled_mcp_row("cline").is_none(), "no MCP surface, no row");
        assert!(bundled_mcp_row("not-a-harness").is_none());
        // …and the runtime view is the same filter over whatever table THIS machine resolved.
        assert!(mcp_harness("cline").is_none());
    }

    #[test]
    fn surfaces_are_as_specified() {
        let get = |slug: &str| bundled_mcp_row(slug).unwrap().mcp().unwrap();

        let cc = get("claude-code");
        let s = cc.user.unwrap();
        assert_eq!(s.dir.root(), Root::ClaudeHome);
        assert_eq!(s.dir.suffix(), "skills/topos-mcp");
        assert_eq!(s.dialect, McpDialect::ClaudePluginDir);
        assert_eq!(
            cc.project,
            Some((".mcp.json", McpDialect::ClaudeProjectJson))
        );

        let codex = get("codex");
        let s = codex.user.unwrap();
        assert_eq!(
            (s.dir.root(), s.dir.suffix()),
            (Root::CodexHome, "config.toml")
        );
        assert_eq!(s.dialect, McpDialect::CodexToml);
        assert_eq!(
            codex.project,
            Some((".codex/config.toml", McpDialect::CodexToml))
        );

        let cursor = get("cursor");
        let s = cursor.user.unwrap();
        assert_eq!(
            (s.dir.root(), s.dir.suffix()),
            (Root::Home, ".cursor/mcp.json")
        );
        assert_eq!(s.dialect, McpDialect::CursorJson);
        assert_eq!(
            cursor.project,
            Some((".cursor/mcp.json", McpDialect::CursorJson))
        );

        let oc = get("opencode");
        let s = oc.user.unwrap();
        assert_eq!(
            (s.dir.root(), s.dir.suffix()),
            (Root::Config, "opencode/opencode.json")
        );
        assert_eq!(s.dialect, McpDialect::OpencodeJson);
        assert_eq!(
            oc.project,
            Some(("opencode.json", McpDialect::OpencodeJson))
        );

        let claw = get("openclaw");
        let s = claw.user.unwrap();
        assert_eq!(
            (s.dir.root(), s.dir.suffix()),
            (Root::Home, ".openclaw/openclaw.json")
        );
        assert_eq!(s.dialect, McpDialect::OpenclawJson);
        assert_eq!(claw.project, None);

        let hermes = get("hermes-agent");
        let s = hermes.user.unwrap();
        assert_eq!(
            (s.dir.root(), s.dir.suffix()),
            (Root::HermesHome, "config.yaml")
        );
        assert_eq!(s.dialect, McpDialect::HermesYaml);
        assert_eq!(hermes.project, None);
    }

    #[test]
    fn user_surface_paths_resolve_under_home() {
        let home = Path::new("/test-home");
        let path = |slug: &str| bundled_mcp_row(slug).unwrap().mcp_user_path(home);
        // Home-rooted rows resolve deterministically (no env override to perturb the test).
        assert_eq!(
            path("cursor"),
            Some(PathBuf::from("/test-home/.cursor/mcp.json"))
        );
        assert_eq!(
            path("openclaw"),
            Some(PathBuf::from("/test-home/.openclaw/openclaw.json"))
        );
        // Env-rooted rows resolve to the developer's REAL override when one is set (a test can't
        // unset env vars here), so assert the invariant part: the suffix.
        let cc = path("claude-code").unwrap();
        assert!(cc.ends_with("skills/topos-mcp"), "{cc:?}");
        let codex = path("codex").unwrap();
        assert!(codex.ends_with("config.toml"), "{codex:?}");
        let hermes = path("hermes-agent").unwrap();
        assert!(hermes.ends_with("config.yaml"), "{hermes:?}");
        let oc = path("opencode").unwrap();
        assert!(oc.ends_with("opencode/opencode.json"), "{oc:?}");
        // With no `$XDG_CONFIG_HOME` it lands under `home/.config`.
        if std::env::var_os("XDG_CONFIG_HOME").is_none() {
            assert_eq!(
                oc,
                PathBuf::from("/test-home/.config/opencode/opencode.json")
            );
        }
    }
}
