//! `cline` — one topos-owned hook script at `<root>/hooks/TaskStart.sh` (production root:
//! `~/.cline`): a bash script running the guarded, sentinel-marked sweep line. Fires at task
//! start — the session-shaped boundary, so the kind is `SessionStart`.
//!
//! **Evidence level:** the hooks directory, the filename-as-event convention (`TaskStart.sh`),
//! and interpreter-by-extension (`.sh` → bash, no exec bit needed) were verified against cline
//! 3.0.43 source (2026-07-16). No consent gate — the file in place IS the live trigger, so a
//! placement reports `Active` with no note owed.

use std::path::{Path, PathBuf};

use topos_types::CurrencyKind;

use crate::ConfigStore;

use super::file_drop::{FileDrop, FileDropSpec};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "cline",
    marker_id: "topos:cline:currency:1",
    marker_needle: "topos:cline:currency",
    live_kind: CurrencyKind::SessionStart,
    note: None, // source-verified; no consent step owed — nothing needs saying
};

/// The canonical hook script: shebang, the ownership block, the one guarded sweep line (its
/// trailing sentinel comment is inert under bash).
const SCRIPT: &str = "#!/usr/bin/env bash
# topos:cline:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
command -v topos >/dev/null 2>&1 && topos update --quiet || true  # topos:currency
";

/// Production root: `~/.cline` under the passed home (no env override in the registry table).
pub(crate) fn resolve_root(home: &Path) -> PathBuf {
    home.join(".cline")
}

/// The adapter over an explicit root (tests inject; production resolves).
pub(crate) fn in_root<'a>(root: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(&SPEC, root.join("hooks").join("TaskStart.sh"), SCRIPT, cfg)
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

    const PATH: &str = "/cl/hooks/TaskStart.sh";

    fn a<'c>(cfg: &'c MemConfig) -> FileDrop<'c> {
        in_root(Path::new("/cl"), cfg)
    }

    #[test]
    fn the_canonical_script_is_the_shebanged_guarded_sweep() {
        assert!(SCRIPT.starts_with("#!/usr/bin/env bash\n"));
        assert!(
            SCRIPT.ends_with(&format!("{SHELL_SWEEP_LINE}\n")),
            "the one guarded, sentinel-marked sweep line"
        );
        assert!(SCRIPT.contains(SPEC.marker_id));
        assert!(SCRIPT.contains("Managed by topos"));
        assert!(SCRIPT.contains("topos uninstall"));
    }

    #[test]
    fn fresh_install_places_the_script_and_reports_active_with_no_note() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "cline");
        assert_eq!(report.marker_id, "topos:cline:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert!(
            report.note.is_none(),
            "source-verified — nothing needs saying"
        );
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(SCRIPT));
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
    fn a_foreign_script_on_our_path_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(PATH, "#!/bin/sh\necho their own task hook\n");
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
        let adapter = in_root(&home.0, &cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(adapter.present());
        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("hooks/TaskStart.sh").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
    }
}
