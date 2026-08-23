//! `pi` — one topos-owned extension at `<home>/.pi/agent/extensions/topos.ts`. Pi auto-loads every
//! extension in that directory; the extension runs the plain sweep (`topos update --quiet`) once
//! when pi loads it and again on every `session_start`, swallowing failures itself — updating is
//! best-effort, never session-breaking.
//!
//! The sweep goes out through node's `child_process.exec`, which is what makes the plugin surface
//! guard-equivalent to the shell one: a machine with no `topos` on PATH answers the callback with
//! an error nobody reads, so the extension is a silent no-op instead of a session-start failure.
//!
//! **The root is home-anchored deliberately.** Pi reads a config-dir override of its own, but the
//! registry row that DISCOVERS pi resolves it under `~/.pi/agent`, and an arming that landed
//! somewhere discovery never looks would be a trigger nobody could see or scrub. Arming follows
//! discovery; the day one moves, both move.
//!
//! **Evidence level: extension API unverified** — the extensions directory, the default-export
//! entry point (`export default function (pi)`) and the `session_start` event are taken from
//! herdr's shipped pi integration (Apache-2.0), which registers on exactly those; no pi
//! documentation was read here and no live build was probed. Nothing in that integration describes
//! a per-extension consent gate, so a placement reports `Active` carrying the evidence-level note.
//! The two calls into pi's own object are written optionally (`pi?.on?.(…)`) for the same reason:
//! an API that turned out to differ must cost a person nothing.

use std::path::Path;

use topos_types::CurrencyKind;

use crate::ConfigStore;

use super::PLAIN_SWEEP;
use super::file_drop::{FileDrop, FileDropSpec};

pub(crate) static SPEC: FileDropSpec = FileDropSpec {
    slug: "pi",
    marker_id: "topos:pi:currency:1",
    marker_needle: "topos:pi:currency",
    live_kind: CurrencyKind::SessionStart,
    note: Some("extension API unverified"),
};

/// The canonical extension. The header comment is the ownership block (marker id + the managed-by
/// warning + the removal command); the sweep is the plain argv form — the try/catch and the ignored
/// callback error are the shell surface's `command -v` guard and exit-0 tail in another language.
/// Composed from the shared sweep const so the one spelling can never drift per-surface (the
/// byte-exact fixture is pinned in the tests).
fn extension() -> String {
    format!(
        r#"// topos:pi:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Runs the topos update sweep when pi loads this extension and again at each session start;
// failures are swallowed (updating is best-effort, never session-breaking), so a machine with no
// topos on PATH is a silent no-op.
// @ts-nocheck
import {{ exec }} from "node:child_process"

const sweep = () =>
  new Promise((resolve) => {{
    try {{
      exec("{PLAIN_SWEEP}", () => resolve())
    }} catch {{
      resolve()
    }}
  }})

export default function (pi) {{
  void sweep()
  pi?.on?.("session_start", async () => {{
    await sweep()
  }})
}}
"#
    )
}

/// The adapter over an explicit extensions dir (tests inject; production resolves).
pub(crate) fn in_extensions_dir<'a>(dir: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    FileDrop::new(&SPEC, dir.join("topos.ts"), &extension(), cfg)
}

pub(crate) fn adapter<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> FileDrop<'a> {
    in_extensions_dir(&home.join(".pi").join("agent").join("extensions"), cfg)
}

#[cfg(test)]
mod tests {
    use super::super::TriggerAdapter;
    use super::super::testutil::{DiskConfig, MemConfig, TempHome};
    use super::*;
    use topos_types::TriggerState;

    const PATH: &str = "/x/topos.ts";

    /// The byte-exact extension fixture — pinned as a literal so a drift in the composed
    /// `extension()` (or the shared sweep const) fails loudly here.
    const EXTENSION_FIXTURE: &str = r#"// topos:pi:currency:1 — Managed by topos; hand edits are overwritten. Remove with `topos uninstall`.
// Runs the topos update sweep when pi loads this extension and again at each session start;
// failures are swallowed (updating is best-effort, never session-breaking), so a machine with no
// topos on PATH is a silent no-op.
// @ts-nocheck
import { exec } from "node:child_process"

const sweep = () =>
  new Promise((resolve) => {
    try {
      exec("topos install --quiet", () => resolve())
    } catch {
      resolve()
    }
  })

export default function (pi) {
  void sweep()
  pi?.on?.("session_start", async () => {
    await sweep()
  })
}
"#;

    fn a<'c>(cfg: &'c MemConfig) -> FileDrop<'c> {
        in_extensions_dir(Path::new("/x"), cfg)
    }

    #[test]
    fn the_canonical_extension_carries_the_sweep_the_event_and_the_ownership_block() {
        let extension = extension();
        assert_eq!(extension, EXTENSION_FIXTURE, "byte-exact fixture");
        assert!(
            extension.contains(&format!("exec(\"{PLAIN_SWEEP}\"")),
            "the plain sweep, spelled once"
        );
        assert!(extension.contains("session_start"), "the load-time event");
        assert!(extension.contains("try {"), "failures are swallowed");
        assert!(extension.starts_with("// topos:pi:currency:1"));
        assert!(extension.contains("Managed by topos"));
        assert!(extension.contains("topos uninstall"));
    }

    #[test]
    fn fresh_install_places_the_extension_and_reports_active() {
        let cfg = MemConfig::default();
        let report = a(&cfg).install();
        assert_eq!(report.agent, "pi");
        assert_eq!(report.marker_id, "topos:pi:currency:1");
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.currency_kind, CurrencyKind::SessionStart);
        assert_eq!(report.note.as_deref(), Some("extension API unverified"));
        assert_eq!(report.touched_path.as_deref(), Some(PATH));
        assert_eq!(cfg.text(PATH).as_deref(), Some(EXTENSION_FIXTURE));
        assert_eq!(cfg.writes(), 1);
    }

    /// The extensions dir hangs off the home the caller passed — never the real one, and never a
    /// path discovery does not know about.
    #[test]
    fn the_extension_lands_in_pis_own_extensions_dir() {
        let cfg = MemConfig::default();
        assert_eq!(
            adapter(Path::new("/home/me"), &cfg).config_file(),
            Some(std::path::PathBuf::from(
                "/home/me/.pi/agent/extensions/topos.ts"
            ))
        );
    }

    #[test]
    fn install_is_idempotent_and_migrates_an_ours_but_stale_extension() {
        let cfg = MemConfig::default();
        a(&cfg).install();
        let report = a(&cfg).install();
        assert!(report.touched_path.is_none());
        assert_eq!(cfg.writes(), 1, "second install writes nothing");

        cfg.set(
            PATH,
            "// topos:pi:currency:0 old\nexport default function () {}\n",
        );
        let migrated = a(&cfg).install();
        assert_eq!(migrated.state, TriggerState::Active);
        assert_eq!(
            cfg.text(PATH).as_deref(),
            Some(EXTENSION_FIXTURE),
            "migrated in place"
        );
    }

    #[test]
    fn a_foreign_extension_on_our_path_is_adopt_or_leave() {
        let theirs = "export default function (pi) { /* mine */ }\n";
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
        let adapter = in_extensions_dir(&home.0, &cfg);
        assert!(!adapter.present());
        adapter.install();
        assert!(adapter.present());
        assert_eq!(
            adapter.artifacts(),
            vec![super::super::TriggerArtifact::Path(home.0.join("topos.ts"))]
        );

        let report = adapter.remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join("topos.ts").exists());
        assert_eq!(adapter.remove().state, TriggerState::Inactive, "idempotent");
        assert!(!adapter.present());
        assert!(adapter.artifacts().is_empty());
    }
}
