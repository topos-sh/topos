//! The **baked harness registry** — the on-disk skill-directory conventions for every agent harness
//! recognized by `vercel-labs/skills`, so `topos` can discover *untracked* skills across the whole
//! ecosystem (not just the three harnesses it ships a full [`HarnessAdapter`](crate::HarnessAdapter) for).
//!
//! This is the **broad, simple probe**: a static table (ported from that project's `src/agents.ts`) plus
//! two read-only queries over the real filesystem — [`discover_all`] (what skills are on this machine)
//! and [`attribute_path`] (which harness owns a given skill dir, for `add`-time attribution). It reads
//! skill *directories* only to confirm a root `SKILL.md` exists — never the bytes, never the frontmatter —
//! mirroring the reference adapter's `discover()` probe mechanics exactly.
//!
//! It deliberately does NOT reimplement any adapter's richer behavior (Hermes's `<category>/<name>`
//! nesting, Claude Code's config edit): the three built adapters keep their own `discover()`. Only
//! `claude-code`, `openclaw`, and `hermes-agent` carry [`KnownHarness::adapter_supported`] `= true`
//! (topos can *place + follow* those); every other row is discover-and-`add` only.
//!
//! Per-harness home overrides (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, `$HERMES_HOME`, `$VIBE_HOME`,
//! `$AUTOHAND_HOME`, `$XDG_CONFIG_HOME`, plus `$APPDATA` / `$FLATPAK_XDG_CONFIG_HOME` for Zed) are read
//! from the real environment; where an override is unset the directory resolves relative to the passed
//! `home`, so the whole surface is testable against a temp dir without touching a developer's real config.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
#[derive(Debug, Clone, Copy)]
enum Root {
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
#[derive(Debug, Clone, Copy)]
struct DirSpec {
    root: Root,
    suffix: &'static str,
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
    ),
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
    ),
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
    ),
    kh(
        "cline",
        "Cline",
        false,
        &[home(".agents/skills")],
        ".agents/skills",
        &[home(".cline")],
    ),
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
    ),
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
    ),
    kh(
        "cursor",
        "Cursor",
        false,
        &[home(".cursor/skills")],
        ".agents/skills",
        &[home(".cursor")],
    ),
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
    ),
    kh(
        "github-copilot",
        "GitHub Copilot",
        false,
        &[home(".copilot/skills")],
        ".agents/skills",
        &[home(".copilot")],
    ),
    kh(
        "goose",
        "Goose",
        false,
        &[cfg("goose/skills")],
        ".goose/skills",
        &[cfg("goose")],
    ),
    kh(
        "hermes-agent",
        "Hermes Agent",
        true,
        &[hermes_home("skills")],
        ".hermes/skills",
        &[hermes_home("")],
    ),
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
    ),
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

impl KnownHarness {
    /// The project/cwd-relative skills dir — a `/`-separated path, verbatim as ported from upstream's
    /// `skillsDir`.
    #[must_use]
    pub fn project_dir(&self) -> &'static str {
        self.project_dir
    }

    /// Each user/global skills dir as a canonical RAW spec string — the resolution root named by the
    /// upstream variable it maps to (`home`, `configHome`, `codexHome`, `claudeHome`, `vibeHome`,
    /// `hermesHome`, `autohandHome`, `cwd`, `APPDATA`, `FLATPAK_XDG_CONFIG_HOME`; an absolute root renders
    /// `/`-rooted) followed by the `/`-joined suffix. This is the shape upstream's `join(<root>, …)`
    /// expressions reduce to, so an out-of-band checker can compare the two tables' dir strings without
    /// resolving anything against a real home. Usually one entry; `openclaw` has three, the two cwd-only
    /// harnesses (`eve`, `promptscript`) have none.
    #[must_use]
    pub fn user_dir_specs(&self) -> Vec<String> {
        self.user_dirs.iter().map(spec_display).collect()
    }

    /// Each "is this harness installed" detect dir as a canonical RAW spec string — same encoding as
    /// [`Self::user_dir_specs`].
    #[must_use]
    pub fn detect_dir_specs(&self) -> Vec<String> {
        self.detect_dirs.iter().map(spec_display).collect()
    }
}

/// Render a [`DirSpec`] to its canonical RAW string (see [`KnownHarness::user_dir_specs`]) — the root as a
/// tag naming its upstream resolution variable (an absolute root is the empty tag → a leading `/`), joined
/// to the `/`-separated suffix.
fn spec_display(spec: &DirSpec) -> String {
    let tag = match spec.root {
        Root::Home => "home",
        Root::Config => "configHome",
        Root::CodexHome => "codexHome",
        Root::ClaudeHome => "claudeHome",
        Root::VibeHome => "vibeHome",
        Root::HermesHome => "hermesHome",
        Root::AutohandHome => "autohandHome",
        Root::Cwd => "cwd",
        Root::Abs => "", // an absolute path — renders `/`-rooted below
        Root::Appdata => "APPDATA",
        Root::FlatpakConfig => "FLATPAK_XDG_CONFIG_HOME",
    };
    match (tag.is_empty(), spec.suffix.is_empty()) {
        (true, true) => "/".to_owned(),
        (true, false) => format!("/{}", spec.suffix),
        (false, true) => tag.to_owned(),
        (false, false) => format!("{tag}/{}", spec.suffix),
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
                probe_skill_dir(&dir, SkillScope::User, harness, &mut out);
            }
        }
        if let Some(dir) = project_dir_of(harness, cwd)
            && probed.insert(dir.clone())
        {
            probe_skill_dir(&dir, SkillScope::Project, harness, &mut out);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path)); // read_dir order is OS-dependent — pin it
    out
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
    let harness = HARNESSES.iter().find(|h| h.slug == slug)?;
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
/// `process.env.X?.trim() || default`). The one place the real environment is read, so the rest resolves
/// deterministically from the passed `home`.
fn env_override(var: &str) -> Option<PathBuf> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Resolve a [`Root`] to a concrete base dir, or `None` when it has no meaning in this call (a `Cwd` spec
/// with no `cwd`, or an unset `$APPDATA` / `$FLATPAK_XDG_CONFIG_HOME`).
fn resolve_root(root: Root, home: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
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
        Root::Cwd => cwd.map(Path::to_path_buf),
        Root::Abs => Some(PathBuf::from(std::path::MAIN_SEPARATOR_STR)),
        Root::Appdata => env_override("APPDATA"),
        Root::FlatpakConfig => env_override("FLATPAK_XDG_CONFIG_HOME"),
    }
}

/// Resolve a full [`DirSpec`] to a concrete path (`None` when its root has no meaning here).
fn resolve_spec(spec: &DirSpec, home: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
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

/// Walk ONE level under `dir`: every child directory whose root `SKILL.md` is a regular file is a skill.
/// Skips `.`-prefixed entries (transient staging dirs) and non-UTF-8 names — exactly the reference
/// adapter's `discover()` probe. A missing/unreadable `dir` yields nothing, never an error.
fn probe_skill_dir(
    dir: &Path,
    scope: SkillScope,
    harness: &KnownHarness,
    out: &mut Vec<DiscoveredSkill>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // no such dir (or unreadable) → nothing discovered
    };
    for entry in entries.flatten() {
        // The dir name is the skill's invocation name, so a non-UTF-8 name can't be a skill we manage.
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // A transient `.topos-staging-*` / `.topos-old-*` dir is never a real skill, even with a
        // `SKILL.md` inside — so a discovery during the sub-second swap window can't surface it.
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // A skill is a directory (symlinks followed) whose root `SKILL.md` is a regular file. The file's
        // existence confirms skill-ness — never the frontmatter (we never parse it).
        if path.is_dir() && path.join("SKILL.md").is_file() {
            out.push(DiscoveredSkill {
                path,
                harness_slug: harness.slug.to_owned(),
                harness_name: harness.display_name.to_owned(),
                adapter_supported: harness.adapter_supported,
                scope,
            });
        }
    }
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
        assert_eq!(all.len(), 73, "every vercel-labs/skills agent is ported");

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
