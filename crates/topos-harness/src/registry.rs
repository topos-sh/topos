//! The **baked harness registry** — ONE row per agent harness, carrying everything this crate knows
//! about it: the on-disk skill-directory conventions ported from `vercel-labs/skills` (so `topos` can
//! discover *untracked* skills across the whole ecosystem), the MCP-server config surfaces, and the
//! shared-dir coverage claim. Every consumer joins on this table; supporting a new capability for a
//! harness is filling a column, not adding a table.
//!
//! The queries over the real filesystem are read-only: [`discover_all`] (what skills are on this
//! machine), [`attribute_path`] (which harness owns a given skill dir, for `add`-time attribution), and
//! [`detected_harnesses`] (which agents are installed here). Skill *directories* are read only to
//! confirm a root `SKILL.md` exists — never the bytes, never the frontmatter — through the one shared
//! [`discover_skill_dirs`] probe every adapter's `discover()` also runs.
//!
//! A harness's richer behavior stays its adapter's (Hermes's `<category>/<name>` nesting composes the
//! shared probe's two halves rather than restating them; Claude Code's config edit is its own). Only
//! `claude-code`, `openclaw`, and `hermes-agent` carry [`KnownHarness::adapter_supported`] `= true`
//! (topos can *place + follow* those); every other row is discover-and-`add` only.
//!
//! Per-harness home overrides (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, `$HERMES_HOME`, `$VIBE_HOME`,
//! `$AUTOHAND_HOME`, `$GROK_HOME`, `$XDG_CONFIG_HOME`, plus `$APPDATA` / `$FLATPAK_XDG_CONFIG_HOME` for
//! Zed) are read
//! from the real environment; where an override is unset the directory resolves relative to the passed
//! `home`, so the whole surface is testable against a temp dir without touching a developer's real config.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::coverage::SharedDirSupport;
use crate::mcp::descriptor::McpDialect;

/// The on-disk scope a skill was discovered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// A user/global skills dir (e.g. `$HOME/.claude/skills`).
    User,
    /// A project/cwd-relative skills dir (e.g. `<project>/.agents/skills`).
    Project,
}

/// One known harness's directory conventions (a baked-table row).
#[derive(Debug)]
pub struct KnownHarness {
    /// The stable machine slug, exactly as `vercel-labs/skills` names it (e.g. `claude-code`, `cursor`).
    pub slug: &'static str,
    /// The human-facing name (e.g. `Claude Code`, `Cursor`).
    pub display_name: &'static str,
    /// `true` ONLY for the three harnesses topos ships a full [`HarnessAdapter`](crate::HarnessAdapter)
    /// for — `claude-code`, `openclaw`, `hermes-agent` — meaning topos can place + follow their skills.
    /// Everything else is discover-and-`add` only.
    pub adapter_supported: bool,
    /// The user/global skills dir(s) — resolved via [`resolve_spec`]. Usually one; `openclaw` has three
    /// (`.openclaw` / `.clawdbot` / `.moltbot`); the two cwd-only harnesses (`eve`, `promptscript`) have
    /// none (no global scope).
    user_dirs: &'static [DirSpec],
    /// The project/cwd-relative skills dir (a `/`-separated path joined onto the passed `cwd`).
    project_dir: &'static str,
    /// The "is this harness installed" probes — a config dir (usually under `$HOME`) whose existence marks
    /// the harness present. Any one existing ⇒ present. Empty ⇒ never independently present (the sentinel
    /// `universal` row, whose dirs are covered by the concrete harness that shares them).
    detect_dirs: &'static [DirSpec],
    /// Where this harness reads MCP-server config, in which dialect, and how a change gets picked up —
    /// `None` for a harness with no MCP surface (the great majority).
    mcp: Option<McpSurfaces>,
    /// Whether this harness reads the shared `~/.agents/skills` dir, WITH the claim's provenance.
    /// [`SharedDirSupport::Unknown`] (the default) means the row carries no claim of its own, and
    /// [`crate::coverage::shared_dir_support`] falls through to its derivation from the user dirs below.
    shared_dir: SharedDirSupport,
}

/// One harness's MCP-server config surfaces — the column joining the placement engine's detection probe
/// to the config file it must edit.
#[derive(Debug)]
pub struct McpSurfaces {
    /// The user/global config surface, if the harness has one.
    pub user: Option<McpSurface>,
    /// Project-relative config path + dialect (`None` = no project surface).
    pub project: Option<(&'static str, McpDialect)>,
    /// Receipt copy: how config changes get picked up.
    pub reload_note: &'static str,
}

/// A user-scope MCP config surface: where the file lives + the dialect it speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpSurface {
    /// The file's location, resolved like every other dir in this table ([`resolve_spec`]).
    pub dir: DirSpec,
    pub dialect: McpDialect,
}

/// One discovered skill directory in a known harness.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    /// The skill dir (contains a root `SKILL.md`).
    pub path: PathBuf,
    /// The owning harness's [`KnownHarness::slug`].
    pub harness_slug: String,
    /// The owning harness's [`KnownHarness::display_name`].
    pub harness_name: String,
    /// The owning harness's [`KnownHarness::adapter_supported`].
    pub adapter_supported: bool,
    /// Whether the skill sits in a user- or project-scope dir.
    pub scope: SkillScope,
}

/// Which known harness owns a path, for `add`-time attribution (see [`attribute_path`]).
#[derive(Debug, Clone)]
pub struct HarnessAttribution {
    /// The owning harness's [`KnownHarness::slug`].
    pub slug: String,
    /// The owning harness's [`KnownHarness::display_name`].
    pub name: String,
    /// The owning harness's [`KnownHarness::adapter_supported`].
    pub adapter_supported: bool,
    /// Whether the path sits under a user- or project-scope dir.
    pub scope: SkillScope,
}

/// The root a [`DirSpec`]'s suffix hangs off — how a per-harness env override resolves (each falls back to
/// a `home`-relative default when its variable is unset, so the whole table is testable against a temp
/// home).
///
/// This is the crate's ONE root vocabulary: every skills dir, detection probe, MCP config surface, and
/// trigger config root names its root here and resolves through [`resolve_root`], so adding a root — or
/// changing what an override falls back to — is a one-place edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// The passed home dir.
    Home,
    /// `$XDG_CONFIG_HOME`, else `home/.config`.
    Config,
    /// `$CODEX_HOME`, else `home/.codex`.
    CodexHome,
    /// `$CLAUDE_CONFIG_DIR`, else `home/.claude`.
    ClaudeHome,
    /// `$VIBE_HOME`, else `home/.vibe`.
    VibeHome,
    /// `$HERMES_HOME`, else `home/.hermes`.
    HermesHome,
    /// `$AUTOHAND_HOME`, else `home/.autohand`.
    AutohandHome,
    /// `$GROK_HOME`, else `home/.grok`.
    GrokHome,
    /// The project dir (`None` when no `cwd` is supplied).
    Cwd,
    /// An absolute path (the suffix is joined onto the filesystem root — e.g. `/etc/codex`).
    Abs,
    /// `$APPDATA` (Windows; `None` when unset) — Zed's alternate config home.
    Appdata,
    /// `$FLATPAK_XDG_CONFIG_HOME` (`None` when unset) — Zed's Flatpak config home.
    FlatpakConfig,
}

/// A directory location = a resolution [`Root`] + a `/`-separated suffix under it (empty suffix ⇒ the root
/// itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirSpec {
    root: Root,
    suffix: &'static str,
}

impl Root {
    /// The tag naming this root's upstream resolution variable (`home`, `configHome`, `codexHome`, …) —
    /// the encoding [`DirSpec::raw`] renders and an out-of-band checker compares. An absolute root is the
    /// empty tag (it renders `/`-rooted).
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Root::Home => "home",
            Root::Config => "configHome",
            Root::CodexHome => "codexHome",
            Root::ClaudeHome => "claudeHome",
            Root::VibeHome => "vibeHome",
            Root::HermesHome => "hermesHome",
            Root::AutohandHome => "autohandHome",
            Root::GrokHome => "grokHome",
            Root::Cwd => "cwd",
            Root::Abs => "",
            Root::Appdata => "APPDATA",
            Root::FlatpakConfig => "FLATPAK_XDG_CONFIG_HOME",
        }
    }

    /// The `~/`-spelled base this root resolves to with NO env override — the spelling a manifest `dest`
    /// entry and the generated agent tables name a machine path by. `None` for a root that has no
    /// machine-file spelling at all: the project dir, and the two Windows/Flatpak roots that exist only
    /// as an environment variable.
    #[must_use]
    pub fn default_spelling(self) -> Option<&'static str> {
        Some(match self {
            Root::Home => "~",
            Root::Config => "~/.config",
            Root::CodexHome => "~/.codex",
            Root::ClaudeHome => "~/.claude",
            Root::VibeHome => "~/.vibe",
            Root::HermesHome => "~/.hermes",
            Root::AutohandHome => "~/.autohand",
            Root::GrokHome => "~/.grok",
            Root::Abs => "",
            Root::Cwd | Root::Appdata | Root::FlatpakConfig => return None,
        })
    }
}

impl DirSpec {
    /// The resolution root this location hangs off.
    #[must_use]
    pub fn root(self) -> Root {
        self.root
    }

    /// The `/`-separated suffix under [`Self::root`] (empty ⇒ the root itself).
    #[must_use]
    pub fn suffix(self) -> &'static str {
        self.suffix
    }

    /// This location as a canonical RAW spec string — [`Root::tag`] followed by the `/`-joined suffix (an
    /// absolute root renders `/`-rooted). This is the shape upstream's `join(<root>, …)` expressions
    /// reduce to, so an out-of-band checker can compare the two tables' dir strings without resolving
    /// anything against a real home.
    #[must_use]
    pub fn raw(self) -> String {
        let tag = self.root.tag();
        match (tag.is_empty(), self.suffix.is_empty()) {
            (true, true) => "/".to_owned(),
            (true, false) => format!("/{}", self.suffix),
            (false, true) => tag.to_owned(),
            (false, false) => format!("{tag}/{}", self.suffix),
        }
    }

    /// This location's DEFAULT `~/`-spelled machine path — no env resolution. `None` when the root has no
    /// machine-file spelling ([`Root::default_spelling`]).
    #[must_use]
    pub fn default_spelling(self) -> Option<String> {
        let base = self.root.default_spelling()?;
        Some(match (base.is_empty(), self.suffix.is_empty()) {
            (true, true) => "/".to_owned(),
            (true, false) => format!("/{}", self.suffix),
            (false, true) => base.to_owned(),
            (false, false) => format!("{base}/{}", self.suffix),
        })
    }
}

// Terse const-fn constructors so the baked table below stays a readable one-line-per-harness block.
const fn home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::Home,
        suffix,
    }
}
const fn cfg(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::Config,
        suffix,
    }
}
const fn codex_home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::CodexHome,
        suffix,
    }
}
const fn claude_home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::ClaudeHome,
        suffix,
    }
}
const fn vibe_home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::VibeHome,
        suffix,
    }
}
const fn hermes_home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::HermesHome,
        suffix,
    }
}
const fn autohand_home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::AutohandHome,
        suffix,
    }
}
const fn grok_home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::GrokHome,
        suffix,
    }
}
const fn cwd(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::Cwd,
        suffix,
    }
}
const fn abs(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::Abs,
        suffix,
    }
}
const fn appdata(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::Appdata,
        suffix,
    }
}
const fn flatpak(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::FlatpakConfig,
        suffix,
    }
}

const fn kh(
    slug: &'static str,
    display_name: &'static str,
    adapter_supported: bool,
    user_dirs: &'static [DirSpec],
    project_dir: &'static str,
    detect_dirs: &'static [DirSpec],
) -> KnownHarness {
    KnownHarness {
        slug,
        display_name,
        adapter_supported,
        user_dirs,
        project_dir,
        detect_dirs,
        mcp: None,
        shared_dir: SharedDirSupport::Unknown,
    }
}

impl KnownHarness {
    /// Column setter: this harness's MCP config surfaces (see [`McpSurfaces`]). Chained onto [`kh`] in
    /// the table below so a row without MCP support stays a one-liner.
    const fn with_mcp(mut self, mcp: McpSurfaces) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Column setter: this harness's shared-dir coverage claim, with the evidence that backs it. Each
    /// call site carries the evidence in a comment — a fresh probe result is a one-line edit.
    const fn with_shared_dir(mut self, support: SharedDirSupport) -> Self {
        self.shared_dir = support;
        self
    }
}

/// The full baked registry — ported one-to-one from `vercel-labs/skills`'s `src/agents.ts`. Directory
/// strings are preserved verbatim (only re-expressed as `Root` + suffix so env overrides resolve). Rows
/// are kept in that file's order, which fixes the "first matching harness" tie-break for a shared dir.
static HARNESSES: &[KnownHarness] = &[
    kh(
        "aider-desk",
        "AiderDesk",
        false,
        &[home(".aider-desk/skills")],
        ".aider-desk/skills",
        &[home(".aider-desk")],
    ),
    kh(
        "amp",
        "Amp",
        false,
        &[cfg("agents/skills")],
        ".agents/skills",
        &[cfg("amp")],
    )
    // Vendor manual lists `~/.agents/skills` among its skills dirs (closed source — docs only).
    .with_shared_dir(SharedDirSupport::Docs(true)),
    kh(
        "antigravity",
        "Antigravity",
        false,
        &[home(".gemini/antigravity/skills")],
        ".agents/skills",
        &[home(".gemini/antigravity")],
    ),
    kh(
        "antigravity-cli",
        "Antigravity CLI",
        false,
        &[home(".gemini/antigravity-cli/skills")],
        ".agents/skills",
        &[home(".gemini/antigravity-cli")],
    ),
    kh(
        "astrbot",
        "AstrBot",
        false,
        &[home(".astrbot/data/skills")],
        "data/skills",
        &[cwd("data/skills"), home(".astrbot")],
    ),
    kh(
        "autohand-code",
        "Autohand Code CLI",
        false,
        &[autohand_home("skills")],
        ".autohand/skills",
        &[autohand_home("")],
    ),
    kh(
        "augment",
        "Augment",
        false,
        &[home(".augment/skills")],
        ".augment/skills",
        &[home(".augment")],
    ),
    kh(
        "bob",
        "IBM Bob",
        false,
        &[home(".bob/skills")],
        ".bob/skills",
        &[home(".bob")],
    ),
    kh(
        "claude-code",
        "Claude Code",
        true,
        &[claude_home("skills")],
        ".claude/skills",
        &[claude_home("")],
    )
    .with_mcp(McpSurfaces {
        // An OWNED plugin DIRECTORY: the `.mcp.json` under it is patched through the strict JSON
        // driver; `plugin_dir` renders the constant `.claude-plugin/plugin.json` beside it.
        user: Some(McpSurface {
            dir: claude_home("skills/topos-mcp"),
            dialect: McpDialect::ClaudePluginDir,
        }),
        project: Some((".mcp.json", McpDialect::ClaudeProjectJson)),
        reload_note: "loads next session; /reload-plugins reloads live; sign in with /mcp",
    }),
    kh(
        "openclaw",
        "OpenClaw",
        true,
        &[
            home(".openclaw/skills"),
            home(".clawdbot/skills"),
            home(".moltbot/skills"),
        ],
        "skills",
        &[home(".openclaw"), home(".clawdbot"), home(".moltbot")],
    )
    .with_mcp(McpSurfaces {
        user: Some(McpSurface {
            dir: home(".openclaw/openclaw.json"),
            dialect: McpDialect::OpenclawJson,
        }),
        project: None,
        reload_note: "picked up automatically; sign in with `openclaw mcp login <name>`",
    })
    // Verified against a live containerized install of openclaw@2026.7.1 on 2026-07-16:
    // `~/.agents/skills` is a recognized skills root, higher precedence than `~/.openclaw/skills`.
    .with_shared_dir(SharedDirSupport::Probed(true)),
    kh(
        "cline",
        "Cline",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[home(".cline")],
    )
    // Verified against cline 3.0.43 source on 2026-07-16: `~/.agents/skills` is in the global
    // skills search paths (upgrades the registry-derived docs-level claim).
    .with_shared_dir(SharedDirSupport::Probed(true)),
    kh(
        "codearts-agent",
        "CodeArts Agent",
        false,
        &[home(".codeartsdoer/skills")],
        ".codeartsdoer/skills",
        &[home(".codeartsdoer")],
    ),
    kh(
        "codebuddy",
        "CodeBuddy",
        false,
        &[home(".codebuddy/skills")],
        ".codebuddy/skills",
        &[cwd(".codebuddy"), home(".codebuddy")],
    ),
    kh(
        "codemaker",
        "Codemaker",
        false,
        &[home(".codemaker/skills")],
        ".codemaker/skills",
        &[home(".codemaker")],
    ),
    kh(
        "codestudio",
        "Code Studio",
        false,
        &[home(".codestudio/skills")],
        ".codestudio/skills",
        &[home(".codestudio")],
    ),
    kh(
        "codex",
        "Codex",
        false,
        &[codex_home("skills")],
        ".agents/skills",
        &[codex_home(""), abs("etc/codex")],
    )
    .with_mcp(McpSurfaces {
        user: Some(McpSurface {
            dir: codex_home("config.toml"),
            dialect: McpDialect::CodexToml,
        }),
        // Codex honors a project config only once the user trusts the repo in Codex.
        project: Some((".codex/config.toml", McpDialect::CodexToml)),
        reload_note: "restart codex; sign in with `codex mcp login <name>`",
    })
    // Verified against a live codex-cli 0.144.4 binary on 2026-07-16: no `.agents/skills` path
    // literal exists in the build; its only user skills root is `$CODEX_HOME/skills`.
    .with_shared_dir(SharedDirSupport::Probed(false)),
    kh(
        "command-code",
        "Command Code",
        false,
        &[home(".commandcode/skills")],
        ".commandcode/skills",
        &[home(".commandcode")],
    ),
    kh(
        "continue",
        "Continue",
        false,
        &[home(".continue/skills")],
        ".continue/skills",
        &[cwd(".continue"), home(".continue")],
    ),
    kh(
        "cortex",
        "Cortex Code",
        false,
        &[home(".snowflake/cortex/skills")],
        ".cortex/skills",
        &[home(".snowflake/cortex")],
    ),
    kh(
        "crush",
        "Crush",
        false,
        &[home(".config/crush/skills")],
        ".crush/skills",
        &[home(".config/crush")],
    )
    // Verified against crush v0.85.0 source on 2026-07-16: `~/.agents/skills` is in the global
    // skills dirs ("Per the Agent Skills spec"); its own user dir alone would derive nothing.
    .with_shared_dir(SharedDirSupport::Probed(true)),
    kh(
        "cursor",
        "Cursor",
        false,
        &[home(".cursor/skills")],
        ".agents/skills",
        &[home(".cursor")],
    )
    .with_mcp(McpSurfaces {
        user: Some(McpSurface {
            dir: home(".cursor/mcp.json"),
            dialect: McpDialect::CursorJson,
        }),
        project: Some((".cursor/mcp.json", McpDialect::CursorJson)),
        reload_note: "restart Cursor",
    }),
    kh(
        "deepagents",
        "Deep Agents",
        false,
        &[home(".deepagents/agent/skills")],
        ".agents/skills",
        &[home(".deepagents")],
    ),
    kh(
        "devin",
        "Devin for Terminal",
        false,
        &[cfg("devin/skills")],
        ".devin/skills",
        &[cfg("devin")],
    ),
    kh(
        "dexto",
        "Dexto",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[home(".dexto")],
    ),
    kh(
        "droid",
        "Droid",
        false,
        &[home(".factory/skills")],
        ".factory/skills",
        &[home(".factory")],
    ),
    kh("eve", "Eve", false, &[], "agent/skills", &[cwd("agent")]),
    kh(
        "firebender",
        "Firebender",
        false,
        &[home(".firebender/skills")],
        ".agents/skills",
        &[home(".firebender")],
    ),
    kh(
        "forgecode",
        "ForgeCode",
        false,
        &[home(".forge/skills")],
        ".forge/skills",
        &[home(".forge")],
    ),
    kh(
        "gemini-cli",
        "Gemini CLI",
        false,
        &[home(".gemini/skills")],
        ".agents/skills",
        &[home(".gemini")],
    )
    // Vendor docs list `~/.agents/skills`.
    .with_shared_dir(SharedDirSupport::Docs(true)),
    kh(
        "github-copilot",
        "GitHub Copilot",
        false,
        &[home(".copilot/skills")],
        ".agents/skills",
        &[home(".copilot")],
    )
    // Vendor docs list `~/.agents/skills`.
    .with_shared_dir(SharedDirSupport::Docs(true)),
    kh(
        "goose",
        "Goose",
        false,
        &[cfg("goose/skills")],
        ".goose/skills",
        &[cfg("goose")],
    )
    // Verified LIVE against goose 1.43.0 in a container on 2026-07-16: a skill placed at
    // `~/.agents/skills/<name>` appears in `goose skills list` (and `~/.agents/skills` is the
    // build's writable global skills dir).
    .with_shared_dir(SharedDirSupport::Probed(true)),
    kh(
        "grok",
        "Grok Build",
        false,
        &[grok_home("skills")],
        ".grok/skills",
        &[grok_home("")],
    ),
    kh(
        "hermes-agent",
        "Hermes Agent",
        true,
        &[hermes_home("skills")],
        ".hermes/skills",
        &[hermes_home("")],
    )
    .with_mcp(McpSurfaces {
        user: Some(McpSurface {
            dir: hermes_home("config.yaml"),
            dialect: McpDialect::HermesYaml,
        }),
        project: None,
        reload_note: "/reload-mcp in a session, or next session",
    }),
    kh(
        "inference-sh",
        "inference.sh",
        false,
        &[home(".inferencesh/skills")],
        ".inferencesh/skills",
        &[home(".inferencesh")],
    ),
    kh(
        "jazz",
        "Jazz",
        false,
        &[home(".jazz/skills")],
        ".jazz/skills",
        &[home(".jazz"), cwd(".jazz")],
    ),
    kh(
        "junie",
        "Junie",
        false,
        &[home(".junie/skills")],
        ".junie/skills",
        &[home(".junie")],
    ),
    kh(
        "iflow-cli",
        "iFlow CLI",
        false,
        &[home(".iflow/skills")],
        ".iflow/skills",
        &[home(".iflow")],
    ),
    kh(
        "kilo",
        "Kilo Code",
        false,
        &[home(".kilocode/skills")],
        ".kilocode/skills",
        &[home(".kilocode")],
    ),
    kh(
        "kimchi",
        "Kimchi",
        false,
        // A literal `~/.config` dir, NOT the XDG-overridable config home (`crush` is the precedent) —
        // upstream joins it onto `home`, so `$XDG_CONFIG_HOME` does not move it.
        &[home(".config/kimchi/harness/skills")],
        ".kimchi/skills",
        &[home(".config/kimchi")],
    ),
    kh(
        "kimi-code-cli",
        "Kimi Code CLI",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[home(".kimi-code"), home(".kimi")],
    ),
    kh(
        "kiro-cli",
        "Kiro CLI",
        false,
        &[home(".kiro/skills")],
        ".kiro/skills",
        &[home(".kiro")],
    ),
    kh(
        "kode",
        "Kode",
        false,
        &[home(".kode/skills")],
        ".kode/skills",
        &[home(".kode")],
    ),
    kh(
        "lingma",
        "Lingma",
        false,
        &[home(".lingma/skills")],
        ".lingma/skills",
        &[home(".lingma")],
    ),
    kh(
        "loaf",
        "Loaf",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[home(".loaf")],
    ),
    kh(
        "mcpjam",
        "MCPJam",
        false,
        &[home(".mcpjam/skills")],
        ".mcpjam/skills",
        &[home(".mcpjam")],
    ),
    kh(
        "minimax-code",
        "MiniMax Code",
        false,
        &[home(".minimax/skills")],
        ".minimax/skills",
        // Present when either the home config dir OR the macOS app bundle exists — the `.app` path is
        // joined onto the filesystem root, like `zcode`'s.
        &[home(".minimax"), abs("Applications/MiniMax Code.app")],
    ),
    kh(
        "mistral-vibe",
        "Mistral Vibe",
        false,
        &[vibe_home("skills")],
        ".vibe/skills",
        &[vibe_home("")],
    ),
    kh(
        "moxby",
        "Moxby",
        false,
        &[home(".moxby/skills")],
        ".moxby/skills",
        &[home(".moxby")],
    ),
    kh(
        "mux",
        "Mux",
        false,
        &[home(".mux/skills")],
        ".mux/skills",
        &[home(".mux")],
    ),
    kh(
        "opencode",
        "OpenCode",
        false,
        &[cfg("opencode/skills")],
        ".agents/skills",
        &[cfg("opencode")],
    )
    .with_mcp(McpSurfaces {
        user: Some(McpSurface {
            dir: cfg("opencode/opencode.json"),
            dialect: McpDialect::OpencodeJson,
        }),
        // The project config sits at the checkout root, not under a dot-dir.
        project: Some(("opencode.json", McpDialect::OpencodeJson)),
        // Automatic on 401 + dynamic client registration.
        reload_note: "restart opencode",
    })
    // Verified against a live containerized opencode-ai 1.18.3 on 2026-07-16: the binary's own
    // help text names `~/.agents/skills/<name>/SKILL.md` as an auto-loaded external skills dir.
    .with_shared_dir(SharedDirSupport::Probed(true)),
    kh(
        "openhands",
        "OpenHands",
        false,
        &[home(".openhands/skills")],
        ".openhands/skills",
        &[home(".openhands")],
    ),
    kh(
        "ona",
        "Ona",
        false,
        &[home(".ona/skills")],
        ".ona/skills",
        &[home(".ona")],
    ),
    kh(
        "pi",
        "Pi",
        false,
        &[home(".pi/agent/skills")],
        ".pi/skills",
        &[home(".pi/agent")],
    ),
    kh(
        "qoder",
        "Qoder",
        false,
        &[home(".qoder/skills")],
        ".qoder/skills",
        &[home(".qoder")],
    ),
    kh(
        "qoder-cn",
        "Qoder CN",
        false,
        &[home(".qoder-cn/skills")],
        ".qoder/skills",
        &[home(".qoder-cn")],
    ),
    kh(
        "qwen-code",
        "Qwen Code",
        false,
        &[home(".qwen/skills")],
        ".qwen/skills",
        &[home(".qwen")],
    ),
    kh(
        "replit",
        "Replit",
        false,
        &[cfg("agents/skills")],
        ".agents/skills",
        &[cwd(".replit")],
    ),
    kh(
        "reasonix",
        "Reasonix",
        false,
        &[home(".reasonix/skills")],
        ".reasonix/skills",
        &[home(".reasonix")],
    ),
    kh(
        "rovodev",
        "Rovo Dev",
        false,
        &[home(".rovodev/skills")],
        ".rovodev/skills",
        &[home(".rovodev")],
    ),
    kh(
        "roo",
        "Roo Code",
        false,
        &[home(".roo/skills")],
        ".roo/skills",
        &[home(".roo")],
    ),
    kh(
        "tabnine-cli",
        "Tabnine CLI",
        false,
        &[home(".tabnine/agent/skills")],
        ".tabnine/agent/skills",
        &[home(".tabnine")],
    ),
    kh(
        "terramind",
        "Terramind",
        false,
        &[home(".terramind/skills")],
        ".terramind/skills",
        &[home(".terramind")],
    ),
    kh(
        "tinycloud",
        "Tinycloud",
        false,
        &[home(".tinycloud/skills")],
        ".tinycloud/skills",
        &[home(".tinycloud")],
    ),
    kh(
        "trae",
        "Trae",
        false,
        &[home(".trae/skills")],
        ".trae/skills",
        &[home(".trae")],
    ),
    kh(
        "trae-cn",
        "Trae CN",
        false,
        &[home(".trae-cn/skills")],
        ".trae/skills",
        &[home(".trae-cn")],
    ),
    kh(
        "warp",
        "Warp",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[home(".warp")],
    ),
    kh(
        "windsurf",
        "Windsurf",
        false,
        &[home(".codeium/windsurf/skills")],
        ".windsurf/skills",
        &[home(".codeium/windsurf")],
    ),
    kh(
        "zed",
        "Zed",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[cfg("zed"), appdata("Zed"), flatpak("zed")],
    ),
    kh(
        "zcode",
        "ZCode",
        false,
        &[home(".zcode/skills")],
        ".zcode/skills",
        // Present when either the home config dir OR the macOS app bundle exists (the reference probes
        // both) — the `.app` path is joined onto the filesystem root, like `codex`'s `/etc/codex`.
        &[home(".zcode"), abs("Applications/ZCode.app")],
    ),
    kh(
        "zencoder",
        "Zencoder",
        false,
        &[home(".zencoder/skills")],
        ".zencoder/skills",
        &[home(".zencoder")],
    ),
    kh(
        "zenflow",
        "Zenflow",
        false,
        &[home(".zencoder/skills")],
        ".zencoder/skills",
        &[home(".zencoder")],
    ),
    kh(
        "neovate",
        "Neovate",
        false,
        &[home(".neovate/skills")],
        ".neovate/skills",
        &[home(".neovate")],
    ),
    kh(
        "pochi",
        "Pochi",
        false,
        &[home(".pochi/skills")],
        ".pochi/skills",
        &[home(".pochi")],
    ),
    kh(
        "promptscript",
        "PromptScript",
        false,
        &[],
        ".agents/skills",
        &[cwd(".promptscript"), cwd("promptscript.yaml")],
    ),
    kh(
        "adal",
        "AdaL",
        false,
        &[home(".adal/skills")],
        ".adal/skills",
        &[home(".adal")],
    ),
    kh(
        "universal",
        "Universal",
        false,
        &[cfg("agents/skills")],
        ".agents/skills",
        &[],
    ),
];

/// The full baked registry.
#[must_use]
pub fn known_harnesses() -> &'static [KnownHarness] {
    HARNESSES
}

/// The row for a harness slug, or `None` when the slug is not a known harness.
#[must_use]
pub fn known_harness(slug: &str) -> Option<&'static KnownHarness> {
    HARNESSES.iter().find(|h| h.slug == slug)
}

/// A harness row whose MCP surface is rooted at the passed home, with no skills dirs and no
/// detection probes — a TEST fixture. Production code READS rows from [`known_harnesses`] and never
/// builds one; this exists so a config-placement test can point every surface inside a temp home it
/// fully controls, whatever the developer's `$CLAUDE_CONFIG_DIR` / `$CODEX_HOME` / `$HERMES_HOME`
/// happen to say — a suite that resolved the real roots would write into a real config.
#[must_use]
pub const fn home_rooted_mcp_row(
    slug: &'static str,
    display_name: &'static str,
    user_suffix: &'static str,
    user_dialect: McpDialect,
    project: Option<(&'static str, McpDialect)>,
    reload_note: &'static str,
) -> KnownHarness {
    kh(slug, display_name, false, &[], "", &[]).with_mcp(McpSurfaces {
        user: Some(McpSurface {
            dir: home(user_suffix),
            dialect: user_dialect,
        }),
        project,
        reload_note,
    })
}

impl KnownHarness {
    /// The project/cwd-relative skills dir — a `/`-separated path, verbatim as ported from upstream's
    /// `skillsDir`.
    #[must_use]
    pub fn project_dir(&self) -> &'static str {
        self.project_dir
    }

    /// The user/global skills dir locations. Usually one; `openclaw` has three, the two cwd-only
    /// harnesses (`eve`, `promptscript`) have none. The FIRST is the harness's canonical global skills
    /// location (what [`skills_root`] writes).
    #[must_use]
    pub fn user_dirs(&self) -> &'static [DirSpec] {
        self.user_dirs
    }

    /// Each user/global skills dir as a canonical RAW spec string ([`DirSpec::raw`]) — the shape an
    /// out-of-band checker compares against the upstream table.
    #[must_use]
    pub fn user_dir_specs(&self) -> Vec<String> {
        self.user_dirs.iter().map(|s| s.raw()).collect()
    }

    /// Each "is this harness installed" detect dir as a canonical RAW spec string — same encoding as
    /// [`Self::user_dir_specs`].
    #[must_use]
    pub fn detect_dir_specs(&self) -> Vec<String> {
        self.detect_dirs.iter().map(|s| s.raw()).collect()
    }

    /// This harness's MCP-server config surfaces, or `None` when it has no MCP support.
    #[must_use]
    pub fn mcp(&self) -> Option<&McpSurfaces> {
        self.mcp.as_ref()
    }

    /// The user-scope MCP config path resolved against `home` (env overrides honored exactly as every
    /// other dir in this table). `None` when the harness has no MCP support, or no user surface.
    #[must_use]
    pub fn mcp_user_path(&self, home: &Path) -> Option<PathBuf> {
        resolve_spec(&self.mcp.as_ref()?.user?.dir, home, None)
    }

    /// This row's OWN shared-dir coverage claim ([`SharedDirSupport::Unknown`] = no claim; the query in
    /// [`crate::coverage`] then derives one from the user dirs).
    #[must_use]
    pub fn shared_dir_claim(&self) -> SharedDirSupport {
        self.shared_dir
    }
}

/// Probe every known harness present on this machine, return the skills found.
///
/// `home` = the user's home dir. `cwd` = the project dir for project-scope discovery (`None` skips
/// project scope). Per-harness env overrides are resolved internally (relative to `home` where the
/// override is unset). A harness is probed only when it looks installed (one of its detect dirs exists),
/// so a shared dir (e.g. `.agents/skills`) is attributed to the first *present* harness in table order;
/// each distinct skills dir is probed once (cross-harness path dedup is done here — the same skill is
/// never returned twice). Skips `.`-prefixed and non-UTF-8 entries, and confirms a skill by a root
/// `SKILL.md` being a regular file — the reference adapter's exact probe mechanics.
#[must_use]
pub fn discover_all(home: &Path, cwd: Option<&Path>) -> Vec<DiscoveredSkill> {
    let mut out = Vec::new();
    let mut probed: HashSet<PathBuf> = HashSet::new();
    for harness in HARNESSES {
        if !is_present(harness, home, cwd) {
            continue;
        }
        for spec in harness.user_dirs {
            // `probed` dedups the same resolved dir across harnesses, so a shared dir is walked once and
            // attributed to the first present harness in table order.
            if let Some(dir) = resolve_spec(spec, home, cwd)
                && probed.insert(dir.clone())
            {
                probe(&dir, SkillScope::User, harness, &mut out);
            }
        }
        if let Some(dir) = project_dir_of(harness, cwd)
            && probed.insert(dir.clone())
        {
            probe(&dir, SkillScope::Project, harness, &mut out);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path)); // read_dir order is OS-dependent — pin it
    out
}

/// Attribute every skill dir under `dir` (the shared [`discover_skill_dirs`] probe) to `harness`.
fn probe(dir: &Path, scope: SkillScope, harness: &KnownHarness, out: &mut Vec<DiscoveredSkill>) {
    out.extend(
        discover_skill_dirs(dir)
            .into_iter()
            .map(|path| DiscoveredSkill {
                path,
                harness_slug: harness.slug.to_owned(),
                harness_name: harness.display_name.to_owned(),
                adapter_supported: harness.adapter_supported,
                scope,
            }),
    );
}

/// Which known harness owns `path` — does `path` sit directly under a harness skills dir? For `add`-time
/// attribution. Returns the first matching harness in table order (user scope preferred over project
/// within a row), or `None` if `path` is under no known harness dir. Unlike [`discover_all`] this does NOT
/// gate on the harness looking installed — the path itself is the evidence.
#[must_use]
pub fn attribute_path(path: &Path, home: &Path, cwd: Option<&Path>) -> Option<HarnessAttribution> {
    let parent = path.parent()?;
    for harness in HARNESSES {
        for spec in harness.user_dirs {
            if resolve_spec(spec, home, cwd).as_deref() == Some(parent) {
                return Some(attribution(harness, SkillScope::User));
            }
        }
        if project_dir_of(harness, cwd).as_deref() == Some(parent) {
            return Some(attribution(harness, SkillScope::Project));
        }
    }
    None
}

/// Every known harness that looks INSTALLED on this machine — the rows whose detect dirs exist
/// (the same presence gate [`discover_all`] probes behind). The placement engine's detection
/// read: which agents are actually here, so a followed skill's bytes can reach each of them.
/// Table order is preserved (the same first-match tie-break every other query uses).
#[must_use]
pub fn detected_harnesses(home: &Path, cwd: Option<&Path>) -> Vec<&'static KnownHarness> {
    HARNESSES
        .iter()
        .filter(|h| is_present(h, home, cwd))
        .collect()
}

/// The skills directory a NEW skill for harness `slug` should land in at `scope`, resolved against
/// `home`/`cwd` — the base onto which `add`'s remote import joins `<skill_name>/`. `None` when the slug is
/// unknown, or the scope has no dir for that harness (a cwd-only harness at `User`, or `Project` with no
/// `cwd`). The `User` dir is the harness's FIRST user dir — its canonical global skills location. This is
/// the WRITE counterpart of [`discover_all`]/[`attribute_path`], so an imported skill lands exactly where
/// discovery would later find it.
#[must_use]
pub fn skills_root(
    slug: &str,
    scope: SkillScope,
    home: &Path,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let harness = known_harness(slug)?;
    match scope {
        SkillScope::User => harness
            .user_dirs
            .first()
            .and_then(|spec| resolve_spec(spec, home, cwd)),
        SkillScope::Project => project_dir_of(harness, cwd),
    }
}

// ---------------------------------------------------------------------------------------------
// Resolution + probe internals. No panics; a missing/unreadable dir is "nothing here", never an error.
// ---------------------------------------------------------------------------------------------

/// Read a `$VAR` home override — trimmed, non-empty — as a path, else `None` (mirrors the source's
/// `process.env.X?.trim() || default`). The one place in the crate the real environment is read, so
/// everything else resolves deterministically from the passed `home`.
pub(crate) fn env_override(var: &str) -> Option<PathBuf> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Resolve a [`Root`] to a concrete base dir, or `None` when it has no meaning in this call (a `Cwd` spec
/// with no `cwd`, or an unset `$APPDATA` / `$FLATPAK_XDG_CONFIG_HOME`).
///
/// The crate's ONE root resolver: every skills dir, detection probe, MCP surface, and trigger config root
/// lands here, so a per-harness override (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, `$HERMES_HOME`,
/// `$XDG_CONFIG_HOME`, …) is honored identically wherever it is read — and a test that passes its own
/// `home` never touches a developer's real config.
#[must_use]
pub fn resolve_root(root: Root, home: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    match root {
        Root::Home => Some(home.to_path_buf()),
        Root::Config => {
            Some(env_override("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")))
        }
        Root::CodexHome => Some(env_override("CODEX_HOME").unwrap_or_else(|| home.join(".codex"))),
        Root::ClaudeHome => {
            Some(env_override("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")))
        }
        Root::VibeHome => Some(env_override("VIBE_HOME").unwrap_or_else(|| home.join(".vibe"))),
        Root::HermesHome => {
            Some(env_override("HERMES_HOME").unwrap_or_else(|| home.join(".hermes")))
        }
        Root::AutohandHome => {
            Some(env_override("AUTOHAND_HOME").unwrap_or_else(|| home.join(".autohand")))
        }
        Root::GrokHome => Some(env_override("GROK_HOME").unwrap_or_else(|| home.join(".grok"))),
        Root::Cwd => cwd.map(Path::to_path_buf),
        Root::Abs => Some(PathBuf::from(std::path::MAIN_SEPARATOR_STR)),
        Root::Appdata => env_override("APPDATA"),
        Root::FlatpakConfig => env_override("FLATPAK_XDG_CONFIG_HOME"),
    }
}

/// The user's home dir as this machine reports it — `$HOME`, else the current directory. The ONE
/// read of `$HOME` in the crate: an adapter built at a composition root (which is handed no home)
/// resolves its own config root as [`config_root`] over this, so "where does this harness keep its
/// config" has one answer whether detection asked or the adapter did.
#[must_use]
pub fn real_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A harness CONFIG root resolved against `home` — "the harness's own root, its env override honored",
/// the shape every adapter needs. The three roots whose value can be absent ([`Root::Cwd`],
/// [`Root::Appdata`], [`Root::FlatpakConfig`]) never name a config root, so this answers a plain path;
/// [`resolve_root`] is the general form.
#[must_use]
pub fn config_root(root: Root, home: &Path) -> PathBuf {
    resolve_root(root, home, None)
        .expect("a harness config root never depends on a cwd or an unset variable")
}

/// Resolve a full [`DirSpec`] to a concrete path (`None` when its root has no meaning here).
#[must_use]
pub fn resolve_spec(spec: &DirSpec, home: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let base = resolve_root(spec.root, home, cwd)?;
    Some(join_rel(&base, spec.suffix))
}

/// Join a `/`-separated relative suffix onto a base, component by component (so the stored strings stay
/// platform-neutral). An empty suffix yields the base unchanged.
fn join_rel(base: &Path, suffix: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for component in suffix.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    path
}

/// The harness's project skills dir joined onto `cwd`, or `None` when no `cwd` is supplied (or the row has
/// no project dir).
fn project_dir_of(harness: &KnownHarness, cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd?;
    (!harness.project_dir.is_empty()).then(|| join_rel(cwd, harness.project_dir))
}

/// Does the harness look installed — any one of its detect dirs exists on disk? A row with no detect dirs
/// (the `universal` sentinel) is never independently present.
fn is_present(harness: &KnownHarness, home: &Path, cwd: Option<&Path>) -> bool {
    harness
        .detect_dirs
        .iter()
        .any(|spec| resolve_spec(spec, home, cwd).is_some_and(|p| p.exists()))
}

/// Every CHILD DIRECTORY of `dir` that could hold a skill, as `(name, path)` — the one directory walk
/// every discovery probe in this crate shares. Skips `.`-prefixed entries (a transient
/// `.topos-staging-*` / `.topos-old-*` dir the materializer builds beside a skill dir is never a real
/// skill, even with a `SKILL.md` inside, so a discovery during the sub-second swap window cannot surface
/// it) and non-UTF-8 names (the dir name IS the skill's invocation name, so a name we cannot spell is
/// never one we manage). Symlinks are followed — a symlinked skill dir is valid. Unordered; a missing or
/// unreadable `dir` yields nothing, never an error.
#[must_use]
pub fn child_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            out.push((name, path));
        }
    }
    out
}

/// Whether `path` IS a skill dir: its root `SKILL.md` is a regular file. The file's existence is what
/// confirms skill-ness — never the frontmatter (all-optional, and this crate never parses it), so a
/// malformed `SKILL.md` cannot mislead.
#[must_use]
pub fn is_skill_dir(path: &Path) -> bool {
    path.join("SKILL.md").is_file()
}

/// Walk ONE level under `dir`: every child directory that [`is_skill_dir`] confirms, sorted (`read_dir`
/// order is OS-dependent — pin it). THE skill-directory probe: the registry's own discovery and every
/// adapter's `discover()` run this one function, so "what counts as a skill on disk" is decided in a
/// single place. A harness with a deeper shape composes it from [`child_dirs`] + [`is_skill_dir`] rather
/// than restating the rule.
#[must_use]
pub fn discover_skill_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = child_dirs(dir)
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| is_skill_dir(path))
        .collect();
    out.sort();
    out
}

fn attribution(harness: &KnownHarness, scope: SkillScope) -> HarnessAttribution {
    HarnessAttribution {
        slug: harness.slug.to_owned(),
        name: harness.display_name.to_owned(),
        adapter_supported: harness.adapter_supported,
        scope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A self-cleaning temp tree (RAII) — a stand-in for a `home` or a project `cwd`.
    struct TempTree(PathBuf);
    impl TempTree {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-reg-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        /// `mkdir -p` a `/`-separated relative path, returning it.
        fn mkdir(&self, rel: &str) -> PathBuf {
            let p = join_rel(&self.0, rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
        /// Create a real skill dir (`<rel>/SKILL.md`), returning the skill dir.
        fn skill(&self, rel: &str) -> PathBuf {
            let d = self.mkdir(rel);
            std::fs::write(d.join("SKILL.md"), b"---\nname: x\n---\n# x\n").unwrap();
            d
        }
    }
    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn find<'a>(found: &'a [DiscoveredSkill], leaf: &str) -> Option<&'a DiscoveredSkill> {
        found
            .iter()
            .find(|d| d.path.file_name().and_then(|n| n.to_str()) == Some(leaf))
    }

    /// Keep only skills whose path is under `root`. The env-override harnesses (`claude-code` via
    /// `$CLAUDE_CONFIG_DIR`, `codex` via `$CODEX_HOME`, …) resolve to the developer's REAL config dirs and
    /// legitimately surface real skills; a test can't unset those vars (`std::env::remove_var` is unsafe,
    /// forbidden here), so it isolates its own fixtures by their temp-dir prefix instead.
    fn under<'a>(found: &'a [DiscoveredSkill], root: &Path) -> Vec<&'a DiscoveredSkill> {
        found.iter().filter(|d| d.path.starts_with(root)).collect()
    }

    #[test]
    fn skills_root_resolves_the_write_destination_or_none() {
        let home = TempTree::new("root-home");
        let cwd = TempTree::new("root-cwd");
        // Project scope joins the harness project dir onto cwd (no env override — deterministic).
        assert_eq!(
            skills_root(
                "claude-code",
                SkillScope::Project,
                home.path(),
                Some(cwd.path())
            ),
            Some(join_rel(cwd.path(), ".claude/skills"))
        );
        // User scope, a purely home-rooted harness (no `$…` override to perturb the test).
        assert_eq!(
            skills_root("cline", SkillScope::User, home.path(), None),
            Some(join_rel(home.path(), ".agents/skills"))
        );
        // A cwd-only harness (`eve`) has no user dir; Project with no cwd has none either.
        assert_eq!(
            skills_root("eve", SkillScope::User, home.path(), Some(cwd.path())),
            None
        );
        assert_eq!(
            skills_root("claude-code", SkillScope::Project, home.path(), None),
            None
        );
        // An unknown slug is never a destination.
        assert_eq!(
            skills_root("not-a-harness", SkillScope::User, home.path(), None),
            None
        );
    }

    #[test]
    fn table_shape_is_the_ported_registry() {
        let all = known_harnesses();
        assert_eq!(all.len(), 76, "every vercel-labs/skills agent is ported");

        // Slugs are unique.
        let mut slugs: Vec<&str> = all.iter().map(|h| h.slug).collect();
        slugs.sort_unstable();
        let unique = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), unique, "slugs are unique");

        // adapter_supported is true for EXACTLY the three harnesses topos ships a full adapter for.
        let supported: Vec<&str> = all
            .iter()
            .filter(|h| h.adapter_supported)
            .map(|h| h.slug)
            .collect();
        assert_eq!(supported, vec!["claude-code", "openclaw", "hermes-agent"]);

        // `zcode` sits between `zed` and `zencoder` in upstream file order — and row order fixes the
        // shared-dir tie-break, so pin the neighbours, not just the membership.
        let pos = |slug: &str| all.iter().position(|h| h.slug == slug);
        let zed = pos("zed").expect("zed present");
        let zcode = pos("zcode").expect("zcode present");
        let zencoder = pos("zencoder").expect("zencoder present");
        assert_eq!(zcode, zed + 1, "zcode follows zed");
        assert_eq!(zencoder, zcode + 1, "zcode precedes zencoder");

        // The new row's dir specs, through the raw accessors (also the accessors' own coverage): the home
        // config dir and the absolute macOS app-bundle both mark it present.
        let zc = &all[zcode];
        assert_eq!(zc.display_name, "ZCode");
        assert!(!zc.adapter_supported);
        assert_eq!(zc.project_dir(), ".zcode/skills");
        assert_eq!(zc.user_dir_specs(), vec!["home/.zcode/skills".to_owned()]);
        assert_eq!(
            zc.detect_dir_specs(),
            vec![
                "home/.zcode".to_owned(),
                "/Applications/ZCode.app".to_owned(),
            ]
        );

        // The same neighbour discipline for the three rows added after the first port: each sits exactly
        // where upstream's file puts it, so the shared-dir tie-break stays the reference table's.
        let goose = pos("goose").expect("goose present");
        let grok = pos("grok").expect("grok present");
        let hermes = pos("hermes-agent").expect("hermes-agent present");
        assert_eq!(grok, goose + 1, "grok follows goose");
        assert_eq!(hermes, grok + 1, "grok precedes hermes-agent");

        let kilo = pos("kilo").expect("kilo present");
        let kimchi = pos("kimchi").expect("kimchi present");
        let kimi = pos("kimi-code-cli").expect("kimi-code-cli present");
        assert_eq!(kimchi, kilo + 1, "kimchi follows kilo");
        assert_eq!(kimi, kimchi + 1, "kimchi precedes kimi-code-cli");

        let mcpjam = pos("mcpjam").expect("mcpjam present");
        let minimax = pos("minimax-code").expect("minimax-code present");
        let mistral = pos("mistral-vibe").expect("mistral-vibe present");
        assert_eq!(minimax, mcpjam + 1, "minimax-code follows mcpjam");
        assert_eq!(mistral, minimax + 1, "minimax-code precedes mistral-vibe");

        // `grok` carries its own env-overridable root (`$GROK_HOME`), so its specs render `grokHome`.
        let gr = &all[grok];
        assert_eq!(gr.display_name, "Grok Build");
        assert!(!gr.adapter_supported);
        assert_eq!(gr.project_dir(), ".grok/skills");
        assert_eq!(gr.user_dir_specs(), vec!["grokHome/skills".to_owned()]);
        assert_eq!(gr.detect_dir_specs(), vec!["grokHome".to_owned()]);

        // `kimchi`'s dir is a LITERAL `~/.config` path, not the XDG-overridable config home.
        let km = &all[kimchi];
        assert_eq!(km.display_name, "Kimchi");
        assert_eq!(km.project_dir(), ".kimchi/skills");
        assert_eq!(
            km.user_dir_specs(),
            vec!["home/.config/kimchi/harness/skills".to_owned()]
        );
        assert_eq!(
            km.detect_dir_specs(),
            vec!["home/.config/kimchi".to_owned()]
        );

        // `minimax-code` probes the home dir OR the macOS app bundle (a space in the suffix is fine —
        // suffixes split on `/` only).
        let mm = &all[minimax];
        assert_eq!(mm.display_name, "MiniMax Code");
        assert_eq!(mm.project_dir(), ".minimax/skills");
        assert_eq!(mm.user_dir_specs(), vec!["home/.minimax/skills".to_owned()]);
        assert_eq!(
            mm.detect_dir_specs(),
            vec![
                "home/.minimax".to_owned(),
                "/Applications/MiniMax Code.app".to_owned(),
            ]
        );
    }

    #[test]
    fn discover_all_finds_user_skills_and_skips_dotfiles_and_non_skills() {
        let home = TempTree::new("home");
        // openclaw present (adapter-supported) with a real skill + a dot-staging dir that must be skipped.
        home.mkdir(".openclaw");
        home.skill(".openclaw/skills/openclaw-skill");
        home.skill(".openclaw/skills/.staging"); // dot-prefixed → skipped even with a SKILL.md
        // augment present (not adapter-supported) with a real skill + a dir that isn't a skill.
        home.mkdir(".augment");
        home.skill(".augment/skills/augment-skill");
        home.mkdir(".augment/skills/not-a-skill"); // no SKILL.md → not a skill

        // Isolate our fixtures from any real env-override harness skills (see `under`).
        let found = discover_all(home.path(), None);
        let mine = under(&found, home.path());

        assert!(
            find(&found, ".staging").is_none(),
            "dot-staging dir is skipped (dot-prefixed entries are never skills)"
        );
        assert!(
            mine.iter()
                .all(|d| d.path.file_name().and_then(|n| n.to_str()) != Some("not-a-skill")),
            "a dir without SKILL.md is skipped"
        );

        let oc = *mine
            .iter()
            .find(|d| d.path.ends_with("openclaw-skill"))
            .expect("openclaw skill discovered");
        assert_eq!(oc.harness_slug, "openclaw");
        assert_eq!(oc.harness_name, "OpenClaw");
        assert!(oc.adapter_supported, "openclaw has a full adapter");
        assert_eq!(oc.scope, SkillScope::User);
        assert_eq!(oc.path, home.path().join(".openclaw/skills/openclaw-skill"));

        let ag = *mine
            .iter()
            .find(|d| d.path.ends_with("augment-skill"))
            .expect("augment skill discovered");
        assert_eq!(ag.harness_slug, "augment");
        assert_eq!(ag.harness_name, "Augment");
        assert!(!ag.adapter_supported, "augment is discover+add only");
        assert_eq!(ag.scope, SkillScope::User);

        assert_eq!(mine.len(), 2, "exactly the two real skills under this home");
    }

    #[test]
    fn discover_all_probes_project_scope_only_with_a_cwd() {
        // `droid` present (its `.factory` detect dir exists) and its project dir `.factory/skills` is owned
        // by no other harness, so the attribution is deterministic regardless of which env-override
        // harnesses happen to be installed on the test machine.
        let home = TempTree::new("proj-home");
        home.mkdir(".factory");
        let cwd = TempTree::new("proj-cwd");
        cwd.skill(".factory/skills/proj-skill");

        // No cwd → project scope skipped entirely (nothing from this cwd is discovered).
        let without_cwd = discover_all(home.path(), None);
        assert!(
            under(&without_cwd, cwd.path()).is_empty(),
            "project skills are not probed without a cwd"
        );

        let found = discover_all(home.path(), Some(cwd.path()));
        let ps = find(&found, "proj-skill").expect("project skill discovered");
        assert_eq!(ps.harness_slug, "droid");
        assert_eq!(ps.scope, SkillScope::Project);
        assert!(!ps.adapter_supported);
        assert_eq!(ps.path, cwd.path().join(".factory/skills/proj-skill"));
    }

    #[test]
    fn attribute_path_maps_known_dirs_and_rejects_the_rest() {
        let home = TempTree::new("attr-home");

        // A user-scope path under a uniquely-owned dir → that harness, User scope. (No dir need exist —
        // the path itself is the evidence; attribution does not gate on "installed".)
        let user = home.path().join(".augment/skills/my-skill");
        let a = attribute_path(&user, home.path(), None).expect("augment owns .augment/skills");
        assert_eq!(a.slug, "augment");
        assert_eq!(a.name, "Augment");
        assert!(!a.adapter_supported);
        assert_eq!(a.scope, SkillScope::User);

        // An adapter-supported harness attributes with the flag set.
        let oc = home.path().join(".openclaw/skills/oc");
        let a = attribute_path(&oc, home.path(), None).expect("openclaw owns .openclaw/skills");
        assert_eq!(a.slug, "openclaw");
        assert!(a.adapter_supported);

        // A project-scope path under a uniquely-owned project dir → Project scope.
        let cwd = TempTree::new("attr-cwd");
        let proj = cwd.path().join(".factory/skills/dep-skill");
        let a = attribute_path(&proj, home.path(), Some(cwd.path()))
            .expect("droid owns .factory/skills");
        assert_eq!(a.slug, "droid");
        assert_eq!(a.scope, SkillScope::Project);

        // A project path with no cwd supplied → unattributable.
        assert!(attribute_path(&proj, home.path(), None).is_none());

        // A path under no known harness dir → None.
        let stray = home.path().join("some/random/place/x");
        assert!(attribute_path(&stray, home.path(), None).is_none());
    }
}
