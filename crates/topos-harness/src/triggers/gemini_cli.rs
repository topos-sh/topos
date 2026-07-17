//! `gemini-cli` — the session-start auto-update hook in `<root>/settings.json` (production root:
//! `~/.gemini`): Claude-Code-shaped matcher groups under a top-level `"hooks"` map, event key
//! `"SessionStart"`, handler `{"type": "command", "command": …, "timeout": 60}`.
//!
//! **Evidence level: vendor docs, unverified** — the shape is taken from Gemini CLI's published
//! hooks documentation; no live build was probed.
//!
//! **Consent posture:** the docs describe hook fingerprinting — Gemini asks the user to confirm
//! a new (or changed) hook command at its own surface before running it, and that confirmation
//! store is not readable evidence here. A successful registration therefore reports `Inactive`
//! with the explicit-pull floor and a note naming the consent step; the kind when the hook is
//! live is `SessionStart`. This adapter never writes Gemini's confirmation state.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "gemini-cli",
    marker_id: "topos:gemini-cli:currency:1",
    config_file: "settings.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: true,
    handler_type: true,
    timeout: Some(60),
    root_seed: None,
    live_kind: CurrencyKind::SessionStart,
    // Gemini gates a new/changed hook behind its own confirm prompt (docs), and that store is
    // not readable evidence — so a successful write is honestly NOT yet live.
    placed_state: TriggerState::Inactive,
    note: Some("Gemini will ask you to confirm the new hook — until then, explicit `topos update`"),
};

/// Production root: `~/.gemini` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".gemini")
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&SPEC, resolve_root(home), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::MemConfig;
    use super::super::{SENTINEL, TriggerAdapter};
    use super::*;

    fn a<'c>(cfg: &'c MemConfig) -> JsonHooks<'c> {
        JsonHooks::new(&SPEC, PathBuf::from("/g"), cfg)
    }

    const CONFIG: &str = "/g/settings.json";

    /// The exact bytes a fresh install produces (2-space pretty, keys alphabetical, trailing
    /// newline): one matcher-free group, the guarded sentinel-marked command, timeout 60.
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
    fn fresh_install_writes_the_exact_hook_and_reports_the_consent_floor() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "gemini-cli");
        assert_eq!(report.marker_id, "topos:gemini-cli:currency:1");
        // The hook is registered, but Gemini's own confirm prompt is a consent step we cannot
        // read — so the report is Inactive + the explicit-pull floor + the consent note.
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.note.as_deref().unwrap().contains("confirm"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let after_first = cfg.text(CONFIG);
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(report.touched_path.is_none(), "rerun touches nothing");
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
        assert_eq!(cfg.text(CONFIG), after_first);
    }

    #[test]
    fn a_stale_managed_entry_migrates_in_place() {
        // An entry an earlier build wrote (old command spelling + a source matcher), recognized
        // by the sentinel alone and rewritten to the canonical handler.
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"startup\",\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(cfg.writes(), 1, "one migrating write");
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(text.matches(SENTINEL).count(), 1, "never duplicated");
        assert!(text.contains("topos update --quiet"));
        assert!(!text.contains("matcher"), "the stale matcher is shed");
        // Idempotent after the migration.
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(report.kind, CurrencyKind::ExplicitPullOnly);
        assert!(report.note.is_none());
        assert_eq!(cfg.writes(), 0, "never blind-append beside a user's hook");
    }

    #[test]
    fn malformed_or_wrong_typed_config_degrades_with_zero_writes() {
        let bad = MemConfig::with_file(CONFIG, "{ this is not json ");
        assert_eq!(a(&bad).install().state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
        assert_eq!(bad.text(CONFIG).as_deref(), Some("{ this is not json "));

        let wrong = MemConfig::with_file(CONFIG, "{\"hooks\": \"oops\"}");
        assert_eq!(a(&wrong).install().state, TriggerState::Degraded);
        assert_eq!(wrong.writes(), 0);
        assert_eq!(
            a(&wrong).remove().state,
            TriggerState::Inactive,
            "nothing ours"
        );
        assert_eq!(wrong.writes(), 0);
    }

    #[test]
    fn remove_is_surgical_then_idempotent() {
        let cfg = MemConfig::with_file(CONFIG, "{\n  \"theme\": \"dark\"\n}\n");
        a(&cfg).install();
        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["theme"], "dark", "the sibling key survives the scrub");
        assert!(root.get("hooks").is_none(), "the map we created is pruned");
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
        assert!(
            adapter.present(),
            "the artifact is present even while the consent step is still owed"
        );
        adapter.remove();
        assert!(!adapter.present());
    }
}
