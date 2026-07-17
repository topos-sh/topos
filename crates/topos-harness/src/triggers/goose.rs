//! `goose` — one topos-owned plugin hook file at `<home>/.agents/plugins/topos/hooks/hooks.json`
//! (production home = the USER home; goose's open-plugins dir), holding the Claude-Code-shaped
//! group `{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": <the guarded
//! sweep>, "timeout": 30}]}]}}`.
//!
//! **Evidence level:** the shape was verified against goose 1.43.0 source (2026-07-16); 30 s is
//! goose's own default hook timeout. The file's exact schema is goose's and not ours to extend,
//! so the ownership marker is the in-command `# topos:currency` sentinel (no comment slot, no
//! `_comment` field).
//!
//! **CRITICAL consent contract:** goose runs hooks ONLY for plugins ENABLED in its own config —
//! that enablement is goose's consent surface, and topos NEVER writes it. Install writes the
//! plugin dir + `hooks.json` and reports `Active` ONLY on read-only, fail-closed evidence of
//! enablement: a zero-risk LINE scan of `<config-home>/goose/config.yaml` for a single
//! zero-indent `plugins:` block holding a `- topos` list item or a `topos: true` key. Anything
//! unreadable or ambiguous (an absent config, non-UTF-8 bytes, duplicate `plugins:` keys, a bare
//! `topos:` key whose nested value we cannot prove, `topos: false`) is NOT evidence — the report
//! then degrades to `Inactive` + the explicit-pull floor with a note naming goose's own consent
//! step. The kind when live is `SessionStart`.

use std::path::{Path, PathBuf};

use topos_types::{CurrencyKind, TriggerState};

use crate::ConfigStore;

use super::file_drop::{FileDrop, FileDropSpec};
use super::{SENTINEL, TriggerAdapter, TriggerOutcome, resolve_config_home};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "goose",
    marker_id: "topos:goose:currency:1",
    // The hooks.json schema is goose's own (source-verified) — no safe slot for a marker id, so
    // ownership keys on the in-command sentinel the guarded sweep line carries.
    marker_needle: SENTINEL,
    live_kind: CurrencyKind::SessionStart,
    note: None, // the wrapper attaches the consent note when enablement evidence is missing
};

/// The consent step still owed when no enablement evidence is readable.
const NOTE: &str = "enable the topos plugin in goose (goose's own consent step)";

/// The canonical plugin hook file (source-verified shape; goose's 30 s default timeout).
const HOOKS_FILE: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "command": "command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency",
            "timeout": 30,
            "type": "command"
          }
        ]
      }
    ]
  }
}
"#;

/// The `goose` [`TriggerAdapter`]: the file-drop base over the plugin hook file, wrapped with
/// the read-only enablement-evidence gate. The base has no API that could write goose's config;
/// this wrapper only READS it.
pub(crate) struct Goose<'a> {
    file: FileDrop<'a>,
    /// `<config-home>/goose/config.yaml` — goose's own config, read-only evidence at most.
    goose_config: PathBuf,
    cfg: &'a dyn ConfigStore,
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> Goose<'a> {
    Goose::new(home, &resolve_config_home(home), cfg)
}

impl<'a> Goose<'a> {
    /// Construct over an explicit user home (the plugin dir root) + config-home (goose's own
    /// config). Tests inject both; production resolves the config-home from the env the way the
    /// registry does.
    pub(crate) fn new(home: &Path, config_home: &Path, cfg: &'a dyn ConfigStore) -> Self {
        let hooks_path = home
            .join(".agents")
            .join("plugins")
            .join("topos")
            .join("hooks")
            .join("hooks.json");
        Self {
            file: FileDrop::new(&SPEC, hooks_path, HOOKS_FILE, cfg),
            goose_config: config_home.join("goose").join("config.yaml"),
            cfg,
        }
    }

    /// Read-only, fail-closed enablement evidence (see the module doc). Never an error, never a
    /// write — an unreadable or oddly-shaped config is simply not evidence.
    fn enablement_evidence(&self) -> bool {
        let Ok(Some(bytes)) = self.cfg.read(&self.goose_config) else {
            return false;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return false;
        };
        enabled_in_config(text)
    }
}

impl TriggerAdapter for Goose<'_> {
    fn slug(&self) -> &'static str {
        "goose"
    }

    fn install(&self) -> TriggerOutcome {
        let mut out = self.file.install();
        // The artifact landed (or already sat) canonical — but goose fires it only when the
        // plugin is enabled, and that consent is goose's own. Without readable evidence the
        // report demotes to the honest floor; the write itself already happened content-blind.
        if out.state == TriggerState::Active && !self.enablement_evidence() {
            out.state = TriggerState::Inactive;
            out.kind = CurrencyKind::ExplicitPullOnly;
            out.note = Some(NOTE.to_owned());
        }
        out
    }

    fn remove(&self) -> TriggerOutcome {
        self.file.remove()
    }

    fn present(&self) -> bool {
        self.file.present()
    }
}

/// The zero-risk line scan: exactly ONE zero-indent `plugins:` key, and within its block (up to
/// the next zero-indent content line) a line whose trim is `- topos` or `topos: true`. A bare
/// `topos:` key (nested content we cannot prove) or a `topos: false` is deliberately NOT
/// evidence.
fn enabled_in_config(text: &str) -> bool {
    // A byte-order mark hides the first line's true column 0 — never reasoned about.
    if text.starts_with('\u{feff}') {
        return false;
    }
    let lines: Vec<&str> = text.lines().collect();
    let keys: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.starts_with([' ', '\t']) && l.trim_end() == "plugins:")
        .map(|(i, _)| i)
        .collect();
    let [idx] = keys[..] else {
        return false; // absent, or duplicate top-level keys (YAML-ambiguous) → not evidence
    };
    for line in &lines[idx + 1..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue; // blanks/comments stay in-region
        }
        if !line.starts_with([' ', '\t']) {
            break; // the next zero-indent content line ends the plugins block
        }
        if trimmed == "- topos" || trimmed == "topos: true" {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{DiskConfig, MemConfig, TempHome};
    use super::*;

    const HOOKS_PATH: &str = "/h/.agents/plugins/topos/hooks/hooks.json";
    const GOOSE_CONFIG: &str = "/c/goose/config.yaml";

    fn a<'c>(cfg: &'c MemConfig) -> Goose<'c> {
        Goose::new(Path::new("/h"), Path::new("/c"), cfg)
    }

    #[test]
    fn the_canonical_file_is_the_source_verified_shape() {
        let root: serde_json::Value = serde_json::from_str(HOOKS_FILE).expect("valid JSON");
        let handler = &root["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(handler["type"], "command");
        assert_eq!(
            handler["command"].as_str().unwrap(),
            super::super::SHELL_SWEEP_LINE
        );
        assert_eq!(handler["timeout"], 30, "goose's own default hook timeout");
        assert!(
            HOOKS_FILE.contains(SENTINEL),
            "ownership rides the in-command sentinel (the schema has no comment slot)"
        );
    }

    #[test]
    fn install_without_enablement_evidence_reports_the_consent_floor() {
        let cfg = MemConfig::default(); // no goose config at all
        let report = a(&cfg).install();
        assert_eq!(report.slug, "goose");
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.kind, CurrencyKind::ExplicitPullOnly);
        assert_eq!(report.note.as_deref(), Some(NOTE));
        assert_eq!(
            cfg.text(HOOKS_PATH).as_deref(),
            Some(HOOKS_FILE),
            "the artifact still lands — only the claim is demoted"
        );
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_with_enablement_evidence_reports_active() {
        for enabled in ["plugins:\n  - topos\n", "plugins:\n  topos: true\n"] {
            let cfg = MemConfig::default();
            cfg.set(GOOSE_CONFIG, enabled);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Active, "{enabled:?}");
            assert_eq!(report.kind, CurrencyKind::SessionStart);
            assert!(report.note.is_none(), "nothing owed once goose consented");
        }
    }

    #[test]
    fn ambiguous_or_unreadable_enablement_is_never_evidence() {
        let not_evidence: &[&str] = &[
            "",                                                         // empty config
            "plugins:\n  topos: false\n",                               // explicitly disabled
            "plugins:\n  topos:\n    enabled: true\n", // a nested value we cannot prove
            "plugins:\n  other-plugin: true\n",        // some other plugin
            "extensions:\n  topos: true\n",            // not the plugins block
            "profiles:\n  default:\n    plugins:\n      topos: true\n", // nested plugins key
            "plugins:\n  - topos\nplugins:\n  x: 1\n", // duplicate keys — YAML-ambiguous
        ];
        for config in not_evidence {
            let cfg = MemConfig::default();
            cfg.set(GOOSE_CONFIG, config);
            let report = a(&cfg).install();
            assert_eq!(report.state, TriggerState::Inactive, "{config:?}");
            assert_eq!(report.note.as_deref(), Some(NOTE));
        }
        // Non-UTF-8 config bytes are unreadable — not evidence either.
        let cfg = MemConfig::default();
        cfg.set_raw(GOOSE_CONFIG, b"\xff\xfeplugins:");
        assert_eq!(a(&cfg).install().state, TriggerState::Inactive);
    }

    #[test]
    fn install_never_writes_goose_config_and_reruns_write_nothing() {
        let cfg = MemConfig::default();
        cfg.set(GOOSE_CONFIG, "plugins: {}\n");
        a(&cfg).install();
        assert_eq!(cfg.writes(), 1, "exactly the hooks.json write");
        assert_eq!(
            cfg.text(GOOSE_CONFIG).as_deref(),
            Some("plugins: {}\n"),
            "goose's own config is READ-ONLY evidence — never written"
        );
        let report = a(&cfg).install();
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "rerun writes nothing");
    }

    #[test]
    fn a_foreign_file_on_our_path_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(HOOKS_PATH, "{\"hooks\": {}}\n");
        assert_eq!(
            a(&cfg).install().state,
            TriggerState::AlreadyPresentUnmanaged
        );
        assert_eq!(
            a(&cfg).remove().state,
            TriggerState::AlreadyPresentUnmanaged
        );
        assert_eq!(cfg.writes(), 0);
        assert!(!a(&cfg).present());
    }

    #[test]
    fn remove_unlinks_only_ours_then_is_idempotent_and_present_is_honest() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let adapter = Goose::new(&home.0, &home.0.join(".config"), &cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(
            adapter.present(),
            "present = the artifact, independent of the consent step"
        );
        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(
            !home
                .0
                .join(".agents/plugins/topos/hooks/hooks.json")
                .exists()
        );
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
    }
}
