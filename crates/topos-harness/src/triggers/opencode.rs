//! `opencode` — one topos-owned plugin file at `<config-home>/opencode/plugin/topos.ts`
//! (production config-home: `$XDG_CONFIG_HOME` else `~/.config`). The plugin runs the plain
//! sweep (`topos update --quiet`) through the plugin API's `$` shell handle at plugin load and
//! again on every `session.created` event, swallowing failures itself — currency is
//! best-effort, never session-breaking.
//!
//! **Evidence level:** plugin auto-discovery from the plugin dir, the `$` BunShell handle, and
//! the `session.created` event were verified against a live containerized opencode-ai 1.18.3
//! (2026-07-16); the global-dir mirror of the probed project-dir loader is vendor-documented.
//! No consent gate (probed) — the file in place IS the live trigger, so a placement reports
//! `Active` with no note owed.

use std::path::Path;

use topos_types::CurrencyKind;

use crate::ConfigStore;

use super::file_drop::{FileDrop, FileDropSpec};
use super::{PLAIN_SWEEP, resolve_config_home};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "opencode",
    marker_id: "topos:opencode:currency:1",
    marker_needle: "topos:opencode:currency",
    live_kind: CurrencyKind::SessionStart,
    note: None, // probed live; no consent step owed — nothing needs saying
};

/// The canonical plugin. The header comment is the ownership block (marker id + the managed-by
/// warning + the removal command); the sweep is the plain argv form — the try/catch is the
/// shell-surface exit-0 tail's analog. Composed from the shared sweep const so the one spelling
/// can never drift per-surface (the byte-exact fixture is pinned in the tests).
fn plugin() -> String {
    format!(
        r#"// topos:opencode:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Runs the topos currency sweep at plugin load and on each new session; failures are swallowed
// (currency is best-effort, never session-breaking).
export const ToposCurrency = async ({{ $ }}) => {{
  const sweep = async () => {{ try {{ await $`{PLAIN_SWEEP}` }} catch {{}} }}
  await sweep()
  return {{ event: async ({{ event }}) => {{ if (event.type === "session.created") await sweep() }} }}
}}
"#
    )
}

/// The adapter over an explicit config-home (tests inject; production resolves).
pub(crate) fn in_config_home<'a>(config_home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(
        &SPEC,
        config_home.join("opencode").join("plugin").join("topos.ts"),
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

    const PATH: &str = "/cfg/opencode/plugin/topos.ts";

    /// The byte-exact plugin fixture — pinned as a literal so a drift in the composed
    /// `plugin()` (or the shared sweep const) fails loudly here.
    const PLUGIN_FIXTURE: &str = r#"// topos:opencode:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Runs the topos currency sweep at plugin load and on each new session; failures are swallowed
// (currency is best-effort, never session-breaking).
export const ToposCurrency = async ({ $ }) => {
  const sweep = async () => { try { await $`topos update --quiet` } catch {} }
  await sweep()
  return { event: async ({ event }) => { if (event.type === "session.created") await sweep() } }
}
"#;

    fn a<'c>(cfg: &'c MemConfig) -> FileDrop<'c> {
        in_config_home(Path::new("/cfg"), cfg)
    }

    #[test]
    fn the_canonical_plugin_carries_the_sweep_the_event_and_the_ownership_block() {
        let plugin = plugin();
        assert_eq!(plugin, PLUGIN_FIXTURE, "byte-exact fixture");
        assert!(
            plugin.contains(&format!("$`{PLAIN_SWEEP}`")),
            "the plain sweep via `$`"
        );
        assert!(plugin.contains("session.created"), "the probed event");
        assert!(plugin.contains("try {"), "failures are swallowed");
        assert!(plugin.starts_with("// topos:opencode:currency:1"));
        assert!(plugin.contains("Managed by topos"));
        assert!(plugin.contains("topos uninstall"));
    }

    #[test]
    fn fresh_install_places_the_plugin_and_reports_active_with_no_note() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.slug, "opencode");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.kind, CurrencyKind::SessionStart);
        assert!(report.note.is_none(), "probed live — nothing needs saying");
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(PLUGIN_FIXTURE));
        assert_eq!(cfg.writes(), 1);
    }

    #[test]
    fn install_is_idempotent_and_migrates_an_ours_but_stale_plugin() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");

        // An earlier build's plugin (needle present, bytes stale) byte-migrates to canonical.
        cfg.set(
            PATH,
            "// topos:opencode:currency:0 old\nexport const ToposCurrency = 1\n",
        );
        let migrated = a(&cfg).install();
        assert_eq!(migrated.state, TriggerState::Active);
        assert_eq!(
            cfg.text(PATH).as_deref(),
            Some(PLUGIN_FIXTURE),
            "migrated in place"
        );
    }

    #[test]
    fn a_foreign_file_on_our_path_is_adopt_or_leave() {
        let cfg = MemConfig::with_file(PATH, "export const Somebody = () => {}\n");
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
    fn remove_unlinks_only_ours_then_is_idempotent() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let adapter = in_config_home(&home.0, &cfg);
        adapter.install();
        assert!(adapter.present());
        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("opencode/plugin/topos.ts").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
    }
}
