//! The auto-update hooks that FOLLOW THE PICK — (un)installed for exactly the picked agents at
//! one scope, and probed the same way. Nothing else writes a hook.
//!
//! A hook is a per-agent artifact at ONE scope: the person's own config under `~` for a machine
//! pick, or the four project hook files (`.claude/settings.local.json`, `.cursor/hooks.json`,
//! `.codex/hooks.json`, `.opencode/plugin/topos.ts`) for a project pick. The scoped trigger
//! factory ([`triggers::adapter_for_slug_at`]) is the one place that machinery is named; this
//! module only walks the pick through it. A picked agent with no hook at the scope asked for is
//! reported as a [`HookAbsence`] (a project pick of an agent that reads no project hook), never
//! silently skipped — the receipt owes the person the line.
//!
//! Detection plays no part here: a picked agent is registered whether or not its detect dir
//! exists, and an agent nobody picked is never touched (the whole point).

use std::path::{Path, PathBuf};

use topos_harness::triggers::{self, TriggerScope};
use topos_harness::{CommandRunner, ConfigStore};
use topos_types::TriggerReport;
use topos_types::results::StatusTrigger;

use super::arm::EvidenceView;
use crate::ctx::Ctx;

/// A picked agent that has no hook at the scope asked for — the receipt line's data.
pub(crate) struct HookAbsence {
    /// The registry slug.
    pub agent: String,
    /// The one line the receipt prints for it.
    pub note: String,
}

/// The line for an agent whose harness reads no project hook.
pub(crate) fn absence_note(agent: &str) -> String {
    format!("no per-project auto-update for {agent}; `topos update`, or pick {agent} with -g")
}

/// Install the hook of every picked agent at `scope`. Best-effort per agent: a degraded row is
/// reported, never an aborted round. Returns the rows this run attempted, and the picked agents
/// that have no hook at this scope.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired by init -a and the agents verbs")
)]
pub(crate) fn install_for<'s>(
    home: &Path,
    scope: &TriggerScope,
    slugs: impl IntoIterator<Item = &'s str>,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
) -> (Vec<TriggerReport>, Vec<HookAbsence>) {
    let mut reports = Vec::new();
    let mut absent = Vec::new();
    for slug in slugs {
        match triggers::adapter_for_slug_at(slug, scope, home, cfg, run) {
            Some(adapter) => reports.push(adapter.install()),
            None => absent.extend(absence_at(slug, scope)),
        }
    }
    (reports, absent)
}

/// Remove the hook of every named agent at `scope` — what `agents remove` runs for the agents
/// leaving the pick. Surgical and idempotent per adapter (a foreign artifact is never touched).
#[cfg_attr(not(test), expect(dead_code, reason = "wired by the agents verbs"))]
pub(crate) fn remove_for<'s>(
    home: &Path,
    scope: &TriggerScope,
    slugs: impl IntoIterator<Item = &'s str>,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
) -> Vec<TriggerReport> {
    slugs
        .into_iter()
        .filter_map(|slug| triggers::adapter_for_slug_at(slug, scope, home, cfg, run))
        .map(|adapter| adapter.remove())
        .collect()
}

/// Probe the hook of every picked agent at `scope`, READ-ONLY — the `status` half: the same
/// adapters, only their provable-presence probes (nothing is armed, repaired, or scrubbed). An
/// adapter whose trigger lives outside the filesystem answers `armed: None` with its own reason,
/// because proving it would mean running the harness. A picked agent with no hook at this scope
/// answers `armed: None` with the [`absence_note`] in a project, and no row at all under `~`
/// (a placement-only harness has no trigger surface anywhere).
pub(crate) fn probe<'s>(
    home: &Path,
    scope: &TriggerScope,
    slugs: impl IntoIterator<Item = &'s str>,
    cfg: &dyn ConfigStore,
    run: &dyn CommandRunner,
    evidence: &EvidenceView<'_>,
) -> Vec<StatusTrigger> {
    let mut out = Vec::new();
    for slug in slugs {
        let Some(adapter) = triggers::adapter_for_slug_at(slug, scope, home, cfg, run) else {
            out.extend(absence_at(slug, scope).map(|absence| StatusTrigger {
                agent: absence.agent,
                armed: None,
                note: Some(absence.note),
                last_run_age_ms: None,
            }));
            continue;
        };
        let (armed, note) = match adapter.offline_probe_refusal() {
            Some(why) => (None, Some(why.to_owned())),
            None => (
                Some(adapter.present()),
                adapter.pending_step().map(str::to_owned),
            ),
        };
        out.push(StatusTrigger {
            agent: slug.to_owned(),
            armed,
            note,
            last_run_age_ms: evidence.age_of(slug),
        });
    }
    out
}

/// The probe over the EFFECTIVE PICK for the scope `project_dir` stands for (`None` = the
/// machine scope: the machine pick under `~`; a checkout: its effective pick, probed at the
/// project's own hook files). Empty with no machine ports (no `$HOME`, or a rig with an explicit
/// trigger set), with no pick, or with a pick that cannot be read — nothing probes detection.
pub(crate) fn probe_effective(
    ctx: &Ctx<'_>,
    project_dir: Option<&Path>,
    evidence: &EvidenceView<'_>,
) -> Vec<StatusTrigger> {
    let Some(ports) = ctx.triggers.machine_ports() else {
        return Vec::new();
    };
    let picked: Vec<&'static str> = crate::agents_pick::picked_harnesses(ctx, project_dir)
        .iter()
        .map(|h| h.slug)
        .collect();
    let scope = match project_dir {
        Some(dir) => TriggerScope::Project(dir.to_path_buf()),
        None => TriggerScope::User,
    };
    probe(
        ports.home,
        &scope,
        picked.iter().copied(),
        ports.cfg,
        ports.run,
        evidence,
    )
}

/// The checkout the working directory stands in — the nearest `topos.toml` dir below `$HOME` —
/// or `None` outside any project. What the ambient verbs (`status`, `auth status`) probe at.
pub(crate) fn cwd_project(ctx: &Ctx<'_>) -> Option<PathBuf> {
    let roots = ctx.roots.as_ref()?;
    let cwd = roots.cwd.as_deref()?;
    crate::manifest::scopes::nearest_manifest_dir(ctx.fs, cwd, Some(&roots.home))
}

/// A project pick names an agent whose harness reads no project hook: the line for it. Under
/// `~` a trigger-less harness has nothing to register anywhere, and nothing to say.
fn absence_at(slug: &str, scope: &TriggerScope) -> Option<HookAbsence> {
    match scope {
        TriggerScope::Project(_) => Some(HookAbsence {
            agent: slug.to_owned(),
            note: absence_note(slug),
        }),
        TriggerScope::User => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_harness::RunOutput;
    use topos_types::{CurrencyKind, TriggerState};

    /// A path-keyed in-memory [`ConfigStore`] (a round may write several agents' configs).
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

    /// A self-cleaning temp dir (RAII), canonicalized so the containment rail's paths compare
    /// plainly.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-hooks-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let dir = dir.canonicalize().unwrap_or(dir);
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const NO_EVIDENCE: EvidenceView<'static> = EvidenceView {
        agents: &std::collections::BTreeMap::new(),
        now_ms: 0,
    };

    /// The pick, and only the pick. Three agents are installed on the machine (their detect dirs
    /// exist); two are picked; the third's config is never opened, and a placement-only pick
    /// (augment) earns neither a hook nor a row. The user-level rig names only harnesses whose
    /// roots resolve plainly under the passed home — the factory reads `$CLAUDE_CONFIG_DIR` and
    /// `$CODEX_HOME` for the two that do not, which on a developer machine point at real config.
    #[test]
    fn install_for_touches_only_the_picked() {
        let home = Scratch::new("user");
        for d in [".cursor", ".cline", ".gemini", ".augment"] {
            std::fs::create_dir_all(home.0.join(d)).unwrap();
        }
        let cfg = MemConfig::default();
        let (reports, absent) = install_for(
            &home.0,
            &TriggerScope::User,
            ["cursor", "cline", "augment"],
            &cfg,
            &NoBinary,
        );
        let by = |slug: &str| reports.iter().find(|r| r.agent == slug);
        assert_eq!(
            by("cursor").expect("cursor registered").state,
            TriggerState::Active
        );
        assert_eq!(
            by("cline").expect("cline registered").state,
            TriggerState::Active
        );
        assert!(
            by("augment").is_none(),
            "a placement-only harness has no trigger row"
        );
        assert!(
            by("gemini-cli").is_none(),
            "an installed agent nobody picked is not a row"
        );
        assert!(
            absent.is_empty(),
            "under ~ a trigger-less harness has nothing to say"
        );
        let files = cfg.files.borrow();
        assert!(
            files
                .keys()
                .all(|p| p.starts_with(home.0.join(".cursor"))
                    || p.starts_with(home.0.join(".cline"))),
            "only the picked agents' configs were written: {:?}",
            files.keys().collect::<Vec<_>>()
        );
        assert!(
            !files.keys().any(|p| p.starts_with(home.0.join(".gemini"))),
            "gemini-cli's config is never opened"
        );
    }

    /// The status probe is READ-ONLY and honest per picked agent: an unregistered cursor probes
    /// `false`, a registered one `true`; OpenClaw's live-only probe stays unknown with its reason;
    /// a placement-only pick has no row under `~`.
    #[test]
    fn probe_is_read_only_and_honest_per_picked_agent() {
        let home = Scratch::new("probe");
        let cfg = MemConfig::default();
        let pick = ["cursor", "openclaw", "augment"];
        let out = probe(
            &home.0,
            &TriggerScope::User,
            pick,
            &cfg,
            &NoBinary,
            &NO_EVIDENCE,
        );
        let by = |slug: &str| out.iter().find(|r| r.agent == slug);
        assert_eq!(by("cursor").expect("cursor row").armed, Some(false));
        let openclaw = by("openclaw").expect("openclaw row");
        assert_eq!(openclaw.armed, None, "a live-only probe stays unknown");
        assert!(openclaw.note.is_some(), "the unknown names its reason");
        assert!(by("augment").is_none(), "placement-only ⇒ no trigger row");
        assert!(cfg.files.borrow().is_empty(), "the probe writes nothing");

        install_for(&home.0, &TriggerScope::User, ["cursor"], &cfg, &NoBinary);
        let out = probe(
            &home.0,
            &TriggerScope::User,
            ["cursor"],
            &cfg,
            &NoBinary,
            &NO_EVIDENCE,
        );
        assert_eq!(out[0].armed, Some(true));
    }

    /// A picked OpenClaw rides its own adapter: no `openclaw` binary in the test runner, so the
    /// cron cannot be registered — Degraded + the explicit-pull floor, the adapter's own rule.
    #[test]
    fn a_picked_openclaw_rides_its_own_adapter_and_degrades_honestly() {
        let home = Scratch::new("openclaw");
        let cfg = MemConfig::default();
        let (reports, _) = install_for(&home.0, &TriggerScope::User, ["openclaw"], &cfg, &NoBinary);
        let oc = reports
            .iter()
            .find(|r| r.agent == "openclaw")
            .expect("openclaw attempted");
        assert_eq!(oc.state, TriggerState::Degraded);
        assert_eq!(oc.currency_kind, CurrencyKind::ExplicitPullOnly);
    }

    /// A project pick: the four project-capable agents get their hook file inside the checkout
    /// and nothing under `~`; every other picked agent is named as having no project hook, in the
    /// spec's words; `remove_for` takes the four back out; the probe follows.
    #[test]
    fn a_project_pick_installs_the_four_project_hooks_and_names_the_rest() {
        let home = Scratch::new("proj-home");
        let root = Scratch::new("proj-root");
        let cfg = RealFs;
        let scope = TriggerScope::Project(root.0.clone());
        let pick = [
            "claude-code",
            "cursor",
            "codex",
            "opencode",
            "gemini-cli",
            "zed",
        ];
        let (reports, absent) = install_for(&home.0, &scope, pick, &cfg, &NoBinary);
        let agents: Vec<&str> = reports.iter().map(|r| r.agent.as_str()).collect();
        assert_eq!(agents, ["claude-code", "cursor", "codex", "opencode"]);
        for file in [
            root.0.join(".claude").join("settings.local.json"),
            root.0.join(".cursor").join("hooks.json"),
            root.0.join(".codex").join("hooks.json"),
            root.0.join(".opencode").join("plugin").join("topos.ts"),
        ] {
            assert!(file.is_file(), "{file:?} landed");
        }
        assert!(
            std::fs::read_dir(&home.0).unwrap().next().is_none(),
            "a project pick writes nothing under ~"
        );
        let notes: Vec<(&str, &str)> = absent
            .iter()
            .map(|a| (a.agent.as_str(), a.note.as_str()))
            .collect();
        assert_eq!(
            notes,
            [
                (
                    "gemini-cli",
                    "no per-project auto-update for gemini-cli; `topos update`, or pick gemini-cli with -g"
                ),
                (
                    "zed",
                    "no per-project auto-update for zed; `topos update`, or pick zed with -g"
                ),
            ]
        );

        let probed = probe(&home.0, &scope, pick, &cfg, &NoBinary, &NO_EVIDENCE);
        let by = |slug: &str| probed.iter().find(|r| r.agent == slug).expect(slug);
        assert_eq!(by("claude-code").armed, Some(true));
        assert_eq!(by("cursor").armed, Some(true));
        assert_eq!(by("codex").armed, Some(true));
        assert_eq!(by("opencode").armed, Some(true));
        assert_eq!(by("gemini-cli").armed, None);
        assert_eq!(
            by("gemini-cli").note.as_deref(),
            Some(absence_note("gemini-cli").as_str())
        );
        assert_eq!(by("zed").armed, None);

        let removed = remove_for(&home.0, &scope, pick, &cfg, &NoBinary);
        assert_eq!(removed.len(), 4, "only the four had anything to remove");
        assert!(
            removed.iter().all(|r| r.state == TriggerState::Inactive),
            "{removed:?}"
        );
        assert!(
            !root
                .0
                .join(".opencode")
                .join("plugin")
                .join("topos.ts")
                .exists()
        );
        let probed = probe(&home.0, &scope, pick, &cfg, &NoBinary, &NO_EVIDENCE);
        assert!(
            probed
                .iter()
                .filter(|r| r.armed.is_some())
                .all(|r| r.armed == Some(false)),
            "{probed:?}"
        );
    }

    /// No pick, no hook: an empty pick installs nothing and probes nothing, whatever is
    /// installed on the machine.
    #[test]
    fn an_empty_pick_installs_nothing() {
        let home = Scratch::new("empty");
        std::fs::create_dir_all(home.0.join(".cursor")).unwrap();
        let cfg = MemConfig::default();
        let (reports, absent) = install_for(
            &home.0,
            &TriggerScope::User,
            std::iter::empty::<&str>(),
            &cfg,
            &NoBinary,
        );
        assert!(reports.is_empty() && absent.is_empty());
        assert!(cfg.files.borrow().is_empty());
        assert!(
            probe(
                &home.0,
                &TriggerScope::User,
                std::iter::empty::<&str>(),
                &cfg,
                &NoBinary,
                &NO_EVIDENCE
            )
            .is_empty()
        );
    }

    /// The effective-pick probe reads the pick at the scope asked: the machine pick under `~`
    /// for the machine scope, a project's own pick at the project's hook files inside it — and
    /// nothing at all through a rig with no machine ports.
    #[test]
    fn probe_effective_reads_the_pick_at_the_scope_asked() {
        use crate::ctx::AgentRoots;
        use crate::ids::test_sources::{FixedClock, SeqIds};
        use crate::plane::{InertFollow, InertPlane};
        use crate::test_support::MockHarness;

        let home = Scratch::new("eff-home");
        let project = Scratch::new("eff-proj");
        std::fs::write(project.0.join("topos.toml"), b"").unwrap();
        let layout = crate::sidecar::Layout::new(&home.0.join(".topos"));
        crate::agents_pick::write_pick(&crate::agents_pick::machine_path(&layout), &["cursor"]);
        crate::agents_pick::write_pick(
            &crate::agents_pick::project_path(&project.0),
            &["cline", "cursor"],
        );
        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = MockHarness::joining("");
        let active = triggers::claude_code_at(home.0.join(".claude"), &fs);
        #[allow(clippy::too_many_arguments)] // A composition root's rig: every port is passed.
        fn ctx_over<'a>(
            fs: &'a RealFs,
            ids: &'a SeqIds,
            clock: &'a FixedClock,
            harness: &'a MockHarness,
            layout: &crate::sidecar::Layout,
            home: &Path,
            project: &Path,
            triggers: super::super::Triggers<'a>,
        ) -> Ctx<'a> {
            Ctx {
                progress: crate::progress::silent(),
                fs,
                ids,
                clock,
                device_id: "d_test".into(),
                layout: layout.clone(),
                harness,
                triggers,
                plane: &InertPlane,
                follow: &InertFollow,
                roots: Some(AgentRoots {
                    home: home.to_path_buf(),
                    cwd: Some(project.to_path_buf()),
                }),
            }
        }
        let machine = ctx_over(
            &fs,
            &ids,
            &clock,
            &harness,
            &layout,
            &home.0,
            &project.0,
            super::super::Triggers::machine(active.as_ref(), home.0.clone(), &fs, &NoBinary),
        );
        // The machine scope: cursor under ~, not registered.
        let rows = probe_effective(&machine, None, &NO_EVIDENCE);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].agent.as_str(), rows[0].armed),
            ("cursor", Some(false))
        );
        // The project: its own pick, at its own hook files. cline reads no project hook and
        // says so; cursor's project hook is probed inside the checkout.
        triggers::adapter_for_slug_at(
            "cursor",
            &TriggerScope::Project(project.0.clone()),
            &home.0,
            &fs,
            &NoBinary,
        )
        .unwrap()
        .install();
        assert_eq!(cwd_project(&machine).as_deref(), Some(project.0.as_path()));
        let rows = probe_effective(&machine, Some(&project.0), &NO_EVIDENCE);
        let by = |slug: &str| rows.iter().find(|r| r.agent == slug).expect(slug);
        assert_eq!(by("cline").armed, None);
        assert_eq!(
            by("cline").note.as_deref(),
            Some(absence_note("cline").as_str())
        );
        assert_eq!(by("cursor").armed, Some(true));
        assert!(
            !home.0.join(".cursor").exists(),
            "nothing under ~ was probed into being"
        );
        // No machine ports: nothing to probe with.
        let rootless = ctx_over(
            &fs,
            &ids,
            &clock,
            &harness,
            &layout,
            &home.0,
            &project.0,
            super::super::Triggers::active_only(&super::super::INERT_TRIGGER),
        );
        assert!(probe_effective(&rootless, None, &NO_EVIDENCE).is_empty());
    }
}
