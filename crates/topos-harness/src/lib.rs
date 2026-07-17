//! `topos-harness` — the `HarnessAdapter` trait + the `ConfigStore` port + the Claude Code reference
//! impl, the OpenClaw impl, and the Hermes impl.
//!
//! The ONE real client-side port. Content-blind: no `translate`, no `project`, no `to_dialect` (cut).
//! Placement *bytes* are identical across adapters; an adapter differs only in *where* + *when the update check
//! fires*, and edits its own harness *config* (never a skill dir) to (un)install the auto-update trigger.
//!
//! This **harness-independent** unit is frozen (the trait + `CurrencyKind` incl. `ExplicitPullOnly` +
//! `TriggerReport` + the idempotency-marker convention); the OpenClaw impl ships **build-first behind the
//! trait** — its concrete config bytes stay provisional until the pilot's real build is probed (see the
//! `openclaw` module doc) — and Hermes's were probed against a real local build, with the pilot's exact
//! build staying a MUST-VERIFY (see `hermes.rs`).

use std::io;
use std::path::{Path, PathBuf};
use topos_types::{CurrencyKind, HarnessId, TriggerReport};

mod claude_code;
pub mod coverage;
mod hermes;
mod openclaw;
pub mod registry;
pub mod triggers;
pub use claude_code::ClaudeCode;
pub use hermes::Hermes;
pub use openclaw::OpenClaw;

/// A discovered skill placement — probe known dirs; read frontmatter to CONFIRM only. Carries the
/// concrete path + category/layer (Hermes `<category>/<name>`, project/global).
#[derive(Debug, Clone)]
pub struct DiscoveredPlacement {
    pub path: PathBuf,
    pub layer: Option<String>,
}

/// Where a skill's exact bytes get written for this harness (no frontmatter rewrite, no metadata).
#[derive(Debug, Clone)]
pub struct PlacementTarget {
    pub dir: PathBuf,
}

/// Advisory naming hints for a follower's first-receive placement: the skill's plane-supplied display name
/// (the author's folder name) and a workspace slug to disambiguate a name collision. BOTH are UNTRUSTED,
/// unsigned strings (unlike the validated `skill_id`) — an adapter MUST route them through
/// [`sanitize_skill_dir`] before using them as a path component. Absent (or unsafe) falls the placement
/// back to the validated `skill_id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlacementNaming<'a> {
    pub name: Option<&'a str>,
    pub workspace_slug: Option<&'a str>,
}

/// Fold an UNTRUSTED, unsigned name (a plane-supplied skill display name / workspace slug) into a SAFE
/// single path component for a skills folder — or `None` when nothing safe remains (the caller then falls
/// back to the validated id). The result is guaranteed to be ONE component: it keeps only ASCII
/// alphanumerics + `_`, folds every run of anything else (whitespace, `.`, `/`, `\`, punctuation, control,
/// non-ASCII) to a single interior `-`, and carries no leading/trailing `-` — so it can never contain a
/// path separator, can never be `.`/`..`, and can never escape the skills dir. Length-capped.
#[must_use]
pub fn sanitize_skill_dir(raw: &str) -> Option<String> {
    const MAX: usize = 64;
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
            if out.len() >= MAX {
                break;
            }
        } else {
            // Everything else folds to a single separating dash — so no run of dots can ever form `.`/`..`
            // and no `/`, `\`, or other separator can survive into the component.
            pending_dash = true;
        }
    }
    // Leading/trailing dashes are already prevented above; trim defensively anyway.
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Choose the directory a skill's bytes land in under `skills_root` — the ONE naming discipline every
/// placement target follows (the reference adapter's, factored out so registry-resolved dirs name
/// identically): prefer the skill's **sanitized display name** (agents invoke a skill by its folder
/// name); on a collision with a dir that is neither FREE nor this skill's own recorded placement,
/// disambiguate by the sanitized workspace slug (`<ws>-<name>`); fall back to the validated,
/// globally-unique `skill_id`. A foreign dir is NEVER a valid target — only a free dir, or one
/// `is_owned` answers `true` for (the caller's own placement record), is ever chosen, so a placement
/// can never clobber another skill's (or the user's) directory.
///
/// `naming`'s strings are UNTRUSTED and are sanitized to a single safe path component before any join;
/// `skill_id` must be an already-validated single component (the trait-wide id contract).
#[must_use]
pub fn choose_skill_dir(
    skills_root: &Path,
    skill_id: &str,
    naming: PlacementNaming<'_>,
    is_owned: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(name) = naming.name.and_then(sanitize_skill_dir) {
        let by_name = skills_root.join(&name);
        if !by_name.exists() || is_owned(&by_name) {
            return by_name;
        }
        // Collision: a different skill (or the user's own dir) already holds this name. Namespace by
        // the workspace so the two coexist (both parts are already sanitized single components).
        if let Some(ws) = naming.workspace_slug.and_then(sanitize_skill_dir) {
            let namespaced = skills_root.join(format!("{ws}-{name}"));
            if !namespaced.exists() || is_owned(&namespaced) {
                return namespaced;
            }
        }
    }
    // Unnamed / unsafe name / every candidate taken → the unique id (a validated single component
    // that can never collide with another skill).
    skills_root.join(skill_id)
}

/// The narrow filesystem port an adapter needs to read + atomically replace a harness **config** file
/// (e.g. `~/.claude/settings.json`) — never a skill bundle. Defined here in the low crate and
/// implemented by the CLI (the high crate) over its one fault-injectable syscall seam, so the adapter
/// owns config *semantics* (the strict-JSON merge) while the single crash-safe `temp → fsync → rename →
/// fsync-dir` write lives in exactly one place — no second atomic-write to drift. The adapter holding
/// this port (rather than receiving an `FsOps`) is what lets the frozen, parameter-free
/// [`HarnessAdapter::install_currency_trigger`] still perform a fault-injectable write.
pub trait ConfigStore {
    /// Read a config file's bytes, or `None` if it does not exist.
    ///
    /// # Errors
    /// An underlying I/O failure other than not-found (e.g. a permission error).
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
    /// Atomically, crash-safely replace `path`'s contents with `bytes` (temp → fsync → rename →
    /// fsync-dir), creating the parent directory if absent and writing **through** a symlink to its
    /// target (never replacing the link). The bytes are the caller's whole, validated config.
    ///
    /// # Errors
    /// An underlying I/O failure.
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
}

/// One captured subprocess run for [`CommandRunner`]: whether the process exited successfully,
/// plus its captured stdout (the machine surface — `--json` outputs are parsed from here).
#[derive(Debug, Clone)]
pub struct RunOutput {
    /// The process ran AND exited zero.
    pub success: bool,
    /// Captured stdout, lossily decoded.
    pub stdout: String,
}

/// The narrow subprocess port an adapter needs to drive a harness's OWN management CLI (OpenClaw's
/// `openclaw cron …`) — an argv in, a captured outcome out. Argv-only by design: the adapter never
/// composes shell strings, so nothing it passes is ever re-interpreted. Production (the CLI crate)
/// implements it over `std::process` with the binary resolved from `PATH`; every test injects a
/// fake, so no suite ever spawns a real harness process.
pub trait CommandRunner {
    /// Run `program` with `args`, capturing output. `Ok` means the process RAN (its own exit
    /// status rides [`RunOutput::success`]).
    ///
    /// # Errors
    /// A spawn-level failure — the binary is absent (`NotFound`) or another OS-level error.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<RunOutput>;
}

/// The one real swap port. Key placement by the stable skill id + the discovered concrete path
/// (NOT a bare name — same-name skills, categories, project-vs-global layers must be representable).
/// The adapter never receives skill bytes, never hashes a bundle, never moves a skill file, never
/// contacts the plane; it answers only **where** (`discover` / `placement_for`) and **when**
/// (`currency_kind`), and edits its own harness *config* (never a skill dir) to (un)install currency.
///
/// `skill_id` stays a plain `&str` at this seam (this crate holds no id type), but it is joined as a
/// **single path component** into the harness skills dir — so callers MUST pass an already-validated id.
/// The CLI enforces that with its validated-id newtype, parsed at every wire/persisted boundary an id
/// enters (a plane-supplied `"../../x"` never reaches this trait).
pub trait HarnessAdapter {
    fn id(&self) -> HarnessId;
    fn discover(&self) -> Vec<DiscoveredPlacement>;
    /// Where this harness places `skill_id`'s bytes. A pure follower's first receive prefers the skill's
    /// real (sanitized) display name from `naming` so the agent invokes it by name; the adapter MUST
    /// route `naming`'s untrusted strings through [`sanitize_skill_dir`] and fall back to the validated
    /// `skill_id` (a single path component) when the name is absent/unsafe or every candidate collides.
    fn placement_for(
        &self,
        skill_id: &str,
        naming: PlacementNaming<'_>,
        discovered: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget;
    fn currency_kind(&self) -> CurrencyKind;
    /// Idempotently install the auto-update trigger into the harness config (never a skill dir),
    /// check-before-add against a topos sentinel. Reports what state the trigger is in; a re-run
    /// when the managed entry is already present writes nothing.
    fn install_currency_trigger(&self) -> TriggerReport;
    /// The reverse of [`HarnessAdapter::install_currency_trigger`]: surgically scrub the topos-managed
    /// auto-update entry from the harness config, leaving every other hook and the file itself intact, so
    /// `uninstall` is a clean no-op for the user's own settings. Idempotent: a no-op (and an honest
    /// state) when no managed entry is present, the config is absent, or it cannot be parsed.
    fn remove_currency_trigger(&self) -> TriggerReport;
    /// Topos-owned paths **outside** any skill dir, for `--footprint` disclosure — never a skill file
    /// and never a path `uninstall` deletes (a shared config the trigger lives in is scrubbed via
    /// [`HarnessAdapter::remove_currency_trigger`], never removed).
    fn uninstall_footprint(&self) -> Vec<PathBuf>;
    /// Whether a topos-managed auto-update trigger is PROVABLY present right now — the hook-health
    /// probe `list` / `auth status` read. Defaults to the footprint being non-empty (a config-file
    /// adapter's footprint discloses exactly its managed entry); an adapter whose trigger lives
    /// OUTSIDE the filesystem (OpenClaw's scheduler) overrides this with a live probe. Anything
    /// unprovable answers `false` — health is never claimed on faith.
    fn trigger_present(&self) -> bool {
        !self.uninstall_footprint().is_empty()
    }
}

// ClaudeCode (this crate's `claude_code` module) is the reference; OpenClaw (the `openclaw` module)
// ships build-first behind the pilot readiness probe; Hermes (the `hermes` module) is built against a
// probed real local build.
