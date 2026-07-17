//! `cursor` — the session-start currency hook in `<root>/hooks.json` (production root:
//! `~/.cursor`): `{"version": 1, "hooks": {"sessionStart": [{"command": …}]}}` — a FLAT entry
//! array per event (no matcher groups), lowercase-camel event key, and a top-level schema
//! `version` (seeded as `1` only when the file is created from scratch; an existing file's own
//! value is never touched).
//!
//! **Evidence level: vendor docs, unverified** — the shape is the one Cursor's published hooks
//! documentation describes; no live build was probed. The docs describe no per-hook consent
//! gate, so a placed entry reports `Active` carrying the docs-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "cursor",
    marker_id: "topos:cursor:currency:1",
    config_file: "hooks.json",
    events_path: &["hooks"],
    event: "sessionStart",
    grouped: false,
    handler_type: false,
    timeout: None,
    root_seed: Some(("version", 1)),
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: Some("vendor docs, unverified"),
};

/// Production root: `~/.cursor` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".cursor")
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&SPEC, resolve_root(home), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::MemConfig;
    use super::super::{SENTINEL, SHELL_SWEEP_LINE, TriggerAdapter};
    use super::*;

    fn a<'c>(cfg: &'c MemConfig) -> JsonHooks<'c> {
        JsonHooks::new(&SPEC, PathBuf::from("/c"), cfg)
    }

    const CONFIG: &str = "/c/hooks.json";

    /// The exact bytes a fresh install produces: the flat entry + the seeded schema version.
    const FRESH_INSTALL: &str = "\
{
  \"hooks\": {
    \"sessionStart\": [
      {
        \"command\": \"command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency\"
      }
    ]
  },
  \"version\": 1
}
";

    #[test]
    fn fresh_install_writes_the_exact_hook_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "cursor");
        assert_eq!(report.marker_id, "topos:cursor:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
    }

    #[test]
    fn the_version_seed_lands_only_on_a_from_scratch_file() {
        // An existing file WITHOUT a version key: install registers the hook and leaves the
        // user's schema-key choice alone.
        let cfg = MemConfig::with_file(CONFIG, "{\"hooks\": {}}\n");
        a(&cfg).install();
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert!(
            root.get("version").is_none(),
            "never seeded into an existing file"
        );
        assert!(root["hooks"]["sessionStart"].is_array());

        // And an existing version value is never touched.
        let cfg = MemConfig::with_file(CONFIG, "{\"version\": 7}\n");
        a(&cfg).install();
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["version"], 7);
    }

    #[test]
    fn a_stale_managed_flat_entry_migrates_in_place() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"sessionStart\":[{\"command\":\"topos pull --quiet  # topos:currency\",\"timeout\":5}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1);
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(
            text.matches(SENTINEL).count(),
            1,
            "rewritten, never duplicated"
        );
        assert!(text.contains(SHELL_SWEEP_LINE));
        assert!(
            !text.contains("timeout"),
            "the canonical flat entry has no timeout"
        );
        // Idempotent after the migration.
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"sessionStart\":[{\"command\":\"topos pull\"}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn malformed_config_degrades_with_zero_writes() {
        let bad = MemConfig::with_file(CONFIG, "not json at all");
        assert_eq!(a(&bad).install().state, TriggerState::Degraded);
        assert_eq!(a(&bad).remove().state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
        assert_eq!(bad.text(CONFIG).as_deref(), Some("not json at all"));
    }

    #[test]
    fn remove_scrubs_only_our_flat_entry_then_is_idempotent() {
        // A user's own entry shares the event array; only ours is scrubbed, and the array (and
        // maps) survive because our removal did not empty them.
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"sessionStart\":[{\"command\":\"echo mine\"}]},\"version\":1}",
        );
        a(&cfg).install();
        assert_eq!(cfg.text(CONFIG).unwrap().matches(SENTINEL).count(), 1);

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        let entries = root["hooks"]["sessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "only ours was scrubbed");
        assert_eq!(entries[0]["command"], "echo mine");
        assert_eq!(root["version"], 1, "the schema key survives");

        let writes = cfg.writes();
        let again = a(&cfg).remove();
        assert_eq!(again.state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), writes, "second remove writes nothing");
    }

    #[test]
    fn present_is_honest() {
        let cfg = MemConfig::default();
        let adapter = a(&cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(adapter.present());
        adapter.remove();
        assert!(!adapter.present());
    }
}
