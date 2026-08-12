//! `grok` — one topos-owned hook config at `<root>/hooks/topos.json` (production root: `$GROK_HOME`
//! else `~/.grok`), holding the Claude-Code-shaped group `{"hooks": {"SessionStart": [{"hooks":
//! [{"type": "command", "command": <the guarded sweep>, "timeout": 60}]}]}}`.
//!
//! Grok MERGES every `hooks/*.json` under its config root, so a trigger here needs no shared file
//! at all: topos writes one file of its own and never reads, rewrites, or reorders a person's other
//! hook configs. That is why this is a file drop rather than a config merge — the same reasoning
//! goose's plugin hook file follows.
//!
//! **Evidence level: hook shape unverified** — the merged-directory mechanism and this schema are
//! taken from herdr's shipped Grok integration (Apache-2.0), which installs its own dedicated file
//! in exactly this directory for exactly this reason; no Grok documentation was read here and no
//! live build was probed. Nothing in that integration describes a per-hook consent gate, so a
//! placement reports `Active` carrying the evidence-level note.
//!
//! The file's schema is Grok's and not ours to extend, so the ownership marker is the in-command
//! `# topos:currency` sentinel (JSON has no comment slot, and a `"_comment"` key would be a field
//! the harness never asked for).

use std::path::{Path, PathBuf};

use topos_types::CurrencyKind;

use crate::ConfigStore;
use crate::registry::{self, Root};

use super::SENTINEL;
use super::file_drop::{FileDrop, FileDropSpec};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "grok",
    marker_id: "topos:grok:currency:1",
    // Grok's hook schema has no safe slot for a marker id, so ownership keys on the in-command
    // sentinel the guarded sweep line carries.
    marker_needle: SENTINEL,
    live_kind: CurrencyKind::SessionStart,
    note: Some("hook shape unverified"),
};

/// The canonical hook config — the family's writer style (2-space, keys alphabetical), so a person
/// comparing it with another harness's file sees the same bytes saying the same thing.
const HOOK_CONFIG: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "command": "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency",
            "timeout": 60,
            "type": "command"
          }
        ]
      }
    ]
  }
}
"#;

/// Production root: `$GROK_HOME` (Grok's own config-home override, resolved the way the registry
/// resolves it for discovery) else `~/.grok` under the passed home.
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    registry::config_root(Root::GrokHome, home)
}

/// The adapter over an explicit root (tests inject; production resolves).
pub(crate) fn in_root<'a>(root: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(
        &SPEC,
        root.join("hooks").join("topos.json"),
        HOOK_CONFIG,
        cfg,
    )
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

    const PATH: &str = "/g/hooks/topos.json";

    fn a<'c>(cfg: &'c MemConfig) -> FileDrop<'c> {
        in_root(Path::new("/g"), cfg)
    }

    #[test]
    fn the_canonical_config_is_the_claude_shaped_group_carrying_the_sentinel() {
        let root: serde_json::Value = serde_json::from_str(HOOK_CONFIG).expect("valid JSON");
        let handler = &root["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(handler["type"], "command");
        assert_eq!(handler["command"].as_str().unwrap(), SHELL_SWEEP_LINE);
        assert_eq!(handler["timeout"], 60);
        assert!(
            HOOK_CONFIG.contains(SENTINEL),
            "ownership rides the in-command sentinel (the schema has no comment slot)"
        );
    }

    #[test]
    fn fresh_install_places_our_own_file_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "grok");
        assert_eq!(report.marker_id, "topos:grok:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("hook shape unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(HOOK_CONFIG));
        assert_eq!(cfg.writes(), 1);
    }

    /// Grok merges every file in the hooks dir, so a person's own configs beside ours are not
    /// read, not rewritten, and not counted — arming touches exactly one path.
    #[test]
    fn a_neighbouring_hook_config_is_never_read_or_rewritten() {
        let cfg = MemConfig::with_file(
            "/g/hooks/mine.json",
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo mine\"}]}]}}",
        );
        a(&cfg).install();
        assert_eq!(
            cfg.text("/g/hooks/mine.json").as_deref(),
            Some(
                "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo mine\"}]}]}}"
            ),
            "byte-identical"
        );
        assert_eq!(cfg.writes(), 1, "only our own file");
    }

    #[test]
    fn install_is_idempotent_and_migrates_an_ours_but_stale_config() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");

        // An earlier build's file (the sentinel present, the bytes stale) byte-migrates in place.
        cfg.set(
            PATH,
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"command\":\"topos pull --quiet  # topos:currency\"}]}]}}",
        );
        let migrated = a(&cfg).install();
        assert_eq!(migrated.state, TriggerState::Active);
        assert_eq!(cfg.text(PATH).as_deref(), Some(HOOK_CONFIG));
        assert_eq!(cfg.writes(), 2);
    }

    #[test]
    fn a_foreign_file_on_our_path_is_adopt_or_leave() {
        let theirs =
            "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"command\":\"topos update\"}]}]}}";
        let cfg = MemConfig::with_file(PATH, theirs);
        assert_eq!(
            a(&cfg).install().state,
            TriggerState::AlreadyPresentUnmanaged
        );
        assert_eq!(
            a(&cfg).remove().state,
            TriggerState::AlreadyPresentUnmanaged
        );
        assert_eq!(cfg.writes(), 0);
        assert_eq!(cfg.text(PATH).as_deref(), Some(theirs));
        assert!(!a(&cfg).present());
    }

    #[test]
    fn remove_unlinks_only_ours_then_is_idempotent_and_present_is_honest() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let adapter = in_root(&home.0, &cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(adapter.present());
        assert_eq!(
            adapter.artifacts(),
            vec![super::super::TriggerArtifact::Path(
                home.0.join("hooks").join("topos.json")
            )]
        );

        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("hooks/topos.json").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
        assert!(adapter.artifacts().is_empty());
    }

    /// The root follows `$GROK_HOME` exactly as discovery does, so arming and detection can never
    /// disagree about where Grok lives.
    #[test]
    fn the_hook_config_lives_under_the_grok_home() {
        let root = resolve_root(Path::new("/home/me"));
        assert_eq!(
            root,
            registry::config_root(Root::GrokHome, Path::new("/home/me"))
        );
        let cfg = MemConfig::default();
        assert_eq!(
            in_root(&root, &cfg).config_file(),
            Some(root.join("hooks").join("topos.json"))
        );
    }
}
