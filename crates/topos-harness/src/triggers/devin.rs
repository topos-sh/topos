//! `devin` — the session-start auto-update hook in `<root>/config.json` (production root:
//! `$XDG_CONFIG_HOME/devin`, else `~/.config/devin`). Note the file name: Devin for Terminal
//! keeps its hooks in the same `config.json` that holds the rest of its settings, not in a
//! `settings.json` beside it.
//!
//! The shape is the Claude-Code-compatible one — top-level `"hooks"` → `"SessionStart"` →
//! matcher-free groups wrapping handler arrays, handler `{"type": "command", "command": …,
//! "timeout": 60}`. Devin fires five more lifecycle events; topos registers ONLY `SessionStart`,
//! because what it needs is a refresh moment, not a state feed — every extra event would be one
//! more sweep saying nothing.
//!
//! **Evidence level: hook shape unverified** — it is taken from herdr's shipped Devin
//! integration (Apache-2.0), which writes exactly this file and this schema; no Devin
//! documentation was read here and no live build was probed. Nothing in that integration
//! describes a per-hook consent gate, so a placed entry reports `Active` carrying the
//! evidence-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;
use crate::registry::{self, Root};

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "devin",
    marker_id: "topos:devin:currency:1",
    config_file: "config.json",
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
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: Some("hook shape unverified"),
};

/// Production root: `$XDG_CONFIG_HOME/devin` else `~/.config/devin`, resolved through the
/// registry's ONE resolver so arming and detection can never disagree about where Devin lives.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    registry::config_root(Root::Config, home).join("devin")
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
        JsonHooks::new(&SPEC, PathBuf::from("/v/devin"), cfg)
    }

    const CONFIG: &str = "/v/devin/config.json";

    /// The exact bytes a fresh install produces: one matcher-free group, the guarded
    /// sentinel-marked command, timeout 60 — in `config.json`, not a settings file.
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
        assert_eq!(report.agent, "devin");
        assert_eq!(report.marker_id, "topos:devin:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("hook shape unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert!(
            !FRESH_INSTALL.contains("matcher"),
            "devin's group carries no matcher key"
        );
        assert_eq!(cfg.writes(), 1);
    }

    /// The root follows `$XDG_CONFIG_HOME` exactly as detection does, and the file underneath is
    /// Devin's own `config.json`.
    #[test]
    fn the_config_lives_under_the_config_home() {
        let root = resolve_root(Path::new("/home/me"));
        let expected = registry::config_root(Root::Config, Path::new("/home/me")).join("devin");
        assert_eq!(root, expected);
        let cfg = MemConfig::default();
        let adapter = JsonHooks::new(&SPEC, root.clone(), &cfg);
        assert_eq!(
            adapter.config_file(),
            Some(root.join("config.json")),
            "the hooks live in config.json, never a settings.json beside it"
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
    fn a_stale_managed_entry_migrates_in_place() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"startup\",\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        assert_eq!(a(&cfg).install().state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1, "one migrating write");
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(text.matches(SENTINEL).count(), 1, "never duplicated");
        assert_eq!(text, FRESH_INSTALL, "canonical, the stale matcher shed");
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1, "idempotent after the migration");
    }

    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"topos pull\"}]}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0, "never blind-append beside a user's hook");
    }

    #[test]
    fn malformed_config_degrades_with_zero_writes() {
        let bad = MemConfig::with_file(CONFIG, "{\"hooks\": \"oops\"}");
        assert_eq!(a(&bad).install().state, TriggerState::Degraded);
        assert_eq!(bad.writes(), 0);
        assert_eq!(
            a(&bad).remove().state,
            TriggerState::Inactive,
            "nothing ours to scrub"
        );
        assert_eq!(bad.writes(), 0);
    }

    /// Devin's config.json holds the rest of its settings too — a scrub takes our group and
    /// leaves every one of them byte-identical.
    #[test]
    fn remove_is_surgical_then_idempotent() {
        let before = "{\n  \"model\": \"devin-1\"\n}\n";
        let cfg = MemConfig::with_file(CONFIG, before);
        a(&cfg).install();
        assert!(cfg.text(CONFIG).unwrap().contains(SHELL_SWEEP_LINE));

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["model"], "devin-1", "the user's settings survive");
        assert!(root.get("hooks").is_none(), "the map we created is pruned");

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
