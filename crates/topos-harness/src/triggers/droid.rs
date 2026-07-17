//! `droid` — the session-start auto-update hook in `<root>/hooks.json` (production root:
//! `~/.factory`): a Claude-Code-COMPATIBLE schema — top-level `"hooks"` → `"SessionStart"` →
//! matcher groups wrapping handler arrays — with the handler like Claude Code's minus `async`
//! (unsupported there per the docs): `{"type": "command", "command": …, "timeout": 60}`.
//!
//! **Evidence level: vendor docs, unverified** — the schema compatibility is the one Droid's
//! published hooks documentation claims; no live build was probed. The docs describe no
//! per-hook consent gate, so a placed entry reports `Active` carrying the docs-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "droid",
    marker_id: "topos:droid:currency:1",
    config_file: "hooks.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: true,
    handler_type: true,
    timeout: Some(60),
    root_seed: None,
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: Some("vendor docs, unverified"),
};

/// Production root: `~/.factory` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".factory")
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&SPEC, resolve_root(home), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::TriggerAdapter;
    use super::super::testutil::MemConfig;
    use super::*;

    fn a<'c>(cfg: &'c MemConfig) -> JsonHooks<'c> {
        JsonHooks::new(&SPEC, PathBuf::from("/d"), cfg)
    }

    const CONFIG: &str = "/d/hooks.json";

    /// The exact bytes a fresh install produces — the Claude-Code-compatible group shape, the
    /// handler without `async` (unsupported per the docs).
    const FRESH_INSTALL: &str = "\
{
  \"hooks\": {
    \"SessionStart\": [
      {
        \"hooks\": [
          {
            \"command\": \"command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency\",
            \"timeout\": 60,
            \"type\": \"command\"
          }
        ]
      }
    ]
  }
}
";

    #[test]
    fn fresh_install_writes_the_exact_hook_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "droid");
        assert_eq!(report.marker_id, "topos:droid:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert!(
            !FRESH_INSTALL.contains("async"),
            "droid's handler never carries async (unsupported per the docs)"
        );
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
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn malformed_config_degrades_with_zero_writes() {
        let bad = MemConfig::with_file(CONFIG, "{ nope");
        assert_eq!(a(&bad).install().state, TriggerState::Degraded);
        assert_eq!(a(&bad).remove().state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
    }

    #[test]
    fn remove_is_surgical_then_idempotent() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"PostToolUse\":[{\"matcher\":\"Bash\"}]}}",
        );
        a(&cfg).install();
        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert!(
            root["hooks"]["PostToolUse"].is_array(),
            "sibling event survives"
        );
        assert!(
            root["hooks"].get("SessionStart").is_none(),
            "the array we created is pruned"
        );
        let writes = cfg.writes();
        assert_eq!(a(&cfg).remove().state, TriggerState::Inactive);
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
