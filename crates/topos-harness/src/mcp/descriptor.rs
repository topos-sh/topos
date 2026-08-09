//! The MCP harness descriptor table — WHERE each of the six MCP-capable harnesses reads
//! server config from, in which dialect, and how a change gets picked up. Facts verified against
//! live docs + local binaries; slugs match [`registry`](crate::registry) rows exactly (asserted
//! in tests), so the placement engine joins this table onto the registry's detection probe.
//!
//! Path resolution mirrors the registry's rule: an env override (`$XDG_CONFIG_HOME`,
//! `$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, `$HERMES_HOME`) read trimmed/non-empty, else a
//! home-relative default — so every resolver stays testable against a temp home.

use std::path::{Path, PathBuf};

use crate::triggers::{env_override, resolve_config_home};

/// One MCP-capable harness's config surfaces.
#[derive(Debug)]
pub struct McpHarness {
    /// The stable machine slug — matches [`registry`](crate::registry) slugs.
    pub slug: &'static str,
    pub display_name: &'static str,
    /// The user/global config surface, if the harness has one.
    pub user_surface: Option<McpSurface>,
    /// Project-relative config path + dialect (`None` = no project surface).
    pub project_surface: Option<(&'static str, McpDialect)>,
    /// Receipt copy: how config changes get picked up.
    pub reload_note: &'static str,
}

/// A user-scope config surface: a resolution root + a `/`-separated suffix under it + the
/// dialect the file speaks.
#[derive(Debug, Clone, Copy)]
pub struct McpSurface {
    pub root: SurfaceRoot,
    pub suffix: &'static str,
    pub dialect: McpDialect,
}

/// The root a surface suffix hangs off — resolved like `registry.rs`: the env override when set
/// (trimmed, non-empty), else the home-relative default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRoot {
    /// The user's home dir.
    Home,
    /// `$XDG_CONFIG_HOME`, else `home/.config`.
    Config,
    /// `$CLAUDE_CONFIG_DIR`, else `home/.claude`.
    ClaudeHome,
    /// `$CODEX_HOME`, else `home/.codex`.
    CodexHome,
    /// `$HERMES_HOME`, else `home/.hermes`.
    HermesHome,
}

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

/// The six MCP-capable harnesses. Row facts are the verified ground truth; keep the table in
/// registry-slug spelling.
static MCP_HARNESSES: &[McpHarness] = &[
    McpHarness {
        slug: "claude-code",
        display_name: "Claude Code",
        // An OWNED plugin DIRECTORY: the `.mcp.json` under it is patched through the strict
        // JSON driver; `plugin_dir` renders the constant `.claude-plugin/plugin.json` beside it.
        user_surface: Some(McpSurface {
            root: SurfaceRoot::ClaudeHome,
            suffix: "skills/topos-mcp",
            dialect: McpDialect::ClaudePluginDir,
        }),
        project_surface: Some((".mcp.json", McpDialect::ClaudeProjectJson)),
        reload_note: "loads next session; /reload-plugins reloads live; sign in with /mcp",
    },
    McpHarness {
        slug: "codex",
        display_name: "Codex",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::CodexHome,
            suffix: "config.toml",
            dialect: McpDialect::CodexToml,
        }),
        // Codex honors a project config only once the user trusts the repo in Codex.
        project_surface: Some((".codex/config.toml", McpDialect::CodexToml)),
        reload_note: "restart codex; sign in with `codex mcp login <name>`",
    },
    McpHarness {
        slug: "cursor",
        display_name: "Cursor",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".cursor/mcp.json",
            dialect: McpDialect::CursorJson,
        }),
        project_surface: Some((".cursor/mcp.json", McpDialect::CursorJson)),
        reload_note: "restart Cursor",
    },
    McpHarness {
        slug: "opencode",
        display_name: "OpenCode",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Config,
            suffix: "opencode/opencode.json",
            dialect: McpDialect::OpencodeJson,
        }),
        // The project config sits at the checkout root, not under a dot-dir.
        project_surface: Some(("opencode.json", McpDialect::OpencodeJson)),
        // Automatic on 401 + dynamic client registration.
        reload_note: "restart opencode",
    },
    McpHarness {
        slug: "openclaw",
        display_name: "OpenClaw",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".openclaw/openclaw.json",
            dialect: McpDialect::OpenclawJson,
        }),
        project_surface: None,
        reload_note: "picked up automatically; sign in with `openclaw mcp login <name>`",
    },
    McpHarness {
        slug: "hermes-agent",
        display_name: "Hermes Agent",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::HermesHome,
            suffix: "config.yaml",
            dialect: McpDialect::HermesYaml,
        }),
        project_surface: None,
        reload_note: "/reload-mcp in a session, or next session",
    },
];

/// Every MCP-capable harness row.
#[must_use]
pub fn mcp_harnesses() -> &'static [McpHarness] {
    MCP_HARNESSES
}

/// The row for a registry slug, or `None` when the harness has no MCP support here.
#[must_use]
pub fn mcp_harness(slug: &str) -> Option<&'static McpHarness> {
    MCP_HARNESSES.iter().find(|h| h.slug == slug)
}

/// The harness's user-scope config path resolved against `home`, honoring the root's env
/// override exactly like `registry.rs` (trimmed, non-empty). `None` when the harness has no user
/// surface.
#[must_use]
pub fn user_surface_path(h: &McpHarness, home: &Path) -> Option<PathBuf> {
    let surface = h.user_surface.as_ref()?;
    let base = match surface.root {
        SurfaceRoot::Home => home.to_path_buf(),
        SurfaceRoot::Config => resolve_config_home(home),
        SurfaceRoot::ClaudeHome => {
            env_override("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"))
        }
        SurfaceRoot::CodexHome => env_override("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
        SurfaceRoot::HermesHome => {
            env_override("HERMES_HOME").unwrap_or_else(|| home.join(".hermes"))
        }
    };
    let mut path = base;
    for component in surface.suffix.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_rows_and_every_slug_is_a_registry_slug() {
        let all = mcp_harnesses();
        assert_eq!(all.len(), 6);
        let registry_slugs: Vec<&str> = crate::registry::known_harnesses()
            .iter()
            .map(|h| h.slug)
            .collect();
        for h in all {
            assert!(
                registry_slugs.contains(&h.slug),
                "{} must be a registry slug",
                h.slug
            );
            assert!(!h.reload_note.is_empty(), "{}", h.slug);
        }
        let slugs: Vec<&str> = all.iter().map(|h| h.slug).collect();
        assert_eq!(
            slugs,
            [
                "claude-code",
                "codex",
                "cursor",
                "opencode",
                "openclaw",
                "hermes-agent"
            ]
        );
    }

    #[test]
    fn surfaces_are_as_specified() {
        let get = |slug: &str| mcp_harness(slug).unwrap();

        let cc = get("claude-code");
        let s = cc.user_surface.unwrap();
        assert_eq!(s.root, SurfaceRoot::ClaudeHome);
        assert_eq!(s.suffix, "skills/topos-mcp");
        assert_eq!(s.dialect, McpDialect::ClaudePluginDir);
        assert_eq!(
            cc.project_surface,
            Some((".mcp.json", McpDialect::ClaudeProjectJson))
        );

        let codex = get("codex");
        let s = codex.user_surface.unwrap();
        assert_eq!((s.root, s.suffix), (SurfaceRoot::CodexHome, "config.toml"));
        assert_eq!(s.dialect, McpDialect::CodexToml);
        assert_eq!(
            codex.project_surface,
            Some((".codex/config.toml", McpDialect::CodexToml))
        );

        let cursor = get("cursor");
        let s = cursor.user_surface.unwrap();
        assert_eq!((s.root, s.suffix), (SurfaceRoot::Home, ".cursor/mcp.json"));
        assert_eq!(s.dialect, McpDialect::CursorJson);
        assert_eq!(
            cursor.project_surface,
            Some((".cursor/mcp.json", McpDialect::CursorJson))
        );

        let oc = get("opencode");
        let s = oc.user_surface.unwrap();
        assert_eq!(
            (s.root, s.suffix),
            (SurfaceRoot::Config, "opencode/opencode.json")
        );
        assert_eq!(s.dialect, McpDialect::OpencodeJson);
        assert_eq!(
            oc.project_surface,
            Some(("opencode.json", McpDialect::OpencodeJson))
        );

        let claw = get("openclaw");
        let s = claw.user_surface.unwrap();
        assert_eq!(
            (s.root, s.suffix),
            (SurfaceRoot::Home, ".openclaw/openclaw.json")
        );
        assert_eq!(s.dialect, McpDialect::OpenclawJson);
        assert_eq!(claw.project_surface, None);

        let hermes = get("hermes-agent");
        let s = hermes.user_surface.unwrap();
        assert_eq!((s.root, s.suffix), (SurfaceRoot::HermesHome, "config.yaml"));
        assert_eq!(s.dialect, McpDialect::HermesYaml);
        assert_eq!(hermes.project_surface, None);
    }

    #[test]
    fn user_surface_paths_resolve_under_home() {
        let home = Path::new("/test-home");
        // Home-rooted rows resolve deterministically (no env override to perturb the test).
        assert_eq!(
            user_surface_path(mcp_harness("cursor").unwrap(), home),
            Some(PathBuf::from("/test-home/.cursor/mcp.json"))
        );
        assert_eq!(
            user_surface_path(mcp_harness("openclaw").unwrap(), home),
            Some(PathBuf::from("/test-home/.openclaw/openclaw.json"))
        );
        // Env-rooted rows resolve to the developer's REAL override when one is set (a test can't
        // unset env vars here), so assert the invariant part: the suffix.
        let cc = user_surface_path(mcp_harness("claude-code").unwrap(), home).unwrap();
        assert!(cc.ends_with("skills/topos-mcp"), "{cc:?}");
        let codex = user_surface_path(mcp_harness("codex").unwrap(), home).unwrap();
        assert!(codex.ends_with("config.toml"), "{codex:?}");
        let hermes = user_surface_path(mcp_harness("hermes-agent").unwrap(), home).unwrap();
        assert!(hermes.ends_with("config.yaml"), "{hermes:?}");
        let oc = user_surface_path(mcp_harness("opencode").unwrap(), home).unwrap();
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
