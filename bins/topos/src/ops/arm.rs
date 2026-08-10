//! The TRIGGER half of the harness ports — the active agent's auto-update trigger, and the breadth
//! sweep over every OTHER agent's.
//!
//! The placement engine delivers a followed skill's bytes to every detected agent (the shared
//! `~/.agents/skills` copy plus native dirs); this module keeps those copies CURRENT by
//! (un)installing each agent's trigger. It iterates ONE list — the registry rows — and asks
//! [`topos_harness::triggers::adapter_for_slug`] for each row's trigger, so which machinery serves a
//! harness (a config merge, a dropped file, its own scheduler) is never this module's business. The
//! honesty rules are the adapters' own: evidence-gated `Active`, consent never forged, fail-closed
//! config edits.
//!
//! [`Triggers`] is what a verb holds ([`Ctx::triggers`](crate::ctx::Ctx)) — the ACTIVE harness's
//! trigger plus, in production, the machine roots the whole-machine set resolves under. It is the
//! reason a preview can DISCLOSE what an apply will touch: [`Triggers::footprint`] and
//! [`Triggers::scrub_others`] walk the same set, so `uninstall`'s describe and `list --footprint`
//! name exactly the harnesses `uninstall --yes` reaches. The detection-scoped sweeps
//! ([`arm_detected`], [`probe_detected`]) stay free functions at the composition root, which is the
//! one layer holding `$HOME`, the cwd, and the real ports. Everything is injected, so tests never
//! probe the developer's machine or spawn a harness CLI.

use std::path::{Path, PathBuf};

use topos_harness::triggers::TriggerAdapter;
use topos_harness::{CommandRunner, ConfigStore, registry, triggers};
use topos_types::{TriggerReport, TriggerState};

/// The auto-update-trigger ports a verb acts through: the ACTIVE harness's trigger, plus (in
/// production) the machine root + the two ports every OTHER supported harness's trigger resolves
/// through. The placement half is [`Ctx::harness`](crate::ctx::Ctx) — a separate port, deliberately:
/// a verb arms exactly one harness while disclosing the whole machine's trigger footprint.
///
/// Construction is I/O-free (each adapter is a struct over injected paths), so carrying this on every
/// invocation costs nothing; the harness detection the breadth set needs runs lazily, only when
/// [`Self::footprint`] or [`Self::scrub_others`] is actually called.
#[derive(Clone)]
pub(crate) struct Triggers<'a> {
    active: &'a dyn TriggerAdapter,
    breadth: Option<Breadth<'a>>,
}

/// What the whole-machine set resolves against: the USER home (each adapter resolves its own harness
/// root under it, env overrides honored) and the two injected ports.
#[derive(Clone)]
struct Breadth<'a> {
    home: PathBuf,
    cfg: &'a dyn ConfigStore,
    run: &'a dyn CommandRunner,
}

impl<'a> Triggers<'a> {
    /// The ACTIVE harness's trigger alone — no breadth. Production with no `$HOME` (detection needs
    /// one), and every test that does not exercise the machine-wide sweep.
    pub(crate) fn active_only(active: &'a dyn TriggerAdapter) -> Self {
        Self {
            active,
            breadth: None,
        }
    }

    /// The whole machine: the active harness's trigger plus every other supported harness's,
    /// resolved under `home` through the two injected ports.
    pub(crate) fn machine(
        active: &'a dyn TriggerAdapter,
        home: PathBuf,
        cfg: &'a dyn ConfigStore,
        run: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            active,
            breadth: Some(Breadth { home, cfg, run }),
        }
    }

    /// The active harness's trigger — the one a verb arms on its own receipt.
    pub(crate) fn active(&self) -> &dyn TriggerAdapter {
        self.active
    }

    /// Every OTHER supported harness's trigger this machine's scrub reaches. Sweeps every KNOWN row
    /// rather than the detected ones — an artifact must be scrubbed even when the harness's detect
    /// dir has since vanished — minus the active slug (the verb handles that one) and minus a
    /// trigger whose scrub must dial the harness's OWN program on a machine where it does not look
    /// installed.
    fn others(&self) -> Vec<Box<dyn TriggerAdapter + 'a>> {
        let Some(b) = &self.breadth else {
            return Vec::new();
        };
        let mut detected: Option<Vec<&'static str>> = None;
        let mut out: Vec<Box<dyn TriggerAdapter + 'a>> = Vec::new();
        for harness in registry::known_harnesses() {
            if harness.slug == self.active.slug() {
                continue;
            }
            let Some(adapter) = triggers::adapter_for_slug(harness.slug, &b.home, b.cfg, b.run)
            else {
                continue;
            };
            if adapter.scrub_needs_live_harness() {
                let live = detected.get_or_insert_with(|| {
                    registry::detected_harnesses(&b.home, None)
                        .iter()
                        .map(|h| h.slug)
                        .collect()
                });
                if !live.contains(&harness.slug) {
                    continue;
                }
            }
            out.push(adapter);
        }
        out
    }

    /// Every topos-owned path outside a skill dir, across EXACTLY the harnesses an `uninstall --yes`
    /// reaches — the active trigger's plus [`Self::others`]'s. This is what `uninstall`'s describe
    /// and `list --footprint` disclose, so a preview can never name less than the apply touches.
    /// Sorted and de-duplicated (two harnesses may share a config file).
    pub(crate) fn footprint(&self) -> Vec<PathBuf> {
        let mut out = self.active.footprint();
        for adapter in self.others() {
            out.extend(adapter.footprint());
        }
        out.sort();
        out.dedup();
        out
    }

    /// Scrub every OTHER supported agent's trigger (the uninstall half; the active one is the verb's
    /// own). Reports only the rows that had something to say — a clean `Inactive` no-op is noise on
    /// an uninstall receipt.
    pub(crate) fn scrub_others(&self) -> Vec<TriggerReport> {
        self.others()
            .into_iter()
            .map(|a| a.remove())
            .filter(|r| r.state != TriggerState::Inactive || r.touched_path.is_some())
            .collect()
    }
}

/// An inert [`TriggerAdapter`] for the rigs whose subject is not the trigger: it arms nothing,
/// scrubs nothing, and owns no path, so a `Ctx` built around it exercises the verb and not the
/// harness config. [`INERT_TRIGGER`] is the `'static` binding a rig borrows.
#[cfg(test)]
pub(crate) struct InertTrigger;

/// The one borrowable [`InertTrigger`] — `'static`, so a rig's `Ctx` needs no extra local.
#[cfg(test)]
pub(crate) static INERT_TRIGGER: InertTrigger = InertTrigger;

#[cfg(test)]
impl TriggerAdapter for InertTrigger {
    fn slug(&self) -> &'static str {
        topos_types::HarnessId::ClaudeCode.slug()
    }
    fn install(&self) -> TriggerReport {
        self.inert()
    }
    fn remove(&self) -> TriggerReport {
        self.inert()
    }
    fn present(&self) -> bool {
        false
    }
    fn footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[cfg(test)]
impl InertTrigger {
    fn inert(&self) -> TriggerReport {
        TriggerReport {
            agent: self.slug().to_owned(),
            currency_kind: topos_types::CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "inert".to_owned(),
            state: TriggerState::Inactive,
            note: None,
        }
    }
}

/// Arm the auto-update trigger of every DETECTED agent other than `active_slug` (the active
/// adapter's, armed by the verb itself). Best-effort per agent: a degraded row is reported, never
/// an aborted sweep.
pub(crate) fn arm_detected(
    home: &Path,
    cwd: Option<&Path>,
    active_slug: &str,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
) -> Vec<TriggerReport> {
    let mut out = Vec::new();
    for harness in registry::detected_harnesses(home, cwd) {
        if harness.slug == active_slug {
            continue;
        }
        if let Some(adapter) = triggers::adapter_for_slug(harness.slug, home, cfg, run) {
            out.push(adapter.install());
        }
        // Every other detected harness is placement-only (no trigger surface) — its copies stay
        // current through the harness's own session-start skill scan reading the placed bytes.
    }
    out
}

/// Probe the auto-update trigger of every DETECTED trigger-capable agent, READ-ONLY — the
/// `status` half of the sweep above: the same detection, the same adapters, but only their
/// provable-presence probes (nothing is armed, repaired, or scrubbed). The active adapter's row
/// rides its own `present` (a config read); an adapter whose trigger lives outside the filesystem
/// answers `armed: None` with its own reason, because proving it would mean running the harness.
/// Placement-only harnesses have no trigger surface and no row.
pub(crate) fn probe_detected(
    home: &Path,
    cwd: Option<&Path>,
    active: &dyn TriggerAdapter,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
) -> Vec<topos_types::results::StatusTrigger> {
    use topos_types::results::StatusTrigger;
    let active_slug = active.slug();
    let mut out = Vec::new();
    for harness in registry::detected_harnesses(home, cwd) {
        if harness.slug == active_slug {
            out.push(StatusTrigger {
                agent: active_slug.to_owned(),
                armed: Some(active.present()),
                note: None,
            });
        } else if let Some(adapter) = triggers::adapter_for_slug(harness.slug, home, cfg, run) {
            let (armed, note) = match adapter.offline_probe_refusal() {
                Some(why) => (None, Some(why.to_owned())),
                None => (Some(adapter.present()), None),
            };
            out.push(StatusTrigger {
                agent: harness.slug.to_owned(),
                armed,
                note,
            });
        }
    }
    out
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

    /// The ACTIVE harness's trigger, built the one way production builds it — through the registry
    /// factory, under the injected home.
    fn active_trigger<'a>(
        home: &Path,
        cfg: &'a dyn ConfigStore,
        run: &'a dyn CommandRunner,
    ) -> Box<dyn TriggerAdapter + 'a> {
        triggers::adapter_for_slug("claude-code", home, cfg, run)
            .expect("claude-code has a trigger")
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

    /// The status probe is READ-ONLY: it reports presence over the same detection the arming
    /// sweep uses, writes nothing, answers honestly per agent (an unarmed cursor probes `false`,
    /// an armed one `true`), refuses OpenClaw's live scheduler query with an explicit unknown,
    /// and gives a placement-only harness no row.
    #[test]
    fn probe_detected_is_read_only_and_honest_per_agent() {
        let home = TempHome::new();
        for d in [".cursor", ".augment", ".claude", ".openclaw"] {
            std::fs::create_dir_all(home.0.join(d)).unwrap();
        }
        let cfg = MemConfig::default();
        let active = active_trigger(&home.0, &cfg, &NoBinary);

        let out = probe_detected(&home.0, None, active.as_ref(), &cfg, &NoBinary);
        let by = |slug: &str| out.iter().find(|r| r.agent == slug);
        assert_eq!(by("claude-code").expect("active row").armed, Some(false));
        assert_eq!(by("cursor").expect("cursor row").armed, Some(false));
        let openclaw = by("openclaw").expect("openclaw row");
        assert_eq!(openclaw.armed, None, "a live-only probe stays unknown");
        assert!(openclaw.note.is_some(), "the unknown names its reason");
        assert!(by("augment").is_none(), "placement-only ⇒ no trigger row");
        assert!(cfg.files.borrow().is_empty(), "the probe writes nothing");

        // Arm cursor through the real sweep, then the probe reports it present.
        arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);
        let out = probe_detected(&home.0, None, active.as_ref(), &cfg, &NoBinary);
        let cursor = out.iter().find(|r| r.agent == "cursor").expect("cursor");
        assert_eq!(cursor.armed, Some(true));
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
    fn scrub_others_reports_only_rows_with_something_to_say() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.0.join(".cursor")).unwrap();
        let cfg = MemConfig::default();
        // Arm cursor first, then scrub everything: only cursor's removal touched a file.
        let _ = arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);
        let active = active_trigger(&home.0, &cfg, &NoBinary);
        let out =
            Triggers::machine(active.as_ref(), home.0.clone(), &cfg, &NoBinary).scrub_others();
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
        assert!(
            !out.iter().any(|r| r.agent == "claude-code"),
            "the active harness is the verb's own scrub, never swept twice"
        );
    }

    /// The disclosure contract: what `uninstall`'s describe and `list --footprint` print covers every
    /// path the apply would touch. The preview walks the ACTIVE trigger plus exactly the set
    /// `scrub_others` walks, so a machine armed across several harnesses can never be told about one
    /// of them and have three more scrubbed.
    #[test]
    fn the_preview_footprint_covers_every_path_the_scrub_touches() {
        let home = TempHome::new();
        // Three trigger-capable harnesses beside the active one, spanning both shared bases: cursor
        // + gemini (a JSON config merge) and cline (a dropped file).
        for d in [".claude", ".cursor", ".gemini", ".cline"] {
            std::fs::create_dir_all(home.0.join(d)).unwrap();
        }
        // A REAL config store under the temp home: the file-drop family's scrub is an unlink, so an
        // in-memory store would report the removal without ever having a file to remove.
        let cfg = crate::fs_seam::RealFs;
        let active = active_trigger(&home.0, &cfg, &NoBinary);
        let ports = Triggers::machine(active.as_ref(), home.0.clone(), &cfg, &NoBinary);

        // A clean machine owns nothing anywhere.
        assert!(
            ports.footprint().is_empty(),
            "nothing armed → nothing owned"
        );

        // Arm the whole machine, exactly as a login/add receipt does.
        active.install();
        arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);

        // The preview, taken BEFORE the apply — the bytes the user reads. Every armed harness is in
        // it, the active one included; before the split only the active harness's row appeared.
        // (A developer machine with a harness env override may add rows on top — the registry's own
        // test discipline — so the fixtures are asserted by name, never by an exact list.)
        let preview = ports.footprint();
        for expected in [
            home.0.join(".claude").join("settings.json"),
            home.0.join(".cursor").join("hooks.json"),
            home.0.join(".gemini").join("settings.json"),
            home.0.join(".cline").join("hooks").join("TaskStart.sh"),
        ] {
            assert!(
                preview.contains(&expected),
                "the preview must disclose {expected:?}: {preview:?}"
            );
        }

        // The apply: the verb's own scrub of the active harness, then the breadth sweep.
        let mut touched: Vec<PathBuf> = Vec::new();
        if let Some(p) = active.remove().touched_path {
            touched.push(PathBuf::from(p));
        }
        touched.extend(
            ports
                .scrub_others()
                .into_iter()
                .filter_map(|r| r.touched_path)
                .map(PathBuf::from),
        );
        assert!(
            touched.len() >= 4,
            "the apply reaches every armed harness: {touched:?}"
        );
        for path in &touched {
            assert!(
                preview.contains(path),
                "the apply touched {path:?}, which the preview never disclosed"
            );
        }
    }

    /// With no `$HOME` there is no detection and therefore no breadth: the active trigger is the
    /// whole surface, both in the preview and in the scrub.
    #[test]
    fn without_a_machine_root_the_active_trigger_is_the_whole_surface() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.0.join(".cursor")).unwrap();
        let cfg = MemConfig::default();
        // Arm cursor so a breadth-aware sweep would have something to find.
        arm_detected(&home.0, None, "claude-code", &cfg, &NoBinary);

        let active = active_trigger(&home.0, &cfg, &NoBinary);
        active.install();
        let ports = Triggers::active_only(active.as_ref());
        assert_eq!(ports.footprint(), active.footprint());
        assert!(ports.scrub_others().is_empty());
    }
}
