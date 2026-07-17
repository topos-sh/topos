//! The breadth arming sweep — currency triggers for every OTHER detected agent.
//!
//! The placement engine delivers a followed skill's bytes to every detected agent (the shared
//! `~/.agents/skills` copy plus native dirs); this module keeps those copies CURRENT by
//! (un)installing each detected agent's trigger alongside the active adapter's own — the nine
//! registry-slug trigger adapters ([`topos_harness::triggers`]) plus the two non-active sibling
//! `HarnessAdapter`s (OpenClaw's cron; Hermes's session hooks). One sweep command everywhere; the
//! honesty rules are the adapters' own (evidence-gated `Active`, consent never forged, fail-closed
//! config edits) — this module only iterates detection and converts reports for the receipt.
//!
//! Called from the composition root at the same moments the active adapter is armed (the
//! enrollment promote's receipt; `add`'s adopt receipt) and scrubbed (`uninstall --yes`), with the
//! outcomes riding the payloads' additive `triggers` field. Everything is injected (home, cwd, the
//! two ports), so tests never probe the developer's machine or spawn a harness CLI.

use std::path::Path;

use topos_harness::{
    CommandRunner, ConfigStore, HarnessAdapter, Hermes, OpenClaw, registry, triggers,
};
use topos_types::TriggerState;
use topos_types::results::BreadthTriggerReport;

/// Arm the currency trigger of every DETECTED agent other than `active_slug` (the active
/// adapter's, armed by the verb itself). Best-effort per agent: a degraded row is reported, never
/// an aborted sweep.
pub(crate) fn arm_detected(
    home: &Path,
    cwd: Option<&Path>,
    active_slug: &str,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
) -> Vec<BreadthTriggerReport> {
    sweep(home, cwd, active_slug, cfg, run, Action::Install)
}

/// Scrub every non-active agent's trigger (the uninstall half). Sweeps the SUPPORTED set rather
/// than the detected one — a trigger artifact must be scrubbed even when the harness's detect dir
/// has since vanished — and reports only the rows that had something to say (a clean `Inactive`
/// no-op is noise on an uninstall receipt).
pub(crate) fn scrub_all(
    home: &Path,
    active_slug: &str,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
) -> Vec<BreadthTriggerReport> {
    let mut out = Vec::new();
    for slug in triggers::supported_slugs() {
        if *slug == active_slug {
            continue;
        }
        if let Some(adapter) = triggers::adapter_for_slug(slug, home, cfg) {
            let removed = from_outcome(adapter.remove());
            if removed.state != TriggerState::Inactive || removed.touched_path.is_some() {
                out.push(removed);
            }
        }
    }
    for slug in ["openclaw", "hermes-agent"] {
        if slug == active_slug {
            continue;
        }
        // The sibling adapters' removes are honest no-ops when nothing is armed; only a real scrub
        // (or a disclosed degrade — e.g. OpenClaw's gateway down with a job possibly surviving) is
        // worth a receipt row. OpenClaw's remove dials its CLI, so skip it entirely when the
        // harness does not even look installed (never probe a machine that never had it).
        if !registry::detected_harnesses(home, None)
            .iter()
            .any(|h| h.slug == slug)
        {
            continue;
        }
        let report = sibling_adapter_report(slug, home, cfg, run, Action::Remove);
        if let Some(r) = report
            && (r.state != TriggerState::Inactive || r.touched_path.is_some())
        {
            out.push(r);
        }
    }
    out
}

/// What a sweep pass does per agent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    Remove,
}

fn sweep(
    home: &Path,
    cwd: Option<&Path>,
    active_slug: &str,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
    action: Action,
) -> Vec<BreadthTriggerReport> {
    let mut out = Vec::new();
    for harness in registry::detected_harnesses(home, cwd) {
        if harness.slug == active_slug {
            continue;
        }
        if let Some(adapter) = triggers::adapter_for_slug(harness.slug, home, cfg) {
            let outcome = match action {
                Action::Install => adapter.install(),
                Action::Remove => adapter.remove(),
            };
            out.push(from_outcome(outcome));
        } else if let Some(report) = sibling_adapter_report(harness.slug, home, cfg, run, action) {
            out.push(report);
        }
        // Every other detected harness is placement-only (no trigger surface) — its copies stay
        // current through the harness's own session-start skill scan reading the placed bytes.
    }
    out
}

/// The two non-active sibling `HarnessAdapter`s, constructed over the SAME home the registry
/// detected them against (`$HERMES_HOME` honored exactly as detection honored it), so the sweep
/// never arms a harness detection did not see.
fn sibling_adapter_report(
    slug: &str,
    home: &Path,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
    action: Action,
) -> Option<BreadthTriggerReport> {
    let report = match slug {
        "openclaw" => {
            let adapter = OpenClaw::new(home.join(".openclaw"), cfg, run);
            match action {
                Action::Install => adapter.install_currency_trigger(),
                Action::Remove => adapter.remove_currency_trigger(),
            }
        }
        "hermes-agent" => {
            // The one env read here mirrors the registry's own `$HERMES_HOME` resolution (the
            // detect dir already resolved through it); acceptance evidence stays Hermes's own.
            let hermes_home = std::env::var_os("HERMES_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| home.join(".hermes"));
            let adapter = Hermes::new(hermes_home, Hermes::resolve_accept_hooks(), cfg);
            match action {
                Action::Install => adapter.install_currency_trigger(),
                Action::Remove => adapter.remove_currency_trigger(),
            }
        }
        _ => return None,
    };
    Some(BreadthTriggerReport {
        agent: slug.to_owned(),
        currency_kind: report.currency_kind,
        state: report.state,
        touched_path: report.touched_path,
        marker_id: report.marker_id,
        note: None,
    })
}

fn from_outcome(o: triggers::TriggerOutcome) -> BreadthTriggerReport {
    BreadthTriggerReport {
        agent: o.slug.to_owned(),
        currency_kind: o.kind,
        state: o.state,
        touched_path: o.touched_path,
        marker_id: o.marker_id,
        note: o.note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_harness::RunOutput;

    /// A path-keyed in-memory [`ConfigStore`] (the sweep may write several agents' configs).
    #[derive(Debug, Default)]
    struct MemConfig {
        files: RefCell<HashMap<PathBuf, Vec<u8>>>,
    }
    impl ConfigStore for MemConfig {
        fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.files.borrow().get(path).cloned())
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }
    }

    /// A `CommandRunner` whose binary is absent — the honest OpenClaw degrade path (no suite ever
    /// spawns a real harness CLI).
    #[derive(Debug)]
    struct NoBinary;
    impl CommandRunner for NoBinary {
        fn run(&self, _p: &str, _a: &[&str]) -> std::io::Result<RunOutput> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "absent"))
        }
    }

    /// A self-cleaning temp home (RAII) whose detect dirs the test creates explicitly.
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-arm-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The sweep arms exactly the DETECTED trigger-supported agents, skips the active adapter's
    /// own slug, and reports each row honestly. (Env-override harnesses may surface extra rows on
    /// a developer machine — assertions filter to the fixtures' slugs, mirroring the registry's
    /// own test discipline.)
    #[test]
    fn arm_detected_covers_detected_trigger_agents_and_skips_the_active_one() {
        let home = TempHome::new();
        // Detected: cursor (trigger-supported), cline (trigger-supported), augment
        // (placement-only), and claude-code (the ACTIVE adapter — must be skipped).
        for d in [".cursor", ".cline", ".augment", ".claude"] {
            std::fs::create_dir_all(home.0.join(d)).unwrap();
        }
        let cfg = MemConfig::default();
        let out = arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);

        let cursor = out
            .iter()
            .find(|r| r.agent == "cursor")
            .expect("cursor armed");
        assert_eq!(cursor.state, TriggerState::Active);
        let cline = out
            .iter()
            .find(|r| r.agent == "cline")
            .expect("cline armed");
        assert_eq!(cline.state, TriggerState::Active);
        assert!(
            !out.iter().any(|r| r.agent == "claude-code"),
            "the active adapter is armed by its verb, never double-armed here"
        );
        assert!(
            !out.iter().any(|r| r.agent == "augment"),
            "a placement-only harness has no trigger row"
        );
        // The files landed under the injected home only.
        assert!(
            cfg.files
                .borrow()
                .keys()
                .all(|p| p.starts_with(&home.0) || !p.starts_with(std::env::temp_dir())),
        );
    }

    #[test]
    fn a_detected_openclaw_rides_its_own_adapter_and_degrades_honestly() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.0.join(".openclaw")).unwrap();
        let cfg = MemConfig::default();
        let out = arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);
        let oc = out
            .iter()
            .find(|r| r.agent == "openclaw")
            .expect("openclaw swept");
        // No `openclaw` binary in the test runner: the cron cannot be registered — Degraded +
        // the explicit-pull floor, exactly the adapter's own honesty rule.
        assert_eq!(oc.state, TriggerState::Degraded);
        assert_eq!(
            oc.currency_kind,
            topos_types::CurrencyKind::ExplicitPullOnly
        );
    }

    #[test]
    fn scrub_all_reports_only_rows_with_something_to_say() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.0.join(".cursor")).unwrap();
        let cfg = MemConfig::default();
        // Arm cursor first, then scrub everything: only cursor's removal touched a file.
        let _ = arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);
        let out = scrub_all(&home.0, "claude-code", &cfg, &NoBinary);
        assert!(
            out.iter().any(|r| r.agent == "cursor"
                && r.state == TriggerState::Inactive
                && r.touched_path.is_some()),
            "the armed agent's scrub is disclosed"
        );
        assert!(
            !out.iter()
                .any(|r| r.touched_path.is_none() && r.state == TriggerState::Inactive),
            "clean no-ops stay off the receipt"
        );
    }
}
