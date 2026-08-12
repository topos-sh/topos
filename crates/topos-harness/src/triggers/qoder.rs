//! `qoder` — the session-start auto-update hook in `<root>/settings.json` (production root:
//! `~/.qoder`): a Claude-Code-COMPATIBLE schema — top-level `"hooks"` → `"SessionStart"` →
//! matcher groups wrapping handler arrays — with the handler `{"type": "command", "command": …,
//! "timeout": 60}`.
//!
//! The one deviation from the reference shape is the matcher: Qoder's own documentation writes
//! the wildcard `"*"` explicitly rather than omitting the key, so this instance states it and a
//! re-arm pins it. The two spellings mean the same thing (every event source); writing the one
//! the harness's own docs show is what keeps a person reading their config from wondering.
//!
//! **Evidence level: vendor docs, unverified** — the schema is the one Qoder's published hooks
//! documentation describes (the docs say it mirrors Claude's `settings.json`), corroborated by
//! herdr's shipped Qoder integration (Apache-2.0), which registers exactly this shape; no live
//! build was probed. The docs describe no per-hook consent gate, so a placed entry reports
//! `Active` carrying the docs-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "qoder",
    marker_id: "topos:qoder:currency:1",
    config_file: "settings.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: true,
    matcher: Some("*"),
    handler_type: true,
    command_key: "command",
    timeout: Some(("timeout", 60)),
    handler_async: false,
    hook_dialect: None,
    root_seed: None,
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: Some("vendor docs, unverified"),
};

/// Production root: `~/.qoder` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".qoder")
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
        JsonHooks::new(&SPEC, PathBuf::from("/q"), cfg)
    }

    const CONFIG: &str = "/q/settings.json";

    /// The exact bytes a fresh install produces — the Claude-Code-compatible group carrying the
    /// documented wildcard matcher, the handler without `async`.
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
        ],
        \"matcher\": \"*\"
      }
    ]
  }
}
";

    #[test]
    fn fresh_install_writes_the_exact_hook_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "qoder");
        assert_eq!(report.marker_id, "topos:qoder:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
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

    /// An entry an earlier build wrote — recognized by the sentinel alone — is rewritten in
    /// place, and the documented wildcard matcher is restored rather than shed.
    #[test]
    fn a_stale_managed_entry_migrates_in_place_and_keeps_the_wildcard_matcher() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "one migrating write");
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(text.matches(SENTINEL).count(), 1, "never duplicated");
        assert_eq!(text, FRESH_INSTALL, "canonical, matcher included");
        // Idempotent after the migration.
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"*\",\"hooks\":[{\"type\":\"command\",\"command\":\"topos update --quiet\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0, "never blind-append beside a user's hook");
    }

    #[test]
    fn malformed_config_degrades_with_zero_writes() {
        let bad = MemConfig::with_file(CONFIG, "{ nope");
        assert_eq!(a(&bad).install().state, TriggerState::Degraded);
        assert_eq!(a(&bad).remove().state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
        assert_eq!(bad.text(CONFIG).as_deref(), Some("{ nope"));
    }

    #[test]
    fn remove_is_surgical_then_idempotent() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"*\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo mine\"}]}]},\"theme\":\"dark\"}",
        );
        a(&cfg).install();
        assert_eq!(
            cfg.text(CONFIG).unwrap().matches(SHELL_SWEEP_LINE).count(),
            1
        );

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["theme"], "dark", "the sibling key survives the scrub");
        let groups = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "only OUR group was pruned");
        assert_eq!(groups[0]["hooks"][0]["command"], "echo mine");

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
