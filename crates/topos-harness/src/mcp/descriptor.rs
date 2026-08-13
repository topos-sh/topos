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
    /// VS Code's `mcp.json` — the one surface whose entries do NOT sit under `mcpServers`: the
    /// top-level key is `servers`, and beside it stand `inputs` and `sandbox`, which the edit
    /// leaves alone. JSONC, because the editor that owns this file reads its own JSON with
    /// comments in it.
    VscodeJson,
    /// GitHub Copilot CLI's `mcp-config.json` (strict JSON, top-level `mcpServers`; every entry
    /// carries a `tools` allowance and spells a local server `type: "local"`).
    CopilotCliJson,
    /// LM Studio's `mcp.json` (strict JSON, top-level `mcpServers`; an address alone — no `type`
    /// key appears in any documented entry).
    LmStudioJson,
    /// Claude Desktop's `claude_desktop_config.json` (strict JSON, top-level `mcpServers`) — the
    /// app's whole configuration, not an MCP-only file, so every key outside that slot is
    /// somebody else's.
    ClaudeDesktopJson,
    /// Gemini CLI's `settings.json` (strict JSON, top-level `mcpServers`) — the CLI's whole
    /// settings file, and one whose server entries are validated CLOSED, so an invented key is a
    /// warning on every run.
    GeminiJson,
    /// Windsurf's `mcp_config.json` (strict JSON, top-level `mcpServers`; an address is spelled
    /// `serverUrl` and carries no type at all).
    WindsurfJson,
    /// Cline's `cline_mcp_settings.json` (strict JSON, top-level `mcpServers`) — `type` is
    /// camelCase `streamableHttp` here, and omitting it silently means SSE.
    ClineJson,
    /// Roo Code's `mcp_settings.json` (strict JSON, top-level `mcpServers`) — the same shape as
    /// Cline's with the type spelled `streamable-http`, and the same silent SSE if it is left out.
    RooJson,
    /// Zed's `settings.json` (JSONC — comments are the norm in it; entries under
    /// `context_servers`) — the user's ENTIRE editor configuration, so the surgical edit is not a
    /// nicety here.
    ZedJson,
}

/// **How a harness spells a REFERENCE to an environment variable** inside an entry's env values —
/// the one place a machine-local server names a value topos must never carry: the variable's NAME
/// travels in the bundle, and each machine's own environment fills it in.
///
/// It is a registry-row column rather than a branch in the renderer, because it is a fact ABOUT a
/// harness, and the table is where facts about harnesses live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvRef {
    /// `${VAR}` — the shell-shaped spelling (Claude Code, Cursor, Codex, OpenClaw).
    #[default]
    DollarBrace,
    /// `{env:VAR}` — OpenCode's own.
    BraceEnv,
    /// `${env:VAR}` — the VS Code / Copilot family's.
    DollarBraceEnv,
}

impl EnvRef {
    /// A reference to the environment variable `name`, in this harness's spelling.
    #[must_use]
    pub fn render(self, name: &str) -> String {
        match self {
            Self::DollarBrace => format!("${{{name}}}"),
            Self::BraceEnv => format!("{{env:{name}}}"),
            Self::DollarBraceEnv => format!("${{env:{name}}}"),
        }
    }
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
    fn the_mcp_capable_rows_are_the_verified_harnesses() {
        let slugs: Vec<&str> = bundled_mcp_rows().iter().map(|h| h.slug).collect();
        assert_eq!(
            slugs,
            [
                "claude-code",
                "openclaw",
                "cline",
                "codex",
                "cursor",
                "gemini-cli",
                "github-copilot",
                "hermes-agent",
                "opencode",
                "roo",
                "windsurf",
                "zed",
                "vscode",
                "lm-studio",
                "claude-desktop",
            ],
            "registry-table order"
        );
        for h in bundled_mcp_rows() {
            let mcp = h.mcp().expect("mcp row");
            assert!(!mcp.reload_note.is_empty(), "{}", h.slug);
            // A row that can be given NEITHER shape could never be given anything, so no such row
            // is worth writing — one column or the other is always true.
            assert!(mcp.remote || mcp.stdio, "{}", h.slug);
        }
        // A harness whose MCP surface this build has no grounded shape for has no row at all —
        // `continue` and `kilo` are today's, and both are ordinary skills rows.
        for slug in ["continue", "kilo", "goose"] {
            assert!(
                registry::bundled_harnesses()
                    .iter()
                    .any(|h| h.slug == slug && h.mcp().is_none()),
                "{slug}: a skills row without an MCP surface"
            );
        }
        assert!(bundled_mcp_row("not-a-harness").is_none());
        // …and the runtime view is the same filter over whatever table THIS machine resolved.
        assert!(mcp_harness("not-a-harness").is_none());
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

    /// The rows added for the popular agents, stated the same way: WHERE the file is, and which
    /// dialect answers for it. Each path is one a vendor source or vendor docs name — the reason
    /// three of them hang off the platform's own config dir rather than the XDG one.
    #[test]
    fn the_breadth_surfaces_are_as_specified() {
        let get = |slug: &str| bundled_mcp_row(slug).unwrap().mcp().unwrap();
        let user_of = |slug: &str| {
            let s = get(slug).user.unwrap();
            (s.dir.root(), s.dir.suffix(), s.dialect)
        };

        assert_eq!(
            user_of("vscode"),
            (
                Root::AppSupport,
                "Code/User/mcp.json",
                McpDialect::VscodeJson
            )
        );
        assert_eq!(
            get("vscode").project,
            Some((".vscode/mcp.json", McpDialect::VscodeJson))
        );
        assert_eq!(
            user_of("github-copilot"),
            (
                Root::Home,
                ".copilot/mcp-config.json",
                McpDialect::CopilotCliJson
            )
        );
        assert_eq!(get("github-copilot").project, None);
        assert_eq!(
            user_of("gemini-cli"),
            (Root::Home, ".gemini/settings.json", McpDialect::GeminiJson)
        );
        assert_eq!(
            get("gemini-cli").project,
            Some((".gemini/settings.json", McpDialect::GeminiJson))
        );
        // Cline's own home — shared by its editor, JetBrains and CLI clients — and NOT the VS Code
        // extension storage older builds used.
        assert_eq!(
            user_of("cline"),
            (
                Root::Home,
                ".cline/data/settings/cline_mcp_settings.json",
                McpDialect::ClineJson
            )
        );
        assert_eq!(
            user_of("roo"),
            (
                Root::AppSupport,
                "Code/User/globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json",
                McpDialect::RooJson
            )
        );
        assert_eq!(
            get("roo").project,
            Some((".roo/mcp.json", McpDialect::RooJson))
        );
        assert_eq!(
            user_of("windsurf"),
            (
                Root::Home,
                ".codeium/windsurf/mcp_config.json",
                McpDialect::WindsurfJson
            )
        );
        assert_eq!(
            user_of("zed"),
            (Root::Config, "zed/settings.json", McpDialect::ZedJson)
        );
        assert_eq!(
            user_of("lm-studio"),
            (Root::Home, ".lmstudio/mcp.json", McpDialect::LmStudioJson)
        );
        assert_eq!(
            user_of("claude-desktop"),
            (
                Root::AppSupport,
                "Claude/claude_desktop_config.json",
                McpDialect::ClaudeDesktopJson
            )
        );
        // The two rows whose OWN storage dir is the proof they are installed: topos never
        // materializes another program's storage tree on a guess.
        let detect = |slug: &str| {
            registry::bundled_harnesses()
                .iter()
                .find(|h| h.slug == slug)
                .unwrap()
                .detect_dir_specs()
        };
        assert!(
            detect("roo").contains(
                &"appSupport/Code/User/globalStorage/rooveterinaryinc.roo-cline".to_owned()
            ),
            "{:?}",
            detect("roo")
        );
        assert_eq!(detect("vscode"), vec!["appSupport/Code/User".to_owned()]);
        assert_eq!(detect("claude-desktop"), vec!["appSupport/Claude".to_owned()]);
    }

    /// The capability columns as the bundled table states them: every one of the six dials an
    /// address itself, all but Hermes run a local program, and OpenCode is the one that spells an
    /// environment reference its own way.
    #[test]
    fn the_capability_columns_say_what_each_agent_can_be_given() {
        let get = |slug: &str| bundled_mcp_row(slug).unwrap().mcp().unwrap();
        for slug in [
            "claude-code",
            "openclaw",
            "codex",
            "cursor",
            "hermes-agent",
            "opencode",
        ] {
            assert!(get(slug).remote, "{slug} dials an address itself");
        }
        for slug in ["claude-code", "openclaw", "codex", "cursor", "opencode"] {
            assert!(get(slug).stdio, "{slug} runs a local program");
        }
        assert!(
            !get("hermes-agent").stdio,
            "hermes: no evidenced program grammar, so nothing is written in one"
        );
        assert_eq!(get("opencode").env_ref, EnvRef::BraceEnv);
        for slug in ["claude-code", "openclaw", "codex", "cursor", "hermes-agent"] {
            assert_eq!(get(slug).env_ref, EnvRef::DollarBrace, "{slug}");
        }
    }

    /// The two capability columns the breadth rows are the first to state as FALSE, and the one
    /// reference spelling three of them read. Each is a documented fact about the agent, not a
    /// property of its dialect — and the dialect agrees, which is what the last loop checks.
    #[test]
    fn a_row_that_reaches_a_server_only_one_way_says_so() {
        let get = |slug: &str| bundled_mcp_row(slug).unwrap().mcp().unwrap();

        // LM Studio documents an address and only an address…
        assert!(get("lm-studio").remote);
        assert!(!get("lm-studio").stdio);
        // …and Claude Desktop a program and only a program: its remote servers are added in the
        // app, so a shared address reaches it through the bridge or not at all.
        assert!(!get("claude-desktop").remote);
        assert!(get("claude-desktop").stdio);

        for slug in ["vscode", "cline", "roo", "windsurf"] {
            assert_eq!(get(slug).env_ref, EnvRef::DollarBraceEnv, "{slug}");
        }
        for slug in ["gemini-cli", "github-copilot", "zed", "claude-desktop"] {
            assert_eq!(get(slug).env_ref, EnvRef::DollarBrace, "{slug}");
        }

        // No row promises a shape its dialect has no grammar for — the pairing the CLI's
        // capability narrowing depends on.
        for h in bundled_mcp_rows() {
            let mcp = h.mcp().expect("mcp row");
            for surface in [mcp.user.map(|u| u.dialect), mcp.project.map(|(_, d)| d)]
                .into_iter()
                .flatten()
            {
                if mcp.remote {
                    assert!(
                        super::super::dialect_expresses(surface, &EMPTY_ADDRESS),
                        "{}: dials an address its dialect cannot spell",
                        h.slug
                    );
                }
                if mcp.stdio {
                    assert!(
                        super::super::dialect_expresses(surface, &EMPTY_PROGRAM),
                        "{}: runs a program its dialect cannot spell",
                        h.slug
                    );
                }
            }
        }
    }

    static EMPTY_ADDRESS: super::super::McpTarget = super::super::McpTarget::Remote {
        url: String::new(),
        headers: Vec::new(),
    };
    static EMPTY_PROGRAM: super::super::McpTarget = super::super::McpTarget::Local {
        command: String::new(),
        args: Vec::new(),
        env: Vec::new(),
        env_ref: EnvRef::DollarBrace,
    };

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
