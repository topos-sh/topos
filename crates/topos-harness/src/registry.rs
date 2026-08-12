//! The **harness registry** — ONE row per agent harness, carrying everything this crate knows
//! about it: the on-disk skill-directory conventions ported from `vercel-labs/skills` (so `topos` can
//! discover *untracked* skills across the whole ecosystem), the MCP-server config surfaces, and the
//! shared-dir coverage claim. Every consumer joins on this table; supporting a new capability for a
//! harness is filling a column, not adding a table.
//!
//! **The table is DATA, not code.** The rows live in `crates/topos-harness/registry.toml`, which
//! this crate compiles in and [`mod@format`] parses — and which a machine may carry a newer copy of
//! under `~/.topos/harness-registry/` without waiting for a release, because an agent moving its
//! skills dir is news, not a new feature. [`known_harnesses`] resolves the levels once per process
//! and hands out the same `&'static` rows every caller always had. What a downloaded copy may say
//! is fenced hard: it can only refine slugs this binary already knows (a NEW harness still needs a
//! release — its trigger, its adapter and its id are compiled), and its dirs are re-validated. All
//! of that lives in [`mod@format`].
//!
//! **Three accessors, three questions.** [`known_harnesses`] is what this MACHINE reads today, and
//! everything acting on it — discovery, detection, placement, arming — joins there.
//! [`teardown_harnesses`] adds the bundled floor back, because a table that shrank does not
//! un-arm the hooks the shrunk-away rows were armed with. [`bundled_harnesses`] is this BUILD's
//! table alone — the repo's own tooling and this crate's shape pins, which must not answer
//! differently on a developer machine carrying an override.
//!
//! The queries over the real filesystem are read-only: [`discover_all`] (what skills are on this
//! machine), [`folder_readers`] (which INSTALLED harnesses read a given skills dir — the one
//! attribution answer both the untracked listing and `add`'s `@<slug>` resolution join on), and
//! [`detected_harnesses`] (which agents are installed here). Skill *directories* are read only to
//! confirm a root `SKILL.md` exists — never the bytes, never the frontmatter — through the one shared
//! [`discover_skill_dirs`] probe every adapter's `discover()` also runs.
//!
//! A harness's richer behavior stays its adapter's (Hermes's `<category>/<name>` nesting composes the
//! shared probe's two halves rather than restating them; Claude Code's config edit is its own). This
//! table serves discovery, attribution, the MCP config surfaces, and the shared-dir coverage claim;
//! which harnesses topos can PLACE into and ARM is decided per port elsewhere
//! ([`crate::HarnessAdapter`] impls and [`crate::triggers::adapter_for_slug`]), never by a column here.
//!
//! Per-harness home overrides (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`, `$HERMES_HOME`, `$VIBE_HOME`,
//! `$AUTOHAND_HOME`, `$GROK_HOME`, `$XDG_CONFIG_HOME`, plus `$APPDATA` / `$FLATPAK_XDG_CONFIG_HOME` for
//! Zed) are read
//! from the real environment; where an override is unset the directory resolves relative to the passed
//! `home`, so the whole surface is testable against a temp dir without touching a developer's real config.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::coverage::SharedDirSupport;
use crate::mcp::descriptor::McpDialect;

pub mod format;

pub use format::{
    MAX_REGISTRY_BYTES, Origin, ParsedRegistry, REGISTRY_ENGINE_VERSION, RegistryError,
    RegistryVersion, SCHEMA_VERSION, bundled_harnesses, bundled_version, cache_path, override_path,
    parse_registry,
};

/// The on-disk scope a skill was discovered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// A user/global skills dir (e.g. `$HOME/.claude/skills`).
    User,
    /// A project/cwd-relative skills dir (e.g. `<project>/.agents/skills`).
    Project,
}

/// One known harness's directory conventions (one `registry.toml` row, parsed and leaked).
#[derive(Debug)]
pub struct KnownHarness {
    /// The stable machine slug, exactly as `vercel-labs/skills` names it (e.g. `claude-code`, `cursor`).
    pub slug: &'static str,
    /// The human-facing name (e.g. `Claude Code`, `Cursor`).
    pub display_name: &'static str,
    /// The user/global skills dir(s) — resolved via [`resolve_spec`]. Usually one; the two cwd-only
    /// harnesses (`eve`, `promptscript`) have none (no global scope).
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
    /// Whether the skill sits in a user- or project-scope dir.
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
    /// Every root, in one place — what [`Root::from_tag`] searches and what a refusal lists. A new
    /// root is a new arm here and in [`Root::tag`]; nothing else enumerates them.
    const ALL: &'static [Root] = &[
        Root::Home,
        Root::Config,
        Root::CodexHome,
        Root::ClaudeHome,
        Root::VibeHome,
        Root::HermesHome,
        Root::AutohandHome,
        Root::GrokHome,
        Root::Cwd,
        Root::Abs,
        Root::Appdata,
        Root::FlatpakConfig,
    ];

    /// The inverse of [`Root::tag`] — the root a raw spec string names, or `None` for a tag this
    /// build does not know. The absolute root has no tag (it is spelled by a leading `/`), so it is
    /// never matched here.
    fn from_tag(tag: &str) -> Option<Root> {
        (!tag.is_empty())
            .then(|| Root::ALL.iter().copied().find(|r| r.tag() == tag))
            .flatten()
    }

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

/// The ONE `home`-rooted dir constructor still built in Rust: the test fixture below. Every
/// production row is DATA — parsed out of `registry.toml` by [`mod@format`].
const fn home(suffix: &'static str) -> DirSpec {
    DirSpec {
        root: Root::Home,
        suffix,
    }
}

const fn kh(
    slug: &'static str,
    display_name: &'static str,
    user_dirs: &'static [DirSpec],
    project_dir: &'static str,
    detect_dirs: &'static [DirSpec],
) -> KnownHarness {
    KnownHarness {
        slug,
        display_name,
        user_dirs,
        project_dir,
        detect_dirs,
        mcp: None,
        shared_dir: SharedDirSupport::Unknown,
    }
}

impl KnownHarness {
    /// Column setter: this harness's MCP config surfaces (see [`McpSurfaces`]). Chained onto [`kh`]
    /// by the test fixture below.
    const fn with_mcp(mut self, mcp: McpSurfaces) -> Self {
        self.mcp = Some(mcp);
        self
    }
}

/// THE registry, resolved ONCE per process: the local override, else a downloaded copy newer than
/// this build's, else the table compiled in from `registry.toml` — see [`mod@format`] for the fences
/// and the warnings. Resolved lazily and leaked, so every consumer keeps the `&'static` rows it
/// always had and there is exactly one table however many callers ask for it.
#[must_use]
pub fn known_harnesses() -> &'static [KnownHarness] {
    static TABLE: OnceLock<&'static [KnownHarness]> = OnceLock::new();
    TABLE.get_or_init(format::load_table)
}

/// The row for a harness slug, or `None` when the slug is not a known harness.
#[must_use]
pub fn known_harness(slug: &str) -> Option<&'static KnownHarness> {
    known_harnesses().iter().find(|h| h.slug == slug)
}

/// Every row a TEARDOWN must reach: the loaded table UNIONED with the bundled one, deduped by slug
/// (the loaded row wins wherever both define it), the loaded table's order first and the
/// bundled-only rows appended in their own.
///
/// **The asymmetry is the point.** A downloaded table may legitimately SHRINK — "the file is the
/// table", so a row it omits stops being known — and ARMING follows that decision: a dropped slug
/// is one this machine no longer arms. But the hook armed under the BUNDLED table is still in that
/// harness's config, and a teardown iterating the loaded table alone would walk straight past it:
/// `uninstall --yes` would delete the sidecar and leave the agent invoking a topos that is no
/// longer there. So every teardown-shaped path — the uninstall preview, the scrub sweep, and the
/// footprint the two share — reads THIS instead. What may still be armed is the bundled floor's
/// business; what may still be ARMED ANEW is the loaded table's.
#[must_use]
pub fn teardown_harnesses() -> Vec<&'static KnownHarness> {
    merge_for_teardown(known_harnesses(), bundled_harnesses())
}

/// [`teardown_harnesses`]'s pure core — the union, taken over stated tables so the rule is
/// testable against a table that dropped a row (which is not a shape a suite can resolve out of a
/// real `~/.topos`).
fn merge_for_teardown(
    loaded: &'static [KnownHarness],
    bundled: &'static [KnownHarness],
) -> Vec<&'static KnownHarness> {
    let mut out: Vec<&'static KnownHarness> = loaded.iter().collect();
    out.extend(
        bundled
            .iter()
            .filter(|b| !loaded.iter().any(|l| l.slug == b.slug)),
    );
    out
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
    kh(slug, display_name, &[], "", &[]).with_mcp(McpSurfaces {
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

    /// The user/global skills dir locations. Usually one; the two cwd-only harnesses (`eve`,
    /// `promptscript`) have none. The FIRST is the harness's canonical global skills location
    /// (what [`skills_root`] writes).
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

    /// Whether this harness looks INSTALLED under `home` — one of its detect dirs exists. THE
    /// presence gate [`discover_all`], [`folder_readers`] and [`detected_harnesses`] all run,
    /// exposed for the one caller that asks it of a row it did not get from
    /// [`detected_harnesses`]: a teardown sweeping the [`teardown_harnesses`] union.
    #[must_use]
    pub fn is_installed(&self, home: &Path, cwd: Option<&Path>) -> bool {
        is_present(self, home, cwd)
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
    for (dir, harness, scope) in skills_roots_owned(home, cwd) {
        probe(&dir, scope, harness, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path)); // read_dir order is OS-dependent — pin it
    out
}

/// Every skills ROOT discovery walks on this machine, deduped, in table order — the boundary a
/// caller probes for a folder [`discover_all`] cannot confirm (a skill dir whose `SKILL.md` link
/// is broken is not a skill dir, and is invisible to every listing).
#[must_use]
pub fn skills_roots(home: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    skills_roots_owned(home, cwd)
        .into_iter()
        .map(|(dir, _, _)| dir)
        .collect()
}

/// The roots with their owning row + scope — [`discover_all`]'s own iteration, shared so the walk
/// and the roots a caller probes can never diverge.
///
/// THE ONE SPELLING IS DECIDED HERE. A root is resolved once and canonicalized once: `$HOME` is
/// whatever the environment says (symlinks and all) while a `cwd` arrives already resolved by the
/// kernel, so two roots naming ONE directory used to reach a listing under two different paths.
/// Canonicalizing at the boundary makes every discovered path, every folder line, and every
/// exclusion compare in the same spelling — and lets `probed` dedup a dir reached by two names,
/// so a shared dir is still walked once and attributed to the first present harness in table order.
fn skills_roots_owned(
    home: &Path,
    cwd: Option<&Path>,
) -> Vec<(PathBuf, &'static KnownHarness, SkillScope)> {
    let mut out = Vec::new();
    let mut probed: HashSet<PathBuf> = HashSet::new();
    for harness in known_harnesses() {
        if !is_present(harness, home, cwd) {
            continue;
        }
        for spec in harness.user_dirs {
            if let Some(dir) = resolve_spec(spec, home, cwd).map(resolved_dir)
                && probed.insert(dir.clone())
            {
                out.push((dir, harness, SkillScope::User));
            }
        }
        if let Some(dir) = project_dir_of(harness, cwd).map(resolved_dir)
            && probed.insert(dir.clone())
        {
            out.push((dir, harness, SkillScope::Project));
        }
    }
    out
}

/// A resolved directory in the ONE spelling every discovery answer and folder comparison uses:
/// canonical where it exists, the path itself where it does not (nothing to resolve, nothing to
/// compare against either).
fn resolved_dir(dir: PathBuf) -> PathBuf {
    dir.canonicalize().unwrap_or(dir)
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
                scope,
            }),
    );
}

/// Every INSTALLED harness that reads the skills dir `dir` — THE attribution answer, and the one
/// every surface joins on: the untracked listing's folder line, `add`'s `<name>@<slug>` resolution,
/// and the `@<slug>`-matches-this-folder question a verb asks about a tracked placement.
///
/// A folder is read by a harness when one of that harness's resolved user dirs — or its project dir
/// under `cwd` — IS `dir`. The presence gate is [`discover_all`]'s exactly (a detect dir exists), so a
/// harness that is not installed here never claims a folder someone else's skills sit in; a shared
/// dir (`~/.agents/skills`) honestly answers with EVERY installed claimant instead of picking one by
/// table order. Rows are read in table order and returned SORTED BY SLUG, so the answer does not
/// depend on where a harness happens to sit in the table.
#[must_use]
pub fn folder_readers(dir: &Path, home: &Path, cwd: Option<&Path>) -> Vec<&'static KnownHarness> {
    // Both sides through [`resolved_dir`]: the caller's folder comes from a listing (canonical) or
    // from a tracked placement (canonical), and a row's own dirs resolve from `$HOME` — comparing
    // one spelling against the other loses every reader under a symlinked home.
    let want = resolved_dir(dir.to_path_buf());
    let mut out: Vec<&'static KnownHarness> = known_harnesses()
        .iter()
        .filter(|h| is_present(h, home, cwd))
        .filter(|h| {
            h.user_dirs
                .iter()
                .any(|spec| resolve_spec(spec, home, cwd).map(resolved_dir) == Some(want.clone()))
                || project_dir_of(h, cwd).map(resolved_dir) == Some(want.clone())
        })
        .collect();
    out.sort_unstable_by_key(|h| h.slug);
    out
}

/// [`folder_readers`] as owned slugs — the spelling every wire field and `@<slug>` token uses.
#[must_use]
pub fn folder_reader_slugs(dir: &Path, home: &Path, cwd: Option<&Path>) -> Vec<String> {
    folder_readers(dir, home, cwd)
        .into_iter()
        .map(|h| h.slug.to_owned())
        .collect()
}

/// Every known harness that looks INSTALLED on this machine — the rows whose detect dirs exist
/// (the same presence gate [`discover_all`] probes behind). The placement engine's detection
/// read: which agents are actually here, so a followed skill's bytes can reach each of them.
/// Table order is preserved (the same first-match tie-break every other query uses).
#[must_use]
pub fn detected_harnesses(home: &Path, cwd: Option<&Path>) -> Vec<&'static KnownHarness> {
    known_harnesses()
        .iter()
        .filter(|h| is_present(h, home, cwd))
        .collect()
}

/// The skills directory a NEW skill for harness `slug` should land in at `scope`, resolved against
/// `home`/`cwd` — the base onto which `add`'s remote import joins `<skill_name>/`. `None` when the slug is
/// unknown, or the scope has no dir for that harness (a cwd-only harness at `User`, or `Project` with no
/// `cwd`). The `User` dir is the harness's FIRST user dir — its canonical global skills location. This is
/// the WRITE counterpart of [`discover_all`]/[`folder_readers`], so an imported skill lands exactly where
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
            // CANONICAL from birth: the platform temp dir is itself reached through a symlink on
            // macOS (`/tmp` → `/private/tmp`), and discovery answers in canonical paths — a fixture
            // spelled the other way would compare unequal to its own discovered dirs.
            Self(dir.canonicalize().unwrap())
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

    /// **A teardown reaches a row the loaded table dropped.** A downloaded table may omit a row —
    /// that is the stated cost of "the file is the table" — and the arming sweep honors the
    /// omission. The hook that row was armed with does not un-arm itself, so the teardown set is
    /// the union: the loaded rows as they stand, plus every bundled row they no longer name.
    #[test]
    fn a_teardown_reaches_a_row_the_loaded_table_dropped() {
        // The stand-in for a downloaded table carrying ONE row: a stated table, since a suite
        // cannot resolve one out of a real `~/.topos`.
        let loaded: &'static [KnownHarness] =
            Vec::leak(vec![kh("cursor", "Cursor (moved)", &[], ".moved", &[])]);
        let union = merge_for_teardown(loaded, bundled_harnesses());

        // The loaded row leads, and it is the LOADED one — a table that moved a dir is still the
        // one that says where that harness reads.
        assert_eq!(union[0].slug, "cursor");
        assert_eq!(union[0].display_name, "Cursor (moved)");
        assert_eq!(
            union.iter().filter(|h| h.slug == "cursor").count(),
            1,
            "one row per slug — the loaded one"
        );
        // …and every bundled slug is still reachable, `codex` (an armed hook in its own two
        // config surfaces) first among them.
        assert!(
            union.iter().any(|h| h.slug == "codex"),
            "a dropped row is still swept: {:?}",
            union.iter().map(|h| h.slug).collect::<Vec<_>>()
        );
        for row in bundled_harnesses() {
            assert!(union.iter().any(|h| h.slug == row.slug), "{}", row.slug);
        }
        assert_eq!(
            union.len(),
            bundled_harnesses().len(),
            "no slug is counted twice"
        );

        // The ordinary machine: nothing local, so the union IS the table — same rows, same order.
        let plain = merge_for_teardown(known_harnesses(), bundled_harnesses());
        assert_eq!(
            plain.iter().map(|h| h.slug).collect::<Vec<_>>(),
            known_harnesses().iter().map(|h| h.slug).collect::<Vec<_>>()
        );
    }

    #[test]
    fn table_shape_is_the_ported_registry() {
        // The BUNDLED table: a shape pin answers for the commit, never for whatever
        // `~/.topos/harness-registry/` the developer running it happens to keep.
        let all = bundled_harnesses();
        assert_eq!(all.len(), 76, "every vercel-labs/skills agent is ported");

        // Slugs are unique.
        let mut slugs: Vec<&str> = all.iter().map(|h| h.slug).collect();
        slugs.sort_unstable();
        let unique = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), unique, "slugs are unique");

        // `zcode` sits between `zed` and `zencoder` in upstream file order — and row order fixes the
        // shared-dir tie-break, so pin the neighbours, not just the membership.
        let pos = |slug: &str| all.iter().position(|h| h.slug == slug);
        let zed = pos("zed").expect("zed present");
        let zcode = pos("zcode").expect("zcode present");
        let zencoder = pos("zencoder").expect("zencoder present");
        assert_eq!(zcode, zed + 1, "zcode follows zed");
        assert_eq!(zencoder, zcode + 1, "zcode precedes zencoder");

        // The new row's dir specs, through the raw accessor (also the accessor's own coverage):
        // the home config dir marks it present. The macOS app-bundle probe beside it is GONE —
        // an absolute path is refused at every fenced origin, and the served table is these bytes.
        let zc = &all[zcode];
        assert_eq!(zc.display_name, "ZCode");
        assert_eq!(zc.project_dir(), ".zcode/skills");
        assert_eq!(zc.user_dir_specs(), vec!["home/.zcode/skills".to_owned()]);
        assert_eq!(
            zc.detect_dirs.iter().map(|s| s.raw()).collect::<Vec<_>>(),
            vec!["home/.zcode".to_owned()]
        );

        // …and NO row names an absolute path, whatever the grammar allows: one would make the
        // served copy unreadable to every client (see `format`'s landability gate).
        for row in all {
            for spec in row.user_dirs().iter().chain(row.detect_dirs) {
                assert_ne!(
                    spec.root(),
                    Root::Abs,
                    "{}: a bundled row names an absolute path",
                    row.slug
                );
            }
        }

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
        assert_eq!(gr.project_dir(), ".grok/skills");
        assert_eq!(gr.user_dir_specs(), vec!["grokHome/skills".to_owned()]);

        // `kimchi`'s dir is a LITERAL `~/.config` path, not the XDG-overridable config home.
        let km = &all[kimchi];
        assert_eq!(km.display_name, "Kimchi");
        assert_eq!(km.project_dir(), ".kimchi/skills");
        assert_eq!(
            km.user_dir_specs(),
            vec!["home/.config/kimchi/harness/skills".to_owned()]
        );

        // `minimax-code` probes its home dir; the macOS app bundle beside it is gone with every
        // other absolute path.
        let mm = &all[minimax];
        assert_eq!(mm.display_name, "MiniMax Code");
        assert_eq!(mm.project_dir(), ".minimax/skills");
        assert_eq!(mm.user_dir_specs(), vec!["home/.minimax/skills".to_owned()]);
        assert_eq!(
            mm.detect_dirs.iter().map(|s| s.raw()).collect::<Vec<_>>(),
            vec!["home/.minimax".to_owned()]
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
        assert_eq!(oc.scope, SkillScope::User);
        assert_eq!(oc.path, home.path().join(".openclaw/skills/openclaw-skill"));

        let ag = *mine
            .iter()
            .find(|d| d.path.ends_with("augment-skill"))
            .expect("augment skill discovered");
        assert_eq!(ag.harness_slug, "augment");
        assert_eq!(ag.harness_name, "Augment");
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
        assert_eq!(ps.path, cwd.path().join(".factory/skills/proj-skill"));
    }

    /// The slugs of `folder_readers` — the shape every caller consumes.
    fn readers(dir: &Path, home: &Path, cwd: Option<&Path>) -> Vec<String> {
        folder_reader_slugs(dir, home, cwd)
    }

    #[test]
    fn folder_readers_names_every_installed_claimant_and_ignores_the_rest() {
        let home = TempTree::new("readers-home");

        // A folder no installed harness reads answers with nobody — the presence gate decides, not
        // table order (`cline`, `dexto`, `warp` and `zed` all claim this dir in the table).
        let shared = home.path().join(".agents/skills");
        assert!(readers(&shared, home.path(), None).is_empty());

        // Two claimants of the SHARED dir installed → BOTH, sorted by slug. `dexto` claims the same
        // folder and is not installed, so it never appears.
        home.mkdir(".warp");
        home.mkdir(".cline");
        let found = readers(&shared, home.path(), None);
        let mut sorted = found.clone();
        sorted.sort();
        assert_eq!(found, sorted, "readers come back sorted by slug");
        assert!(found.contains(&"cline".to_owned()), "{found:?}");
        assert!(found.contains(&"warp".to_owned()), "{found:?}");
        assert!(!found.contains(&"dexto".to_owned()), "{found:?}");

        // A uniquely-owned user dir answers with exactly its one installed owner.
        home.mkdir(".augment");
        assert_eq!(
            readers(&home.path().join(".augment/skills"), home.path(), None),
            vec!["augment".to_owned()]
        );

        // The PROJECT dir of an installed harness resolves under `cwd` — and is nobody's without one.
        let cwd = TempTree::new("readers-cwd");
        home.mkdir(".factory");
        let proj = cwd.path().join(".factory/skills");
        assert_eq!(
            readers(&proj, home.path(), Some(cwd.path())),
            vec!["droid".to_owned()]
        );
        assert!(readers(&proj, home.path(), None).is_empty());

        // A folder under no known harness dir is read by nobody.
        assert!(
            readers(&home.path().join("some/random/place"), home.path(), None).is_empty(),
            "a stray folder has no readers"
        );
    }

    #[test]
    fn every_discovered_folder_is_read_by_the_harness_that_discovered_it() {
        // Attribution and discovery are ONE query: whatever folder `discover_all` yields a skill
        // from, `folder_readers` of that folder names the harness discovery attributed it to —
        // never a different, absent claimant.
        let home = TempTree::new("consistency-home");
        home.mkdir(".cline");
        home.mkdir(".warp");
        home.skill(".agents/skills/shared-skill");
        home.mkdir(".augment");
        home.skill(".augment/skills/augment-skill");
        let cwd = TempTree::new("consistency-cwd");
        home.mkdir(".factory");
        cwd.skill(".factory/skills/proj-skill");

        let found = discover_all(home.path(), Some(cwd.path()));
        let mine: Vec<&DiscoveredSkill> = found
            .iter()
            .filter(|d| d.path.starts_with(home.path()) || d.path.starts_with(cwd.path()))
            .collect();
        assert_eq!(mine.len(), 3, "the three fixtures, once each: {mine:?}");
        for d in mine {
            let folder = d.path.parent().expect("a skill dir has a parent");
            let readers = readers(folder, home.path(), Some(cwd.path()));
            assert!(
                readers.contains(&d.harness_slug),
                "{} attributed to {} but read by {readers:?}",
                d.path.display(),
                d.harness_slug
            );
        }
    }

    #[test]
    fn a_home_reached_through_a_symlink_answers_in_one_spelling() {
        // The shape a real machine hits: `$HOME` carries a symlink while the `cwd` the kernel
        // reports is already resolved, so two roots naming ONE directory used to reach a listing
        // under two different paths. Every answer here is canonical — and the readers query still
        // matches when it is asked in the OTHER spelling.
        let real = TempTree::new("symlink-real");
        real.mkdir(".augment");
        real.skill(".augment/skills/augment-skill");
        let link = real.path().with_file_name(format!(
            "{}-link",
            real.path().file_name().unwrap().to_str().unwrap()
        ));
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        #[cfg(not(unix))]
        return;

        let found = discover_all(&link, None);
        let mine: Vec<&DiscoveredSkill> = found
            .iter()
            .filter(|d| d.path.starts_with(real.path()))
            .collect();
        assert_eq!(
            mine.len(),
            1,
            "the fixture, under the REAL spelling: {found:?}"
        );
        assert!(
            !found.iter().any(|d| d.path.starts_with(&link)),
            "nothing is reported through the link spelling: {found:?}"
        );
        assert_eq!(
            skills_roots(&link, None)
                .into_iter()
                .filter(|r| r.starts_with(real.path()))
                .collect::<Vec<_>>(),
            vec![real.path().join(".augment/skills")],
            "the roots a caller probes are the same one spelling"
        );
        // …and the folder the listing prints still resolves its readers, asked either way.
        let folder = real.path().join(".augment/skills");
        assert_eq!(readers(&folder, &link, None), vec!["augment".to_owned()]);
        assert_eq!(
            readers(&link.join(".augment/skills"), &link, None),
            vec!["augment".to_owned()]
        );
        let _ = std::fs::remove_file(&link);
    }
}
