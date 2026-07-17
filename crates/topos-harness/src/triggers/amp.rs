//! `amp` — one topos-owned plugin file at `<config-home>/amp/plugins/topos.js` (production
//! config-home: `$XDG_CONFIG_HOME` else `~/.config`). Per the vendor manual's plugin shape, the
//! plugin registers a `session.start` listener via `amp.on(…)` and runs the plain sweep through
//! Amp's shell API, swallowing failures itself.
//!
//! **Evidence level: vendor docs, unverified** — Amp is closed source, so no source probe is
//! possible and no live build was probed; the plugin shape is the manual's. The docs describe
//! no per-plugin consent gate, so the file in place reports `Active` carrying the docs-level
//! note.

use std::path::Path;

use topos_types::CurrencyKind;

use crate::ConfigStore;

use super::file_drop::{FileDrop, FileDropSpec};
use super::{PLAIN_SWEEP, resolve_config_home};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "amp",
    marker_id: "topos:amp:currency:1",
    marker_needle: "topos:amp:currency",
    live_kind: CurrencyKind::SessionStart,
    note: Some("vendor docs, unverified (closed source)"),
};

/// The canonical plugin. The header comment is the ownership block; the sweep is the plain argv
/// form through Amp's shell API — the try/catch is the shell-surface exit-0 tail's analog.
/// Composed from the shared sweep const so the one spelling can never drift per-surface (the
/// byte-exact fixture is pinned in the tests).
fn plugin() -> String {
    format!(
        r#"// topos:amp:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Shape per the Amp plugin manual (vendor docs, unverified — Amp is closed source): a
// session-start listener running the topos currency sweep through Amp's shell API; failures are
// swallowed (currency is best-effort, never session-breaking).
const sweep = async () => {{ try {{ await amp.shell("{PLAIN_SWEEP}") }} catch {{}} }}
sweep()
amp.on("session.start", () => sweep())
"#
    )
}

/// The adapter over an explicit config-home (tests inject; production resolves).
pub(crate) fn in_config_home<'a>(config_home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(
        &SPEC,
        config_home.join("amp").join("plugins").join("topos.js"),
        &plugin(),
        cfg,
    )
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    in_config_home(&resolve_config_home(home), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::TriggerAdapter;
    use super::super::testutil::{DiskConfig, MemConfig, TempHome};
    use super::*;
    use topos_types::TriggerState;

    const PATH: &str = "/cfg/amp/plugins/topos.js";

    /// The byte-exact plugin fixture — pinned as a literal so a drift in the composed
    /// `plugin()` (or the shared sweep const) fails loudly here.
    const PLUGIN_FIXTURE: &str = r#"// topos:amp:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Shape per the Amp plugin manual (vendor docs, unverified — Amp is closed source): a
// session-start listener running the topos currency sweep through Amp's shell API; failures are
// swallowed (currency is best-effort, never session-breaking).
const sweep = async () => { try { await amp.shell("topos update --quiet") } catch {} }
sweep()
amp.on("session.start", () => sweep())
"#;

    fn a<'c>(cfg: &'c MemConfig) -> FileDrop<'c> {
        in_config_home(Path::new("/cfg"), cfg)
    }

    #[test]
    fn the_canonical_plugin_carries_the_sweep_the_listener_and_the_ownership_block() {
        let plugin = plugin();
        assert_eq!(plugin, PLUGIN_FIXTURE, "byte-exact fixture");
        assert!(plugin.contains(&format!("amp.shell(\"{PLAIN_SWEEP}\")")));
        assert!(plugin.contains("amp.on(\"session.start\""));
        assert!(plugin.starts_with("// topos:amp:currency:1"));
        assert!(plugin.contains("Managed by topos"));
        assert!(plugin.contains("topos uninstall"));
    }

    #[test]
    fn fresh_install_places_the_plugin_and_reports_active_with_the_docs_note() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "amp");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert_eq!(
            report.note.as_deref(),
            Some("vendor docs, unverified (closed source)")
        );
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(PLUGIN_FIXTURE));
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
        let cfg = MemConfig::with_file(PATH, "console.log(\"someone else\")\n");
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
        let adapter = in_config_home(&home.0, &cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(adapter.present());
        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("amp/plugins/topos.js").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
    }
}
