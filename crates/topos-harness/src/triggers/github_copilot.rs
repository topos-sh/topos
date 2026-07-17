//! `github-copilot` — one topos-owned hook file at `<root>/hooks/topos.json` (production root:
//! `~/.copilot`): `{"hooks": {"sessionStart": [{"type": "command", "bash": <the guarded sweep>,
//! "timeoutSec": 60}]}, "version": 1}`. JSON has no comments, so the ownership block rides a
//! top-level `"_comment"` field carrying the marker id, the managed-by warning, and the removal
//! command.
//!
//! **Evidence level: vendor docs, unverified** — the shape is the one Copilot CLI's published
//! hooks documentation describes; no live build was probed. The docs describe per-file ADDITIVE
//! loading from the hooks dir and no per-hook consent gate, so the file in place reports
//! `Active` carrying the docs-level note.

use std::path::{Path, PathBuf};

use topos_types::CurrencyKind;

use crate::ConfigStore;

use super::file_drop::{FileDrop, FileDropSpec};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "github-copilot",
    marker_id: "topos:github-copilot:currency:1",
    marker_needle: "topos:github-copilot:currency",
    live_kind: CurrencyKind::SessionStart,
    note: Some("vendor docs, unverified"),
};

/// The canonical hook file (2-space pretty, keys alphabetical, trailing newline — the family's
/// writer style). The `bash` command is the one guarded, sentinel-marked sweep line.
const HOOK_FILE: &str = r#"{
  "_comment": "topos:github-copilot:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.",
  "hooks": {
    "sessionStart": [
      {
        "bash": "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency",
        "timeoutSec": 60,
        "type": "command"
      }
    ]
  },
  "version": 1
}
"#;

/// Production root: `~/.copilot` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".copilot")
}

/// The adapter over an explicit root (tests inject; production resolves).
pub(crate) fn in_root<'a>(root: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(&SPEC, root.join("hooks").join("topos.json"), HOOK_FILE, cfg)
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    in_root(&resolve_root(home), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{DiskConfig, MemConfig, TempHome};
    use super::super::{SHELL_SWEEP_LINE, TriggerAdapter};
    use super::*;
    use topos_types::TriggerState;

    const PATH: &str = "/cop/hooks/topos.json";

    fn a<'c>(cfg: &'c MemConfig) -> FileDrop<'c> {
        in_root(Path::new("/cop"), cfg)
    }

    #[test]
    fn the_canonical_file_is_valid_json_in_the_documented_shape() {
        let root: serde_json::Value = serde_json::from_str(HOOK_FILE).expect("valid JSON");
        let hook = &root["hooks"]["sessionStart"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["bash"].as_str().unwrap(), SHELL_SWEEP_LINE);
        assert_eq!(hook["timeoutSec"], 60);
        assert_eq!(root["version"], 1);
        let comment = root["_comment"].as_str().unwrap();
        assert!(comment.contains(SPEC.marker_id), "the marker id");
        assert!(
            comment.contains("Managed by topos"),
            "the managed-by warning"
        );
        assert!(comment.contains("topos uninstall"), "the removal command");
    }

    #[test]
    fn fresh_install_places_the_file_and_reports_active_with_the_docs_note() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "github-copilot");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("vendor docs, unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(HOOK_FILE));
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_is_idempotent_a_true_no_op_on_rerun() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");
    }

    #[test]
    fn a_foreign_file_on_our_path_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(PATH, "{\"hooks\": {\"sessionStart\": []}}\n");
        let report = a(&cfg).install();
        assert_eq!(report.state, TriggerState::AlreadyPresentUnmanaged);
        assert_eq!(cfg.writes(), 0);
        assert_eq!(
            a(&cfg).remove().state,
            TriggerState::AlreadyPresentUnmanaged
        );
        assert!(!a(&cfg).present());
    }

    #[test]
    fn remove_unlinks_only_ours_then_is_idempotent() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let adapter = in_root(&home.0, &cfg);
        adapter.install();
        assert!(adapter.present());
        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("hooks").join("topos.json").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
    }
}
