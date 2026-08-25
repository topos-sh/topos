//! `cursor` — the session-start auto-update hook in `<root>/hooks.json` (production root:
//! `~/.cursor`): `{"version": 1, "hooks": {"sessionStart": [{"command": …}]}}` — a FLAT entry
//! array per event (no matcher groups), lowercase-camel event key, and a top-level schema
//! `version` seeded whenever the key is ABSENT: Cursor's validator requires a numeric `version`
//! and refuses the WHOLE file without one — the user's own hooks included — so absence is a
//! fault this install heals (an existing value is never touched).
//!
//! **Evidence level: verified against a live build** (2026-08: Cursor's shipped hooks loader and
//! validator, run directly over this adapter's exact bytes; `sessionStart` fires on a new
//! conversation, and there is no per-hook consent gate, so a placed entry reports `Active`).
//! Two consequences of that verification shape this spec: the missing-`version` refusal above,
//! and the SENTINEL-LESS command — older Cursor builds deliver the hook payload as a heredoc
//! APPENDED to the command text, and a trailing `#` comment swallows it, so ownership here keys
//! on the exact canonical command instead of the sentinel.

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
    matcher: None,
    handler_type: false,
    command_key: "command",
    timeout: None,
    handler_async: false,
    hook_dialect: None,
    root_seed: Some(("version", 1)),
    command_sentinel: false,
    live_kind: CurrencyKind::SessionStart,
    placed_state: TriggerState::Active,
    note: None,
};

/// Production root: `~/.cursor` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".cursor")
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&SPEC, resolve_root(home), cfg)
}

/// The same trigger in ONE PROJECT: `<project>/.cursor/hooks.json`, the same spec — Cursor reads
/// the project file beside the user one and runs both, so the entry, the seeded `version`, and
/// the sentinel-less command are exactly the user-level bytes.
pub(crate) fn in_project<'a>(root: &Path, cfg: &'a dyn ConfigStore) -> JsonHooks<'a> {
    JsonHooks::new(&SPEC, root.join(".cursor"), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::MemConfig;
    use super::super::{SENTINEL, TriggerAdapter};
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
        \"command\": \"command -v topos >/dev/null 2>&1 && topos install --quiet --from cursor || true\"
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
        assert_eq!(report.agent, "cursor");
        assert_eq!(report.marker_id, "topos:cursor:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note, None);
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
    fn the_version_seed_lands_whenever_the_key_is_absent() {
        // An existing file WITHOUT a version key: Cursor refuses the whole file (the user's own
        // hooks included) without a numeric `version`, so the install heals the absence.
        let cfg = MemConfig::with_file(CONFIG, "{\"hooks\": {}}\n");
        a(&cfg).install();
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["version"], 1, "seeded into a version-less file");
        assert!(root["hooks"]["sessionStart"].is_array());

        // An existing version VALUE is never touched.
        let cfg = MemConfig::with_file(CONFIG, "{\"version\": 7}\n");
        a(&cfg).install();
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["version"], 7);

        // A hand-rolled topos sweep is adopt-or-leave — but the seeded root STILL lands:
        // without `version`, Cursor rejects the whole file, that hand-written hook included.
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"hooks\":{\"sessionStart\":[{\"command\":\"topos update --quiet\"}]}}\n",
        );
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        let root: serde_json::Value = serde_json::from_str(&cfg.text(CONFIG).unwrap()).unwrap();
        assert_eq!(root["version"], 1, "seeded beside the unmanaged entry");
        assert_eq!(
            root["hooks"]["sessionStart"][0]["command"], "topos update --quiet",
            "the person's entry is left exactly as written"
        );

        // A re-run over an already-managed, already-seeded file writes nothing.
        let cfg = MemConfig::with_file(CONFIG, "{\"hooks\": {}}\n");
        a(&cfg).install();
        let writes = cfg.writes();
        a(&cfg).install();
        assert_eq!(
            cfg.writes(),
            writes,
            "seed + hook landed in one write, then no-op"
        );
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
            0,
            "the legacy sentinel-ed entry is rewritten to the sentinel-less canonical"
        );
        assert_eq!(
            text.matches(&super::super::cc_hooks::sweep_command(&SPEC))
                .count(),
            1,
            "never duplicated"
        );
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
        // A VALID file (version present) with a hand-rolled sweep: nothing to heal, zero
        // writes. (A version-less file is healed by the seed — the seed test covers it.)
        let cfg = MemConfig::with_file(
            CONFIG,
            "{\"version\":1,\"hooks\":{\"sessionStart\":[{\"command\":\"topos pull\"}]}}",
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
        assert_eq!(
            cfg.text(CONFIG)
                .unwrap()
                .matches(&super::super::cc_hooks::sweep_command(&SPEC))
                .count(),
            1
        );

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
