//! The TRIGGER ports a verb holds — the active harness's auto-update trigger plus, in production,
//! the machine roots the whole-machine set resolves under.
//!
//! Which agents GET a hook is the pick's business (`super::agent_hooks`, over the scoped trigger
//! factory); this module is what a verb DISCLOSES and SCRUBS. It iterates registry rows and asks
//! [`topos_harness::triggers::adapter_for_slug`] for each row's trigger, so which machinery serves a
//! harness (a config merge, a dropped file, its own scheduler) is never this module's business. The
//! honesty rules are the adapters' own: evidence-gated `Active`, consent never forged, fail-closed
//! config edits.
//!
//! TEARDOWN follows [`registry::teardown_harnesses`] — the rows this machine resolved PLUS the
//! bundled floor — because the hook a dropped row was armed with is still in that agent's config,
//! and a scrub that walked past it would leave an agent invoking a topos this teardown just deleted.
//! The pick plays no part in a teardown either: an agent somebody picked and later left out may
//! still hold a hook an earlier build wrote.
//!
//! [`Triggers`] is what a verb holds ([`Ctx::triggers`](crate::ctx::Ctx)). It is the reason a
//! preview can DISCLOSE what an apply will touch: [`Triggers::artifacts`] and
//! [`Triggers::scrub_others`] walk the same set, so `uninstall`'s describe names exactly the
//! artifacts `uninstall --yes` reaches (and `list --footprint`, a path-typed surface, prints that
//! set's path rows). [`Triggers::project_hook_files`] and [`Triggers::scrub_project`] are the
//! project half of that pair: the hook files ONE checkout holds (the one the command ran in),
//! which a teardown names and then scrubs the same way. [`Triggers::machine_ports`] hands the
//! pick-scoped sweeps the same roots and ports, so `status` probes through the one layer that
//! holds them.

use std::path::{Path, PathBuf};

use topos_harness::triggers::{TriggerAdapter, TriggerArtifact, TriggerScope};
use topos_harness::{CommandRunner, ConfigStore, registry, triggers};
use topos_types::{TriggerReport, TriggerState};

/// The auto-update-trigger ports a verb acts through: the ACTIVE harness's trigger, plus (in
/// production) the machine root + the two ports every OTHER supported harness's trigger resolves
/// through. The placement half is [`Ctx::harness`](crate::ctx::Ctx) — a separate port, deliberately:
/// a verb scrubs exactly one harness on its own account while disclosing the whole machine's
/// trigger footprint.
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
    /// puts that out of reach — an ambient variable has nothing left to redirect. `project` is the
    /// ports the PROJECT walks resolve through when a rig wants them (a project adapter takes an
    /// explicit root, so no ambient variable can redirect it); `None` keeps those walks empty.
    #[cfg(test)]
    Explicit {
        adapters: &'a [Box<dyn TriggerAdapter + 'a>],
        project: Option<MachinePorts<'a>>,
    },
}

/// The machine roots + ports the pick-scoped sweeps resolve triggers through — what
/// [`Triggers::machine_ports`] hands out in production, and nothing in a rig that stated its
/// adapters explicitly (there is no home there for a factory to resolve under).
#[derive(Clone, Copy)]
pub(crate) struct MachinePorts<'a> {
    pub home: &'a Path,
    pub cfg: &'a dyn ConfigStore,
    pub run: &'a dyn CommandRunner,
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
            breadth: Some(Breadth::Explicit {
                adapters: others,
                project: None,
            }),
        }
    }

    /// [`Self::machine_of`] whose PROJECT walks ([`Self::project_hook_files`],
    /// [`Self::scrub_project`]) resolve through `cfg` + `run` under `home` — for the rigs whose
    /// subject is a checkout's hooks. The user-level breadth stays the explicit set: nothing here
    /// reaches a config outside the root the test names.
    #[cfg(test)]
    pub(crate) fn machine_of_with_project_ports(
        active: &'a dyn TriggerAdapter,
        others: &'a [Box<dyn TriggerAdapter + 'a>],
        home: &'a Path,
        cfg: &'a dyn ConfigStore,
        run: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            active,
            breadth: Some(Breadth::Explicit {
                adapters: others,
                project: Some(MachinePorts { home, cfg, run }),
            }),
        }
    }

    /// The machine roots + ports, where this set resolves triggers through them (production
    /// under `$HOME`); `None` with no breadth, and for a rig's explicit set.
    pub(crate) fn machine_ports(&self) -> Option<MachinePorts<'_>> {
        match &self.breadth {
            Some(Breadth::Machine { home, cfg, run }) => Some(MachinePorts {
                home,
                cfg: *cfg,
                run: *run,
            }),
            None => None,
            #[cfg(test)]
            Some(Breadth::Explicit { .. }) => None,
        }
    }

    /// The ports the PROJECT walks resolve through: the machine's in production, a rig's stated
    /// project ports otherwise. `None` = no project walk at all.
    fn project_ports(&self) -> Option<MachinePorts<'_>> {
        #[cfg(test)]
        if let Some(Breadth::Explicit { project, .. }) = &self.breadth {
            return *project;
        }
        self.machine_ports()
    }

    /// Visit every OTHER supported harness's trigger this machine's scrub reaches. The production
    /// set sweeps the TEARDOWN table ([`registry::teardown_harnesses`] — the loaded rows plus the
    /// bundled floor) rather than the detected ones — an artifact must be scrubbed even when the
    /// harness's detect dir has since vanished, and even when a newer downloaded table has since
    /// dropped its row, or `uninstall --yes` would delete the sidecar and leave that agent's hook
    /// invoking a topos that is gone. Minus the active slug (the verb handles that one) and minus
    /// a trigger whose scrub must dial the harness's OWN program on a machine where it does not
    /// look installed.
    ///
    /// A visitor rather than a returned list because the two breadth sources own their adapters
    /// differently (one builds them, one borrows them); every caller here only ever reads each
    /// adapter once, in order.
    fn for_each_other(&self, mut f: impl FnMut(&dyn TriggerAdapter)) {
        match &self.breadth {
            None => {}
            Some(Breadth::Machine { home, cfg, run }) => {
                let mut detected: Option<Vec<&'static str>> = None;
                for harness in registry::teardown_harnesses() {
                    if harness.slug == self.active.slug() {
                        continue;
                    }
                    let Some(adapter) = triggers::adapter_for_slug(harness.slug, home, *cfg, *run)
                    else {
                        continue;
                    };
                    if adapter.scrub_needs_live_harness() {
                        // Presence over the SAME teardown rows: a harness whose row a downloaded
                        // table dropped is still installed on this machine, and its scheduled job
                        // is still registered in its own program.
                        let live = detected.get_or_insert_with(|| {
                            registry::teardown_harnesses()
                                .iter()
                                .filter(|h| h.is_installed(home, None))
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
            Some(Breadth::Explicit { adapters, .. }) => {
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

    /// Visit every project-capable harness's trigger at ONE checkout, over the teardown table and
    /// REGARDLESS of the pick (an agent somebody picked and later left out may still hold a hook
    /// an earlier build wrote there). Nothing with no project ports.
    fn for_each_project(&self, root: &Path, mut f: impl FnMut(&dyn TriggerAdapter)) {
        let Some(ports) = self.project_ports() else {
            return;
        };
        let scope = TriggerScope::Project(root.to_path_buf());
        for harness in registry::teardown_harnesses() {
            if let Some(adapter) = triggers::adapter_for_slug_at(
                harness.slug,
                &scope,
                ports.home,
                ports.cfg,
                ports.run,
            ) {
                f(adapter.as_ref());
            }
        }
    }

    /// The hook files ONE checkout holds, across every project-capable harness in the teardown
    /// table and REGARDLESS of the pick — each named only while it is provably topos's right now.
    /// What `uninstall`'s describe lists for the checkout the command ran in, and exactly what
    /// [`Self::scrub_project`] then edits. Empty with no project ports. Sorted.
    pub(crate) fn project_hook_files(&self, root: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        self.for_each_project(root, |adapter| {
            out.extend(
                adapter
                    .artifacts()
                    .iter()
                    .filter_map(|artifact| artifact.path().map(Path::to_path_buf)),
            );
        });
        out.sort();
        out.dedup();
        out
    }

    /// Scrub the hooks ONE checkout holds (the `uninstall --yes` half of
    /// [`Self::project_hook_files`]): every project trigger that is provably topos's right now is
    /// removed surgically, and the rows report what happened. A file that holds no topos entry is
    /// never opened, so the receipt names exactly the files the describe named. Other checkouts
    /// keep theirs: a teardown reaches the one it stands in.
    pub(crate) fn scrub_project(&self, root: &Path) -> Vec<Scrubbed> {
        let mut out = Vec::new();
        self.for_each_project(root, |adapter| {
            if adapter.present() {
                out.push(Scrubbed::of(adapter, adapter.remove()));
            }
        });
        out
    }

    /// Scrub every OTHER supported agent's trigger (the uninstall half; the active one is the verb's
    /// own). Reports the rows that had something to say — a clean `Inactive` no-op on a filesystem
    /// artifact is noise on an uninstall receipt. An OUT-OF-PROCESS scrub is never filtered: it
    /// dialed the harness's own program, which is work that happened and carries no path to prove
    /// it, and the preview named that artifact — so the receipt answers for it either way.
    pub(crate) fn scrub_others(&self) -> Vec<Scrubbed> {
        let mut out = Vec::new();
        self.for_each_other(|adapter| {
            let reaches_out_of_process = adapter
                .artifacts()
                .iter()
                .any(TriggerArtifact::is_out_of_process);
            let removed = Scrubbed::of(adapter, adapter.remove());
            if reaches_out_of_process
                || removed.report.state != TriggerState::Inactive
                || removed.report.touched_path.is_some()
            {
                out.push(removed);
            }
        });
        out
    }

    /// Scrub the ACTIVE harness's trigger, carrying the same file disclosure the breadth rows do.
    pub(crate) fn scrub_active(&self) -> Scrubbed {
        Scrubbed::of(self.active, self.active.remove())
    }
}

/// One trigger scrub's outcome plus the config file it would have edited.
///
/// The path is NOT folded into [`TriggerReport::touched_path`], whose whole meaning is "the file
/// this run edited" — a degraded scrub edited nothing, and a report claiming otherwise would be
/// the lie the receipt is trying to stop telling. It is a second field because it answers a second
/// question: not what happened, but WHERE the thing that did not happen still stands.
pub(crate) struct Scrubbed {
    pub report: TriggerReport,
    /// The harness's config file, when its trigger lives in one — named whether or not the file
    /// is provably topos's right now, because a config topos could not read is exactly the one it
    /// cannot prove anything about. `None` = the trigger lives in the harness's own program.
    pub config_file: Option<String>,
}

impl Scrubbed {
    fn of(adapter: &dyn TriggerAdapter, report: TriggerReport) -> Self {
        Self {
            config_file: adapter
                .config_file()
                .map(|p| p.to_string_lossy().into_owned()),
            report,
        }
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

/// The hook-run evidence a probe row joins: when each slug's hook last ran, against now. An
/// EMPTY view (no document, or a caller with no store) answers `None` for everyone — absence of
/// evidence is never evidence of absence here.
pub(crate) struct EvidenceView<'e> {
    pub agents: &'e std::collections::BTreeMap<String, i64>,
    pub now_ms: i64,
}

impl EvidenceView<'_> {
    pub(crate) fn age_of(&self, slug: &str) -> Option<i64> {
        self.agents
            .get(slug)
            .map(|t| self.now_ms.saturating_sub(*t).max(0))
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
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.files.borrow_mut().remove(path);
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
            Self(dir.canonicalize().unwrap_or(dir))
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

    /// One user-level trigger whose root resolves plainly under the passed home (no env
    /// override), registered as a picked agent's would be.
    fn register(slug: &str, home: &Path, cfg: &dyn ConfigStore) -> TriggerReport {
        triggers::adapter_for_slug(slug, home, cfg, &NoBinary)
            .unwrap_or_else(|| panic!("{slug} has a trigger"))
            .install()
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

    #[test]
    fn scrub_others_reports_only_rows_with_something_to_say() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.0.join(".cursor")).unwrap();
        let cfg = MemConfig::default();
        // Register cursor first, then scrub everything: only cursor's removal touched a file.
        register("cursor", &home.0, &cfg);
        let active = claude_trigger(&home.0, &cfg);
        let out =
            Triggers::machine(active.as_ref(), home.0.clone(), &cfg, &NoBinary).scrub_others();
        assert!(
            out.iter().any(|s| s.report.agent == "cursor"
                && s.report.state == TriggerState::Inactive
                && s.report.touched_path.is_some()),
            "the registered agent's scrub is disclosed"
        );
        assert!(
            !out.iter().any(
                |s| s.report.touched_path.is_none() && s.report.state == TriggerState::Inactive
            ),
            "clean no-ops stay off the receipt"
        );
        assert!(
            !out.iter().any(|s| s.report.agent == "claude-code"),
            "the active harness is the verb's own scrub, never swept twice"
        );
        // Every FILE-backed row names the config it lives in — the path a degraded scrub points a
        // person at, which the report itself cannot carry (nothing was edited).
        assert!(
            out.iter()
                .find(|s| s.report.agent == "cursor")
                .is_some_and(|s| s.config_file.is_some()),
            "a file-backed trigger names its config"
        );
    }

    /// **The teardown sweep reads the TEARDOWN table, not the pick and not detection.** A
    /// downloaded registry may legitimately drop a row, and a pick may leave an agent out — but
    /// the hook that row was registered with is still in the agent's config, so every harness
    /// THIS BUILD can register has to stay reachable by the preview and the scrub. (Where no local
    /// table is installed the two sets coincide; what this pins is which one the sweep asks for,
    /// and the bundled floor is the half that cannot be taken away.)
    #[test]
    fn the_teardown_sweep_covers_every_harness_this_build_can_arm() {
        let home = TempHome::new();
        let cfg = MemConfig::default();
        let active = claude_trigger(&home.0, &cfg);
        let ports = Triggers::machine(active.as_ref(), home.0.clone(), &cfg, &NoBinary);
        let mut swept: Vec<&str> = Vec::new();
        ports.for_each_other(|adapter| swept.push(adapter.slug()));

        for row in registry::bundled_harnesses() {
            let Some(adapter) = triggers::adapter_for_slug(row.slug, &home.0, &cfg, &NoBinary)
            else {
                continue; // placement-only: no trigger, nothing to scrub
            };
            if row.slug == active.slug() || adapter.scrub_needs_live_harness() {
                continue; // the verb's own scrub / the one that needs the harness running
            }
            assert!(
                swept.contains(&row.slug),
                "{} is armable by this build and must stay reachable by its teardown: {swept:?}",
                row.slug
            );
        }
        assert!(
            !swept.contains(&active.slug()),
            "the active harness is the verb's own scrub, never swept twice"
        );
    }

    /// The disclosure contract: the preview names EVERY artifact the apply reaches — every class of
    /// artifact included. The preview walks the ACTIVE trigger plus exactly the set `scrub_others`
    /// walks, so a machine registered across several harnesses can never be told about one of them
    /// and have four more scrubbed — and a trigger with no file to name (OpenClaw's scheduler job)
    /// can never be scrubbed unannounced, which is exactly what a path-only preview did.
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
            "nothing registered → nothing owned on disk, the out-of-process attempt still disclosed"
        );

        // Register the whole machine, exactly as a wide pick does.
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
        // Every path named is under the temp home: this rig registers nothing on the real machine.
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
                .filter_map(|s| s.report.touched_path.clone())
                .map(PathBuf::from),
        );
        assert!(
            touched.len() >= 4,
            "the apply reaches every registered harness: {touched:?}"
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
            .find(|s| s.report.agent == "openclaw")
            .expect("a removed scheduler job is work that happened — it belongs on the receipt");
        assert_eq!(openclaw.report.state, TriggerState::Inactive);
        assert!(
            openclaw.report.touched_path.is_none(),
            "a scheduler job is no file — the row carries no path"
        );
    }

    /// The project half of the disclosure: a checkout's hook files, across the four
    /// project-capable harnesses, named only while they are topos's — and regardless of any pick,
    /// because a teardown lists what stands, not what somebody currently wants.
    #[test]
    fn project_hook_files_names_a_checkouts_hook_files_regardless_of_the_pick() {
        let home = TempHome::new();
        let root = TempHome::new();
        let cfg = crate::fs_seam::RealFs;
        let active = claude_trigger(&home.0, &cfg);
        let ports = Triggers::machine(active.as_ref(), home.0.clone(), &cfg, &NoBinary);
        assert!(ports.project_hook_files(&root.0).is_empty(), "nothing yet");

        let scope = TriggerScope::Project(root.0.clone());
        for slug in ["cursor", "opencode"] {
            triggers::adapter_for_slug_at(slug, &scope, &home.0, &cfg, &NoBinary)
                .unwrap()
                .install();
        }
        // A file at the codex path that is NOT ours is never named.
        std::fs::create_dir_all(root.0.join(".codex")).unwrap();
        std::fs::write(root.0.join(".codex").join("hooks.json"), b"{}\n").unwrap();
        assert_eq!(
            ports.project_hook_files(&root.0),
            vec![
                root.0.join(".cursor").join("hooks.json"),
                root.0.join(".opencode").join("plugin").join("topos.ts"),
            ]
        );
        // Nothing under ~ was named or written.
        assert!(!home.0.join(".cursor").exists());
        // And no machine ports (a rig's explicit set) name nothing.
        let none: Vec<Box<dyn TriggerAdapter>> = Vec::new();
        assert!(
            Triggers::machine_of(active.as_ref(), &none)
                .project_hook_files(&root.0)
                .is_empty()
        );
    }

    /// With no `$HOME` there is no breadth and no machine ports: the active trigger is the whole
    /// surface, both in the preview and in the scrub, and no project hook file is ever named.
    #[test]
    fn without_a_machine_root_the_active_trigger_is_the_whole_surface() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.0.join(".cursor")).unwrap();
        let cfg = MemConfig::default();
        // Register cursor so a breadth-aware sweep would have something to find.
        register("cursor", &home.0, &cfg);

        let active = claude_trigger(&home.0, &cfg);
        active.install();
        let ports = Triggers::active_only(active.as_ref());
        assert_eq!(ports.artifacts(), active.artifacts());
        assert!(ports.scrub_others().is_empty());
        assert!(ports.machine_ports().is_none());
        assert!(ports.project_hook_files(&home.0).is_empty());
    }
}
