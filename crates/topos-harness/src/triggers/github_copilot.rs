//! `github-copilot` — the session-start auto-update hook in `<root>/settings.json` (production
//! root: `$COPILOT_HOME` else `~/.copilot`), in Copilot's OWN hook shape rather than the
//! Claude-Code one: a FLAT entry
//! array per event (no matcher groups) whose entries name the command under `bash` and the
//! timeout under `timeoutSec` — `{"type": "command", "bash": …, "timeoutSec": 60}` — under the
//! uppercase `"SessionStart"` event key.
//!
//! **The location and the spelling both moved.** An earlier build dropped a topos-owned
//! `hooks/topos.json` under a lowercase `sessionStart` key; Copilot reads its hooks from
//! `settings.json`, and the lowercase event name is retired. topos writes the current shape and
//! nothing else: no migration, no cleanup — the old dropped file is dead bytes at a path topos no
//! longer touches.
//!
//! Ownership still keys on the `# topos:currency` sentinel the guarded sweep line carries: the
//! command runs under `bash`, so the trailing comment is inert exactly as it is in every other
//! shell-string surface here.
//!
//! **Evidence level: vendor docs, unverified** — the shape is the one Copilot CLI's published
//! hooks documentation describes, corroborated by herdr's shipped Copilot integration
//! (Apache-2.0), which registers exactly these keys in this file; no live build was probed. The
//! docs describe no per-hook consent gate, so a registered entry reports `Active` carrying the
//! docs-level note.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;
use crate::registry;

use super::cc_hooks::{JsonHooks, JsonHooksSpec};

/// Copilot CLI's own config-home override, the one herdr's integration reads.
///
/// It resolves through [`registry::env_override`] — the crate's ONE env read — rather than a
/// [`registry::Root`] variant of its own: `Root` is the vocabulary of the DIRECTORY SPECS baked
/// into registry rows, and no row names this config home. A variant nothing resolves would be
/// vocabulary with no referent.
const ENV_VAR: &str = "COPILOT_HOME";

pub(crate) static SPEC: JsonHooksSpec = JsonHooksSpec {
    slug: "github-copilot",
    // Schema 2 = the flat entry in `settings.json` (schema 1 was the dropped `hooks/topos.json`).
    marker_id: "topos:github-copilot:currency:2",
    config_file: "settings.json",
    events_path: &["hooks"],
    event: "SessionStart",
    grouped: false,
    matcher: None,
    handler_type: true,
    command_key: "bash",
    timeout: Some(("timeoutSec", 60)),
    handler_async: false,
    hook_dialect: None,
    root_seed: None,
    command_sentinel: true,
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: Some("vendor docs, unverified"),
};

/// Production root: `$COPILOT_HOME` else `~/.copilot` under the passed home.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    root_under(registry::env_override(ENV_VAR), home)
}

/// The root an override decides — split out so a test can state both answers without touching the
/// process environment.
fn root_under(override_dir: Option<PathBuf>, home: &Path) -> PathBuf {
    override_dir.unwrap_or_else(|| home.join(".copilot"))
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
        JsonHooks::new(&SPEC, PathBuf::from("/cop"), cfg)
    }

    const CONFIG: &str = "/cop/settings.json";

    /// The exact bytes a fresh install produces: Copilot's flat entry, its own `bash` and
    /// `timeoutSec` keys, the uppercase event.
    const FRESH_INSTALL: &str = "\
{
  \"hooks\": {
    \"SessionStart\": [
      {
        \"bash\": \"command -v topos >/dev/null 2>&1 && topos install --quiet --from github-copilot || true  # topos:currency\",
        \"timeoutSec\": 60,
        \"type\": \"command\"
      }
    ]
  }
}
";

    #[test]
    fn fresh_install_writes_the_exact_hook_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "github-copilot");
        assert_eq!(report.marker_id, "topos:github-copilot:currency:2");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(CONFIG));
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(FRESH_INSTALL));
        assert_eq!(cfg.writes(), 1);

        // The command carries the sentinel — inert under `bash`, and the whole of ownership.
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        let entry = &root["hooks"]["SessionStart"][0];
        assert_eq!(
            entry["bash"].as_str().unwrap(),
            super::super::cc_hooks::sweep_command(&SPEC)
        );
        assert!(entry.get("command").is_none(), "Copilot's key is `bash`");
        assert!(entry.get("timeout").is_none(), "…and `timeoutSec`");
    }

    /// The entry lives in `settings.json` — the retired drop file is never written, and a copy
    /// sitting there is not evidence of anything.
    #[test]
    fn the_hook_lands_in_settings_json_and_never_in_the_retired_drop_file() {
        const RETIRED: &str = "/cop/hooks/topos.json";
        let cfg = MemConfig::default();
        let adapter = a(&cfg);
        adapter.install();
        assert!(cfg.text(RETIRED).is_none(), "no file is ever dropped");
        assert_eq!(adapter.config_file().as_deref(), Some(Path::new(CONFIG)));

        let stale = MemConfig::with_file(RETIRED, FRESH_INSTALL);
        assert!(
            !a(&stale).present(),
            "a copy at the retired path is never claimed as a live trigger"
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

    /// An entry an earlier build wrote — recognized by the sentinel in the `bash` command alone —
    /// is rewritten to the canonical flat entry in place, never duplicated beside.
    #[test]
    fn a_stale_managed_entry_migrates_in_place() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"type\":\"command\",\"bash\":\"topos pull --quiet  # topos:currency\",\"timeoutSec\":5}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 1);
        let text = cfg.text(CONFIG).unwrap();
        assert_eq!(text.matches(SENTINEL).count(), 1, "rewritten, not doubled");
        assert_eq!(text, FRESH_INSTALL);
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1, "idempotent after the migration");
    }

    #[test]
    fn a_hand_rolled_topos_hook_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"type\":\"command\",\"bash\":\"topos update --quiet\",\"timeoutSec\":10}]}}",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0, "never blind-append beside a user's hook");
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
    fn remove_scrubs_only_our_entry_then_is_idempotent() {
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"SessionStart\":[{\"type\":\"command\",\"bash\":\"echo mine\"}],\"Stop\":[]},\"model\":\"gpt-5\"}",
        );
        a(&cfg).install();

        let report = a(&cfg).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["model"], "gpt-5", "the sibling key survives");
        let entries = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "only ours was scrubbed");
        assert_eq!(entries[0]["bash"], "echo mine");
        assert!(
            root["hooks"]["Stop"].is_array(),
            "a sibling event we never touched survives"
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

    /// The entry lands under Copilot's own config home, and its own override moves it — a hook
    /// written where the harness never reads is a trigger reported Active that fires for nobody.
    #[test]
    fn the_config_lives_under_copilots_own_root_or_its_override() {
        let home = Path::new("/home/me");
        assert_eq!(root_under(None, home), PathBuf::from("/home/me/.copilot"));
        assert_eq!(
            root_under(Some(PathBuf::from("/elsewhere/copilot")), home),
            PathBuf::from("/elsewhere/copilot"),
            "the override wins outright"
        );
        assert_eq!(ENV_VAR, "COPILOT_HOME");
    }
}
