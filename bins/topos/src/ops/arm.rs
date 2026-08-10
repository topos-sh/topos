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
//! reason a preview can DISCLOSE what an apply will touch: [`Triggers::artifacts`] and
//! [`Triggers::scrub_others`] walk the same set, so `uninstall`'s describe names exactly the
//! artifacts `uninstall --yes` reaches (and `list --footprint`, a path-typed surface, prints that
//! set's path rows). The detection-scoped sweeps
//! ([`arm_detected`], [`probe_detected`]) stay free functions at the composition root, which is the
//! one layer holding `$HOME`, the cwd, and the real ports. Everything is injected, so tests never
//! probe the developer's machine or spawn a harness CLI.

use std::path::{Path, PathBuf};

use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
use topos_harness::{CommandRunner, ConfigStore, registry, triggers};
use topos_types::{TriggerReport, TriggerState};

/// The auto-update-trigger ports a verb acts through: the ACTIVE harness's trigger, plus (in
/// production) the machine root + the two ports every OTHER supported harness's trigger resolves
/// through. The placement half is [`Ctx::harness`](crate::ctx::Ctx) — a separate port, deliberately:
/// a verb arms exactly one harness while disclosing the whole machine's trigger footprint.
///
/// Construction is I/O-free (each adapter is a struct over injected paths), so carrying this on every
/// invocation costs nothing; the harness detection the breadth set needs runs lazily, only when
/// [`Self::artifacts`] or [`Self::scrub_others`] is actually called.
#[derive(Clone)]
pub(crate) struct Triggers<'a> {
    active: &'a dyn TriggerAdapter,
    breadth: Option<Breadth<'a>>,
}

/// Where the whole-machine set comes from.
#[derive(Clone)]
enum Breadth<'a> {
    /// PRODUCTION: every registry row's trigger, resolved under the USER home (each adapter
    /// resolves its own harness root there, env overrides honored) through the two injected ports.
    Machine {
        home: PathBuf,
        cfg: &'a dyn ConfigStore,
        run: &'a dyn CommandRunner,
    },
    /// TESTS: the EXPLICIT adapter set, each built by the test over a root it owns. The registry
    /// resolution above reads `$CLAUDE_CONFIG_DIR` / `$CODEX_HOME` / `$XDG_CONFIG_HOME`, so a rig
    /// that took its breadth from there would aim writes at the developer's real config whenever
    /// one of those happens to be set, whatever temp home it passed. Injecting the adapters is what
    /// puts that out of reach — an ambient variable has nothing left to redirect.
    #[cfg(test)]
    Explicit(&'a [Box<dyn TriggerAdapter + 'a>]),
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
            breadth: Some(Breadth::Machine { home, cfg, run }),
        }
    }

    /// The whole machine as a TEST states it: the active trigger plus exactly the adapters handed
    /// in, each built over a root the test owns. The registry resolution [`Self::machine`] runs
    /// honors per-harness env overrides, which on a developer machine point at real config; a rig
    /// naming its own adapters cannot be redirected by any of them.
    #[cfg(test)]
    pub(crate) fn machine_of(
        active: &'a dyn TriggerAdapter,
        others: &'a [Box<dyn TriggerAdapter + 'a>],
    ) -> Self {
        Self {
            active,
            breadth: Some(Breadth::Explicit(others)),
        }
    }

    /// The active harness's trigger — the one a verb arms on its own receipt.
    pub(crate) fn active(&self) -> &dyn TriggerAdapter {
        self.active
    }

    /// Visit every OTHER supported harness's trigger this machine's scrub reaches. The production
    /// set sweeps every KNOWN registry row rather than the detected ones — an artifact must be
    /// scrubbed even when the harness's detect dir has since vanished — minus the active slug (the
    /// verb handles that one) and minus a trigger whose scrub must dial the harness's OWN program
    /// on a machine where it does not look installed.
    ///
    /// A visitor rather than a returned list because the two breadth sources own their adapters
    /// differently (one builds them, one borrows them); every caller here only ever reads each
    /// adapter once, in order.
    fn for_each_other(&self, mut f: impl FnMut(&dyn TriggerAdapter)) {
        match &self.breadth {
            None => {}
            Some(Breadth::Machine { home, cfg, run }) => {
                let mut detected: Option<Vec<&'static str>> = None;
                for harness in registry::known_harnesses() {
                    if harness.slug == self.active.slug() {
                        continue;
                    }
                    let Some(adapter) = triggers::adapter_for_slug(harness.slug, home, *cfg, *run)
                    else {
                        continue;
                    };
                    if adapter.scrub_needs_live_harness() {
                        let live = detected.get_or_insert_with(|| {
                            registry::detected_harnesses(home, None)
                                .iter()
                                .map(|h| h.slug)
                                .collect()
                        });
                        if !live.contains(&harness.slug) {
                            continue;
                        }
                    }
                    f(adapter.as_ref());
                }
            }
            #[cfg(test)]
            Some(Breadth::Explicit(adapters)) => {
                for adapter in *adapters {
                    f(adapter.as_ref());
                }
            }
        }
    }

    /// Every artifact an `uninstall --yes` REACHES — the active trigger's plus every other
    /// harness's. This is what `uninstall`'s describe discloses (and, filtered to its path rows,
    /// what `list --footprint` prints), so a preview can never name less than the apply touches:
    /// the two walk the same set. Sorted and de-duplicated (two harnesses may share a config file);
    /// paths sort first among themselves, out-of-process rows last.
    pub(crate) fn artifacts(&self) -> Vec<TriggerArtifact> {
        let mut out = self.active.artifacts();
        self.for_each_other(|adapter| out.extend(adapter.artifacts()));
        out.sort();
        out.dedup();
        out
    }

    /// Scrub every OTHER supported agent's trigger (the uninstall half; the active one is the verb's
    /// own). Reports the rows that had something to say — a clean `Inactive` no-op on a filesystem
    /// artifact is noise on an uninstall receipt. An OUT-OF-PROCESS scrub is never filtered: it
    /// dialed the harness's own program, which is work that happened and carries no path to prove
    /// it, and the preview named that artifact — so the receipt answers for it either way.
    pub(crate) fn scrub_others(&self) -> Vec<TriggerReport> {
        let mut out = Vec::new();
        self.for_each_other(|adapter| {
            let reaches_out_of_process = adapter
                .artifacts()
                .iter()
                .any(TriggerArtifact::is_out_of_process);
            let removed = adapter.remove();
            if reaches_out_of_process
                || removed.state != TriggerState::Inactive
                || removed.touched_path.is_some()
            {
                out.push(removed);
            }
        });
        out
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
    fn artifacts(&self) -> Vec<TriggerArtifact> {
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

    /// A fake OpenClaw scheduler: `cron add` REMEMBERS the declaration key it was handed, `cron
    /// list --json` answers with that one job, and `cron rm` forgets it. Enough for the adapter to
    /// register a job and then VERIFY its removal — the only path on which an out-of-process scrub
    /// reports a clean success, which is precisely the row a receipt must still carry.
    #[derive(Debug, Default)]
    struct FakeScheduler {
        job: RefCell<Option<String>>,
    }
    impl CommandRunner for FakeScheduler {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<RunOutput> {
            assert_eq!(
                program, "openclaw",
                "only OpenClaw's own CLI is ever dialed"
            );
            let ok = |stdout: String| {
                Ok(RunOutput {
                    success: true,
                    stdout,
                })
            };
            match args {
                ["cron", "add", rest @ ..] => {
                    let key = rest
                        .windows(2)
                        .find(|w| w[0] == "--declaration-key")
                        .map(|w| w[1].to_owned())
                        .expect("the registration carries its declaration key");
                    *self.job.borrow_mut() = Some(key);
                    ok("{\"created\":true}".to_owned())
                }
                ["cron", "list", "--json"] => {
                    let jobs = match self.job.borrow().as_deref() {
                        Some(key) => {
                            format!("[{{\"id\":\"job-1\",\"declarationKey\":\"{key}\"}}]")
                        }
                        None => "[]".to_owned(),
                    };
                    ok(format!("{{\"jobs\":{jobs}}}"))
                }
                ["cron", "rm", _id] => {
                    *self.job.borrow_mut() = None;
                    ok(String::new())
                }
                _ => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "absent")),
            }
        }
    }

    /// The ACTIVE harness's trigger over an EXPLICIT config root under the injected home. Built
    /// this way and not through [`triggers::adapter_for_slug`] deliberately: that factory resolves
    /// the root through the registry, which reads `$CLAUDE_CONFIG_DIR` — so on a developer machine
    /// with the variable set, a rig writing through a real config store would arm (and then scrub)
    /// the developer's OWN `settings.json`, whatever temp home it passed. Injecting the root leaves
    /// the environment nothing to redirect.
    fn claude_trigger<'a>(home: &Path, cfg: &'a dyn ConfigStore) -> Box<dyn TriggerAdapter + 'a> {
        triggers::claude_code_at(home.join(".claude"), cfg)
    }

    /// The breadth set stated EXPLICITLY, spanning every machinery this port has: cursor + gemini
    /// (a JSON config merge), cline (a dropped file), and openclaw (a job in its own scheduler — no
    /// file anywhere). The three config-file harnesses root plainly under the passed home, so the
    /// factory resolves them without reading the environment; openclaw takes its root directly.
    fn other_triggers<'a>(
        home: &Path,
        cfg: &'a dyn ConfigStore,
        run: &'a dyn CommandRunner,
    ) -> Vec<Box<dyn TriggerAdapter + 'a>> {
        let mut out: Vec<Box<dyn TriggerAdapter + 'a>> = ["cursor", "gemini-cli", "cline"]
            .into_iter()
            .map(|slug| {
                triggers::adapter_for_slug(slug, home, cfg, run)
                    .unwrap_or_else(|| panic!("{slug} has a trigger"))
            })
            .collect();
        out.push(Box::new(topos_harness::OpenClaw::new(
            home.join(".openclaw"),
            cfg,
            run,
        )));
        out
    }

    /// The artifact VOCABULARY, as this suite must cover it. The match is a compile-time fence: a
    /// new [`TriggerArtifact`] variant stops building here until it is named — and the preview test
    /// then requires the preview to carry it, so a new kind of artifact can never join an apply
    /// while the disclosure stays silent about it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ArtifactClass {
        Path,
        OutOfProcess,
    }
    impl ArtifactClass {
        const ALL: [Self; 2] = [Self::Path, Self::OutOfProcess];
        fn of(artifact: &TriggerArtifact) -> Self {
            match artifact {
                TriggerArtifact::Path(_) => Self::Path,
                TriggerArtifact::OutOfProcess { .. } => Self::OutOfProcess,
            }
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
        let active = claude_trigger(&home.0, &cfg);

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
        let active = claude_trigger(&home.0, &cfg);
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

    /// The disclosure contract: the preview names EVERY artifact the apply reaches — every class of
    /// artifact included. The preview walks the ACTIVE trigger plus exactly the set `scrub_others`
    /// walks, so a machine armed across several harnesses can never be told about one of them and
    /// have four more scrubbed — and a trigger with no file to name (OpenClaw's scheduler job) can
    /// never be scrubbed unannounced, which is exactly what a path-only preview did.
    #[test]
    fn the_preview_names_every_artifact_class_the_scrub_reaches() {
        let home = TempHome::new();
        for d in [".claude", ".cursor", ".gemini", ".cline", ".openclaw"] {
            std::fs::create_dir_all(home.0.join(d)).unwrap();
        }
        // A REAL config store under the temp home: the file-drop family's scrub is an unlink, so an
        // in-memory store would report the removal without ever having a file to remove.
        let cfg = crate::fs_seam::RealFs;
        let cli = FakeScheduler::default();
        let active = claude_trigger(&home.0, &cfg);
        let others = other_triggers(&home.0, &cfg, &cli);
        let ports = Triggers::machine_of(active.as_ref(), &others);
        let scheduler_job = TriggerArtifact::OutOfProcess {
            harness: registry::known_harness("openclaw")
                .expect("openclaw is a registry row")
                .display_name,
        };

        // A clean machine owns no FILE anywhere — and still names the scheduler job, because a
        // preview promises what the apply will attempt and never probes a harness to find out.
        assert_eq!(
            ports.artifacts(),
            vec![scheduler_job.clone()],
            "nothing armed → nothing owned on disk, the out-of-process attempt still disclosed"
        );

        // Arm the whole machine, exactly as a login/add receipt does.
        active.install();
        for other in &others {
            other.install();
        }

        // The preview, taken BEFORE the apply — the bytes the user reads.
        let preview = ports.artifacts();
        for expected in [
            home.0.join(".claude").join("settings.json"),
            home.0.join(".cursor").join("hooks.json"),
            home.0.join(".gemini").join("settings.json"),
            home.0.join(".cline").join("hooks").join("TaskStart.sh"),
        ] {
            assert!(
                preview.contains(&TriggerArtifact::Path(expected.clone())),
                "the preview must disclose {expected:?}: {preview:?}"
            );
        }
        assert!(
            preview.contains(&scheduler_job),
            "the preview must disclose the scheduler job: {preview:?}"
        );
        // EVERY class of artifact the vocabulary has is in the preview. The path-only version of
        // this assertion is what let an out-of-process trigger be scrubbed unannounced.
        for class in ArtifactClass::ALL {
            assert!(
                preview.iter().any(|a| ArtifactClass::of(a) == class),
                "no {class:?} artifact in the preview — an apply reaches one: {preview:?}"
            );
        }
        // Every path named is under the temp home: this rig arms nothing on the real machine.
        for artifact in &preview {
            if let Some(path) = artifact.path() {
                assert!(path.starts_with(&home.0), "{path:?} escaped the temp home");
            }
        }

        // The apply: the verb's own scrub of the active harness, then the breadth sweep.
        let mut touched: Vec<PathBuf> = Vec::new();
        if let Some(p) = active.remove().touched_path {
            touched.push(PathBuf::from(p));
        }
        let scrubbed = ports.scrub_others();
        touched.extend(
            scrubbed
                .iter()
                .filter_map(|r| r.touched_path.clone())
                .map(PathBuf::from),
        );
        assert!(
            touched.len() >= 4,
            "the apply reaches every armed harness: {touched:?}"
        );
        for path in &touched {
            assert!(
                preview.contains(&TriggerArtifact::Path(path.clone())),
                "the apply touched {path:?}, which the preview never disclosed"
            );
        }
        // The out-of-process scrub HAPPENED — the scheduler answered that the job is gone — and it
        // has no path to show for it. It rides the receipt all the same: the preview named that
        // artifact, so the receipt is where the promise is answered.
        let openclaw = scrubbed
            .iter()
            .find(|r| r.agent == "openclaw")
            .expect("a removed scheduler job is work that happened — it belongs on the receipt");
        assert_eq!(openclaw.state, TriggerState::Inactive);
        assert!(
            openclaw.touched_path.is_none(),
            "a scheduler job is no file — the row carries no path"
        );
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

        let active = claude_trigger(&home.0, &cfg);
        active.install();
        let ports = Triggers::active_only(active.as_ref());
        assert_eq!(ports.artifacts(), active.artifacts());
        assert!(ports.scrub_others().is_empty());
    }
}
