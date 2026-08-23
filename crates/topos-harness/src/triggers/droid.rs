//! `droid` — the session-start auto-update hook in `<root>/settings.json` (production root:
//! `~/.factory`): a Claude-Code-COMPATIBLE schema — top-level `"hooks"` → `"SessionStart"` →
//! matcher groups wrapping handler arrays — with the handler like Claude Code's minus `async`
//! (unsupported there per the docs): `{"type": "command", "command": …, "timeout": 60}`.
//!
//! **The file moved.** Factory reads its hooks from `settings.json`; `hooks.json` is where an
//! older Droid looked, and an entry left there is simply never read. topos writes the current
//! location and nothing else: no migration, no cleanup — a `hooks.json` an earlier build wrote
//! is dead bytes in a file topos no longer touches.
//!
//! **Evidence level: vendor docs, unverified** — the schema compatibility is the one Droid's
//! published hooks documentation claims, and the file location is corroborated by herdr's
//! shipped Factory integration (Apache-2.0), which writes `settings.json` and migrates the old
//! `hooks.json` away; no live build was probed here. The docs describe no per-hook consent gate,
//! so a placed entry reports `Active` carrying the docs-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "droid",
    // Schema 2 = the entry in `settings.json` (schema 1 sat in the retired `hooks.json`).
    marker_id: "topos:droid:currency:2",
    config_file: "settings.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: true,
    matcher: None,
    handler_type: true,
    command_key: "command",
    timeout: Some(("timeout", 60)),
    handler_async: false,
    hook_dialect: None,
    root_seed: None,
    command_sentinel: true,
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

    const CONFIG: &str = "/d/settings.json";

    /// The exact bytes a fresh install produces — the Claude-Code-compatible group shape, the
    /// handler without `async` (unsupported per the docs).
    const FRESH_INSTALL: &str = "\
{
  \"hooks\": {
    \"SessionStart\": [
      {
        \"hooks\": [
          {
            \"command\": \"command -v topos >/dev/null 2>&1 && topos install --quiet || true  # topos:currency\",
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
        assert_eq!(report.agent, "droid");
        assert_eq!(report.marker_id, "topos:droid:currency:2");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert!(
            !FRESH_INSTALL.contains("async"),
            "droid's handler never carries async (unsupported per the docs)"
        );
        assert_eq!(cfg.writes(), 1);
    }

    /// The entry lives in `settings.json` — the file Factory reads today. The retired
    /// `hooks.json` is never written, and an entry sitting there is not evidence of anything.
    #[test]
    fn the_hook_lands_in_settings_json_and_never_in_the_retired_hooks_file() {
        const RETIRED: &str = "/d/hooks.json";
        let cfg = MemConfig::default();
        let adapter = a(&cfg);
        adapter.install();
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert!(cfg.text(RETIRED).is_none(), "the old file is never written");
        assert_eq!(
            adapter.config_file().as_deref(),
            Some(Path::new(CONFIG)),
            "the file a person is pointed at is the one topos edits"
        );

        let stale = MemConfig::with_file(RETIRED, FRESH_INSTALL);
        assert!(
            !a(&stale).present(),
            "an entry in the retired file is never claimed as a live trigger"
        );
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
