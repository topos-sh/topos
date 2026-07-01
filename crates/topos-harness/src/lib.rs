//! `topos-harness` — the `HarnessAdapter` trait + the `ConfigStore` port + the Claude Code reference
//! impl, the OpenClaw impl, and the Hermes impl.
//!
//! The ONE real client-side port. Content-blind: no `translate`, no `project`, no `to_dialect` (cut).
//! Placement *bytes* are identical across adapters; an adapter differs only in *where* + *when currency
//! fires*, and edits its own harness *config* (never a skill dir) to (un)install the currency trigger.
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
mod hermes;
mod openclaw;
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

/// The one real swap port. Key placement by the stable skill id + the discovered concrete path
/// (NOT a bare name — same-name skills, categories, project-vs-global layers must be representable).
/// The adapter never receives skill bytes, never hashes a bundle, never moves a skill file, never
/// contacts the plane; it answers only **where** (`discover` / `placement_for`) and **when**
/// (`currency_kind`), and edits its own harness *config* (never a skill dir) to (un)install currency.
///
/// `skill_id` is a `&str` placeholder for `topos-core`'s `SkillId` domain newtype.
pub trait HarnessAdapter {
    fn id(&self) -> HarnessId;
    fn discover(&self) -> Vec<DiscoveredPlacement>;
    fn placement_for(
        &self,
        skill_id: &str,
        discovered: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget;
    fn currency_kind(&self) -> CurrencyKind;
    /// Idempotently install the currency trigger into the harness config (never a skill dir),
    /// check-before-add against a topos sentinel. Reports what state the trigger is in; a re-run
    /// when the managed entry is already present writes nothing.
    fn install_currency_trigger(&self) -> TriggerReport;
    /// The reverse of [`HarnessAdapter::install_currency_trigger`]: surgically scrub the topos-managed
    /// currency entry from the harness config, leaving every other hook and the file itself intact, so
    /// `uninstall` is a clean no-op for the user's own settings. Idempotent: a no-op (and an honest
    /// state) when no managed entry is present, the config is absent, or it cannot be parsed.
    fn remove_currency_trigger(&self) -> TriggerReport;
    /// Topos-owned paths **outside** any skill dir, for `--footprint` disclosure — never a skill file
    /// and never a path `uninstall` deletes (a shared config the trigger lives in is scrubbed via
    /// [`HarnessAdapter::remove_currency_trigger`], never removed).
    fn uninstall_footprint(&self) -> Vec<PathBuf>;
}

// ClaudeCode (this crate's `claude_code` module) is the reference; OpenClaw (the `openclaw` module)
// ships build-first behind the pilot readiness probe; Hermes (the `hermes` module) is built against a
// probed real local build.
