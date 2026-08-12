//! `kilo` — one topos-owned plugin at `~/.config/kilo/plugin/topos.js`. Kilo auto-loads every
//! plugin in that directory; the plugin
//! runs the plain sweep (`topos update --quiet`) once when kilo loads it and again on each
//! `session.created` event, swallowing failures itself — updating is best-effort, never
//! session-breaking.
//!
//! The plugin shape is the OpenCode one (an async factory exporting handlers, `event` carrying
//! `{ type }`), which is what kilo's own plugin loader reads. The sweep goes out through node's
//! `child_process.exec` rather than a shell handle: that is the surface herdr's kilo plugin proves
//! is available, and an error nobody reads is what makes a machine with no `topos` on PATH a silent
//! no-op instead of a session-start failure.
//!
//! **Note the directory, and note the `~/.config`.** Kilo keeps its SKILLS under `~/.kilocode`
//! (the registry row's dirs are untouched by this) while its plugin loader reads
//! `~/.config/kilo/plugin` — two different roots for two different things, which is why arming does
//! not follow the skills dir here. That `~/.config` is LITERAL, not the XDG-overridable config
//! home: herdr's kilo integration joins the dir onto `home` and its env resolution never consults
//! `$XDG_CONFIG_HOME`, so honoring the variable here would write the plugin where kilo does not
//! look. The registry documents the same literal idiom for `crush` and `kimchi`.
//!
//! **Evidence level: plugin API unverified** — the plugin directory and the factory shape are taken
//! from herdr's shipped Kilo integration (Apache-2.0); no Kilo documentation was read here and no
//! live build was probed. Nothing in that integration describes a per-plugin consent gate, so a
//! placement reports `Active` carrying the evidence-level note.

use std::path::Path;

use topos_types::CurrencyKind;

use crate::ConfigStore;

use super::PLAIN_SWEEP;
use super::file_drop::{FileDrop, FileDropSpec};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "kilo",
    marker_id: "topos:kilo:currency:1",
    marker_needle: "topos:kilo:currency",
    live_kind: CurrencyKind::SessionStart,
    note: Some("plugin API unverified"),
};

/// The canonical plugin. The header comment is the ownership block (marker id + the managed-by
/// warning + the removal command); the sweep is the plain argv form, composed from the shared const
/// so the one spelling can never drift per-surface (the byte-exact fixture is pinned in the tests).
fn plugin() -> String {
    format!(
        r#"// topos:kilo:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Runs the topos update sweep at plugin load and on each new session; failures are swallowed
// (updating is best-effort, never session-breaking), so a machine with no topos on PATH is a
// silent no-op.
import {{ exec }} from "node:child_process"

const sweep = () =>
  new Promise((resolve) => {{
    try {{
      exec("{PLAIN_SWEEP}", () => resolve())
    }} catch {{
      resolve()
    }}
  }})

export const ToposCurrency = async () => {{
  await sweep()
  return {{
    event: async ({{ event }}) => {{
      if (event?.type === "session.created") await sweep()
    }},
  }}
}}
"#
    )
}

/// The adapter over an explicit config-home (tests inject; production joins the literal one).
pub(crate) fn in_config_home<'a>(config_home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(
        &SPEC,
        config_home.join("kilo").join("plugin").join("topos.js"),
        &plugin(),
        cfg,
    )
}

/// Production path: the LITERAL `~/.config/kilo/plugin` — `$XDG_CONFIG_HOME` deliberately does not
/// move it (see the module doc).
pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    in_config_home(&home.join(".config"), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::TriggerAdapter;
    use super::super::testutil::{DiskConfig, MemConfig, TempHome};
    use super::*;
    use topos_types::TriggerState;

    const PATH: &str = "/cfg/kilo/plugin/topos.js";

    /// The byte-exact plugin fixture — pinned as a literal so a drift in the composed `plugin()`
    /// (or the shared sweep const) fails loudly here.
    const PLUGIN_FIXTURE: &str = r#"// topos:kilo:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Runs the topos update sweep at plugin load and on each new session; failures are swallowed
// (updating is best-effort, never session-breaking), so a machine with no topos on PATH is a
// silent no-op.
import { exec } from "node:child_process"

const sweep = () =>
  new Promise((resolve) => {
    try {
      exec("topos update --quiet", () => resolve())
    } catch {
      resolve()
    }
  })

export const ToposCurrency = async () => {
  await sweep()
  return {
    event: async ({ event }) => {
      if (event?.type === "session.created") await sweep()
    },
  }
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
            plugin.contains(&format!("exec(\"{PLAIN_SWEEP}\"")),
            "the plain sweep, spelled once"
        );
        assert!(plugin.contains("session.created"), "the session event");
        assert!(plugin.contains("try {"), "failures are swallowed");
        assert!(plugin.starts_with("// topos:kilo:currency:1"));
        assert!(plugin.contains("Managed by topos"));
        assert!(plugin.contains("topos uninstall"));
    }

    #[test]
    fn fresh_install_places_the_plugin_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "kilo");
        assert_eq!(report.marker_id, "topos:kilo:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("plugin API unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(PLUGIN_FIXTURE));
        assert_eq!(cfg.writes(), 1);
    }

    /// The plugin lands under the LITERAL `~/.config` kilo's own loader reads — never moved by
    /// `$XDG_CONFIG_HOME` (kilo does not consult it, so honoring it would write where kilo never
    /// looks), and never in the `~/.kilocode` skills root, which arming has no business writing
    /// into.
    #[test]
    fn the_plugin_lands_under_the_literal_config_dir_not_the_skills_root() {
        let cfg = MemConfig::default();
        let home = Path::new("/home/me");
        let path = adapter(home, &cfg).config_file().expect("a path");
        assert_eq!(path, Path::new("/home/me/.config/kilo/plugin/topos.js"));
        assert!(
            !path.to_string_lossy().contains(".kilocode"),
            "the skills root is left alone"
        );
    }

    #[test]
    fn install_is_idempotent_and_migrates_an_ours_but_stale_plugin() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");

        cfg.set(
            PATH,
            "// topos:kilo:currency:0 old\nexport const ToposCurrency = 1\n",
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
    fn a_foreign_plugin_on_our_path_is_adopt_or_leave() {
        let theirs = "export const Somebody = async () => ({})\n";
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
        let adapter = in_config_home(&home.0, &cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(adapter.present());
        assert_eq!(
            adapter.artifacts(),
            vec![super::super::TriggerArtifact::Path(
                home.0.join("kilo").join("plugin").join("topos.js")
            )]
        );

        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("kilo/plugin/topos.js").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
        assert!(adapter.artifacts().is_empty());
    }
}
