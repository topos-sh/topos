//! `topos-harness` — the `HarnessAdapter` trait + its 3 impls.
//!
//! The ONE real client-side port (3 shipping impls). Content-blind: no `translate`, no `project`,
//! no `to_dialect` (cut). Placement *bytes* are identical across adapters (an L1
//! fixture asserts byte-equality); an adapter differs only in *where* + *when currency fires*.
//!
//! This **harness-independent** unit is frozen now (the trait + `CurrencyKind` incl.
//! `ExplicitPullOnly` + `TriggerReport` + the idempotency-marker convention + Claude Code reference
//! bytes); the OpenClaw/Hermes concrete config bytes stay build-first behind the trait until the
//! pilot's real builds are probed.

use std::path::PathBuf;
use topos_types::{CurrencyKind, HarnessId, TriggerReport};

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

/// The one real swap port. Key placement by the stable skill id + the discovered concrete path
/// (NOT a bare name — same-name skills, categories, project-vs-global layers must be representable).
/// The adapter never receives bytes, never hashes, never contacts the plane.
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
    fn install_currency_trigger(&self) -> TriggerReport;
    /// Never a skill file (no-op uninstall — leaves skill bytes untouched).
    fn uninstall_footprint(&self) -> Vec<PathBuf>;
}

// Impls land later — ClaudeCode (reference) first, then OpenClaw and Hermes.
