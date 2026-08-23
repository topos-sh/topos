//! The `OpenClaw` adapter — BOTH of the crate's ports on one type: [`HarnessAdapter`] (discovery +
//! byte-exact placement targeting) and [`TriggerAdapter`] (the idempotent **silent-cron auto-update
//! trigger** registered through OpenClaw's own CLI). The two halves share only the resolved home.
//!
//! OpenClaw reads native AgentSkills-spec `SKILL.md` bundles from `~/.openclaw/skills` (probed
//! live against openclaw@2026.7.1 in a container: recognized offline, ungated, source
//! `openclaw-managed`; the skills watcher defaults ON with a 250 ms debounce, so changed bytes
//! surface on the next agent turn mid-session). Placement therefore needs NO injection surface —
//! the old topos-owned bootstrap-inject plugin (a context file registered in `openclaw.json`) is
//! RETIRED: this adapter never writes it, and never reads or edits `openclaw.json` at all. On
//! current builds that config is JSON5 and the old top-level registration key no longer exists,
//! which independently confirms the retirement.
//!
//! **The auto-update trigger is a silent OpenClaw cron job** (probed live): `openclaw cron add
//! --command <shell> --no-deliver --declaration-key <key> --json` registers a deterministic,
//! model-free shell job persisted in OpenClaw's own SQLite, idempotent by declaration key (a
//! re-add answers `created:false`, same job — the key IS the ownership marker), firing on a
//! 1-minute cadence. The registered shell line is the same guarded sweep the Claude Code hook
//! runs — `topos install --quiet` behind a `command -v` guard with an exit-0 tail (the job runs
//! via `sh -lc`, so the guard works; a cleanly-failing job never trips OpenClaw's error
//! counters, and an orphaned job after a topos uninstall no-ops silently). The sweep self-
//! throttles client-side (TTL + single-flight), so the 1-minute cadence is cheap.
//!
//! **Honest degrade (probed constraints):** `cron add` requires a RUNNING gateway — it fails fast
//! when the gateway is down and never queues; the job stops firing while the gateway/daemon is
//! down and resumes when it returns. So [`TriggerState::Active`] (kind
//! [`CurrencyKind::Scheduled`]) is claimed ONLY when the registration round-trip succeeded — the
//! gateway answered — and every other outcome (no `openclaw` binary, a down gateway, a CLI error)
//! degrades plainly to the [`CurrencyKind::ExplicitPullOnly`] floor with nothing invented. Remove
//! resolves the job id from `cron list --json` by declaration key (`rm` is id-only) and treats
//! missing-as-clean; a down gateway at remove time is `Degraded` — the job survives in OpenClaw's
//! store, disclosed, never silently orphaned (and its guarded command no-ops once topos is gone).
//!
//! Content-blind, like the reference: it reads skill *directories* only to confirm a `SKILL.md`
//! exists (never the bytes, never the frontmatter). Its trigger surface is OpenClaw's own
//! scheduler, driven through the injected [`CommandRunner`] port (argv-only — no shell strings
//! are composed here); it writes NO file at all — not a config, never a skill dir, never a
//! foreign byte.

use std::path::PathBuf;

use serde_json::Value;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::triggers::{TriggerAdapter, TriggerArtifact};
use crate::{CommandRunner, DiscoveredPlacement, HarnessAdapter, PlacementNaming, PlacementTarget};

/// The user-scope layer label recorded for a discovered/placed OpenClaw skill (the resolved layer;
/// a project/enterprise layer stays representable later — `DiscoveredPlacement.layer` is already
/// `Option<String>`).
const LAYER_USER: &str = "user";

/// The structured marker identity reported in [`TriggerReport::marker_id`] — AND the cron job's
/// `--declaration-key`, which makes the registration idempotent (probed: a re-add with the same
/// key answers `created:false`, the same job id, never a duplicate) and is the ownership marker
/// the remove path resolves the job by. Schema 2 = the silent-cron trigger; schema 1 was the
/// retired bootstrap-inject surface.
const MARKER_ID: &str = "topos:openclaw:currency:2";

/// The OpenClaw management CLI, resolved from `PATH` by the injected runner.
const OPENCLAW_BIN: &str = "openclaw";

/// How this harness is NAMED to a person — the disclosure row for the scheduler registration is
/// the only artifact with no path to print, so it prints this instead. It is the registry row's
/// own display name (a test below holds the two in agreement), never a second spelling.
const DISPLAY_NAME: &str = "OpenClaw";

/// The cron job's human-facing name (shows in `openclaw cron list`; identity rides the
/// declaration key, never this label).
const CRON_NAME: &str = "topos-currency";

/// The cadence (probed: sub-minute is allowed; one minute is the deliberate floor — the client's
/// own TTL gate throttles the sweeps this fires).
const CRON_EVERY: &str = "1m";

/// The job's shell payload (OpenClaw runs it via `sh -lc`). The `command -v` guard + exit-0 tail
/// mirror the Claude Code hook line: a machine that lost the `topos` binary (an uninstall; the
/// job surviving in OpenClaw's store) no-ops CLEANLY, so the job never accumulates error state.
const CRON_COMMAND: &str =
    "command -v topos >/dev/null 2>&1 && topos install --quiet --from openclaw || true";

/// The `OpenClaw` adapter — [`HarnessAdapter`] + [`TriggerAdapter`]. Holds the resolved config home
/// and the [`CommandRunner`] port (the `openclaw cron` CLI) — both injected, so tests point the home
/// at a temp dir and drive a fake CLI. There is no config seam: this adapter writes no file.
pub struct OpenClaw<'a> {
    /// `$HOME/.openclaw` — injected in tests; see [`OpenClaw::resolve_home`].
    home: PathBuf,
    cli: &'a dyn CommandRunner,
}

impl std::fmt::Debug for OpenClaw<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenClaw")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

/// How the cron-removal round-trip ended.
enum CronRemoval {
    /// Our job was found and removed.
    Removed,
    /// The list parsed and no job carries our declaration key — provably already clean.
    NotPresent,
    /// The binary is absent / the CLI errored / the gateway is down / the list did not parse —
    /// removal was NOT verified: a persisted job may survive and resume when the gateway returns.
    /// Never claimed clean (the surviving job's guarded command at least no-ops once topos is
    /// gone).
    Unavailable,
}

/// What a `cron list --json` stdout proved.
enum ListRead {
    /// The list parsed; our job's id if present.
    Jobs(Option<String>),
    /// The output did not parse into the probed shape — it proves NOTHING (never "not present").
    Unreadable,
}

impl<'a> OpenClaw<'a> {
    /// Construct over an explicit config home + the process runner. Production passes
    /// [`OpenClaw::resolve_home`] and the CLI crate's real runner; tests pass a temp dir and a fake
    /// so a real `~/.openclaw` (or a real `openclaw` binary) is never touched. There is no config
    /// seam here: this adapter drives the `openclaw` CLI and neither reads nor writes a file.
    #[must_use]
    pub fn new(home: PathBuf, cli: &'a dyn CommandRunner) -> Self {
        Self { home, cli }
    }

    /// Resolve OpenClaw's config home: `$HOME/.openclaw` (falling back to `./.openclaw` if `$HOME`
    /// is unset).
    #[must_use]
    pub fn resolve_home() -> PathBuf {
        crate::registry::real_home().join(".openclaw")
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    /// Register (or re-affirm) the silent cron job. `true` ONLY when the round-trip succeeded —
    /// which requires a reachable gateway, so success IS the gateway-alive evidence.
    fn register_cron(&self) -> bool {
        matches!(
            self.cli.run(
                OPENCLAW_BIN,
                &[
                    "cron",
                    "add",
                    "--name",
                    CRON_NAME,
                    "--every",
                    CRON_EVERY,
                    "--command",
                    CRON_COMMAND,
                    "--no-deliver",
                    "--declaration-key",
                    MARKER_ID,
                    "--json",
                ],
            ),
            Ok(out) if out.success
        )
    }

    /// Remove our cron job: resolve the id by declaration key from `cron list --json` (probed:
    /// `rm` takes an id only, and removing a missing id errors — so the list probe comes first).
    fn remove_cron(&self) -> CronRemoval {
        let list = match self.cli.run(OPENCLAW_BIN, &["cron", "list", "--json"]) {
            Err(_) => return CronRemoval::Unavailable, // binary absent, or any spawn failure
            Ok(out) if !out.success => return CronRemoval::Unavailable,
            Ok(out) => out,
        };
        let id = match read_jobs(&list.stdout) {
            ListRead::Jobs(Some(id)) => id,
            ListRead::Jobs(None) => return CronRemoval::NotPresent,
            // A zero-exit list whose output we cannot read proves nothing about the job.
            ListRead::Unreadable => return CronRemoval::Unavailable,
        };
        match self.cli.run(OPENCLAW_BIN, &["cron", "rm", &id]) {
            Ok(out) if out.success => CronRemoval::Removed,
            _ => CronRemoval::Unavailable,
        }
    }

    /// Honest labeling through the crate's ONE constructor: `Scheduled` only for a verified
    /// registration round-trip; every other state advertises just the guaranteed floor — an
    /// explicit `topos update`. The trigger touches no file, so no path is ever disclosed.
    fn report(&self, state: TriggerState) -> TriggerReport {
        crate::trigger_report(
            HarnessId::OpenClaw.slug(),
            CurrencyKind::Scheduled,
            state,
            None,
            MARKER_ID,
            None,
        )
    }
}

/// Parse `cron list --json` (probed shape: `{"jobs": [{"id": …, "declarationKey": …, …}, …]}`).
/// Output that does not parse into that shape is [`ListRead::Unreadable`] — it proves NOTHING (a
/// job may exist behind it), so callers never fold it into "not present".
fn read_jobs(stdout: &str) -> ListRead {
    let Ok(root) = serde_json::from_str::<Value>(stdout) else {
        return ListRead::Unreadable;
    };
    let Some(jobs) = root.get("jobs").and_then(Value::as_array) else {
        return ListRead::Unreadable;
    };
    ListRead::Jobs(jobs.iter().find_map(|job| {
        (job.get("declarationKey").and_then(Value::as_str) == Some(MARKER_ID))
            .then(|| job.get("id").and_then(Value::as_str).map(str::to_owned))
            .flatten()
    }))
}

impl HarnessAdapter for OpenClaw<'_> {
    fn id(&self) -> HarnessId {
        HarnessId::OpenClaw
    }

    /// The ONE skill-directory probe ([`crate::registry::discover_skill_dirs`]) over this harness's
    /// default-watched skills root — sorted, dot-entries skipped, a root `SKILL.md` the only
    /// confirmation.
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        crate::registry::discover_skill_dirs(&self.skills_dir())
            .into_iter()
            .map(|path| DiscoveredPlacement {
                path,
                layer: Some(LAYER_USER.to_owned()),
            })
            .collect()
    }

    fn placement_for(
        &self,
        skill_id: &str,
        _naming: PlacementNaming<'_>,
        discovered: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        match discovered {
            Some(d) => PlacementTarget {
                dir: d.path.clone(),
            },
            // No-discovered default: `<home>/skills/<skill_id>` — the resolved user layer. Probed:
            // this root is recognized offline, ungated, and watched by default (250 ms debounce),
            // so placed bytes surface without any injection surface.
            None => PlacementTarget {
                dir: self.skills_dir().join(skill_id),
            },
        }
    }
}

impl TriggerAdapter for OpenClaw<'_> {
    fn slug(&self) -> &'static str {
        HarnessId::OpenClaw.slug()
    }

    fn install(&self) -> TriggerReport {
        if self.register_cron() {
            self.report(TriggerState::Active)
        } else {
            // No binary / gateway down / CLI error: nothing is registered, nothing fires on its
            // own — the floor is an explicit `topos update` (the watcher then surfaces the bytes).
            self.report(TriggerState::Degraded)
        }
    }

    fn remove(&self) -> TriggerReport {
        match self.remove_cron() {
            CronRemoval::Removed | CronRemoval::NotPresent => self.report(TriggerState::Inactive),
            // Removal was NOT verified (no binary / gateway down / unreadable list): a persisted
            // job may survive and resume — disclosed as Degraded, never claimed clean.
            CronRemoval::Unavailable => self.report(TriggerState::Degraded),
        }
    }

    /// The hook-health probe: our trigger lives in OpenClaw's SCHEDULER, not the filesystem, so a
    /// footprint-based answer would call a healthy cron "not installed". A live `cron list` proves
    /// presence; anything unprovable (no binary, a down gateway, unreadable output) answers
    /// `false` — health is never claimed on faith.
    fn present(&self) -> bool {
        match self.cli.run(OPENCLAW_BIN, &["cron", "list", "--json"]) {
            Ok(out) if out.success => matches!(read_jobs(&out.stdout), ListRead::Jobs(Some(_))),
            _ => false,
        }
    }

    fn artifacts(&self) -> Vec<TriggerArtifact> {
        // topos owns NO file under the OpenClaw home: the trigger is OpenClaw-owned SCHEDULER
        // state, so it has no path to disclose — and it is named UNCONDITIONALLY, because a scrub
        // of this adapter always dials the scheduler. Probing it here would mean running the
        // harness; the row's own wording carries that ("if registered"), so the preview promises the
        // attempt and never a presence it did not check.
        vec![TriggerArtifact::OutOfProcess {
            harness: DISPLAY_NAME,
        }]
    }

    /// The trigger lives in OpenClaw's SCHEDULER, not the filesystem: proving it there means
    /// running `openclaw cron list`, which a read-only status must not do.
    fn offline_probe_refusal(&self) -> Option<&'static str> {
        Some("presence needs a live scheduler query")
    }

    /// The scrub must reach OUT of the filesystem, into OpenClaw's own program — so it is attempted
    /// only where the harness still looks installed.
    fn scrub_needs_live_harness(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunOutput;
    use std::cell::RefCell;
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// How the fake OpenClaw CLI behaves.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CliMode {
        /// The gateway answers: add/list/rm behave like the probed 2026.7.1 build.
        Healthy,
        /// The binary exists but every invocation fails (gateway down — `cron add` fails fast,
        /// `cron list` errors).
        GatewayDown,
        /// The binary is absent from PATH (spawn-level `NotFound`).
        NoBinary,
        /// Every invocation exits zero but emits output the probed shape does not cover.
        UnreadableList,
    }

    /// The fake `openclaw` CLI: records every argv, simulates the probed cron semantics
    /// (declaration-key idempotence; list/rm by id).
    struct FakeCli {
        mode: CliMode,
        /// Registered jobs as (id, declaration_key).
        jobs: RefCell<Vec<(String, String)>>,
        calls: RefCell<Vec<Vec<String>>>,
    }
    impl FakeCli {
        fn new(mode: CliMode) -> Self {
            Self {
                mode,
                jobs: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn with_job(mode: CliMode, key: &str) -> Self {
            let me = Self::new(mode);
            me.jobs
                .borrow_mut()
                .push(("job-1".to_owned(), key.to_owned()));
            me
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
        fn keys(&self) -> Vec<String> {
            self.jobs.borrow().iter().map(|(_, k)| k.clone()).collect()
        }
    }
    impl CommandRunner for FakeCli {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<RunOutput> {
            assert_eq!(program, OPENCLAW_BIN, "only the openclaw CLI is driven");
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| (*s).to_owned()).collect());
            match self.mode {
                CliMode::NoBinary => Err(io::Error::new(io::ErrorKind::NotFound, "no openclaw")),
                CliMode::GatewayDown => Ok(RunOutput {
                    success: false,
                    stdout: "gateway closed".to_owned(),
                }),
                CliMode::UnreadableList => Ok(RunOutput {
                    success: true,
                    stdout: "a future build's incompatible output".to_owned(),
                }),
                CliMode::Healthy => {
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
                                .expect("add carries a declaration key");
                            let mut jobs = self.jobs.borrow_mut();
                            if jobs.iter().any(|(_, k)| *k == key) {
                                ok("{\"created\":false,\"updated\":false}".to_owned())
                            } else {
                                let id = format!("job-{}", jobs.len() + 1);
                                jobs.push((id, key));
                                ok("{\"created\":true}".to_owned())
                            }
                        }
                        ["cron", "list", "--json"] => {
                            let jobs: Vec<Value> = self
                                .jobs
                                .borrow()
                                .iter()
                                .map(|(id, key)| {
                                    serde_json::json!({"id": id, "declarationKey": key})
                                })
                                .collect();
                            ok(serde_json::json!({ "jobs": jobs }).to_string())
                        }
                        ["cron", "rm", id] => {
                            let mut jobs = self.jobs.borrow_mut();
                            let before = jobs.len();
                            jobs.retain(|(jid, _)| jid != id);
                            if jobs.len() < before {
                                ok("{\"ok\":true,\"removed\":true}".to_owned())
                            } else {
                                Ok(RunOutput {
                                    success: false,
                                    stdout: "id not found".to_owned(),
                                })
                            }
                        }
                        other => panic!("unexpected argv: {other:?}"),
                    }
                }
            }
        }
    }

    /// A self-cleaning temp dir (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-oc-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn skill(&self, name: &str) {
            let d = self.0.join("skills").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), b"---\nname: x\n---\n# x\n").unwrap();
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn install_registers_the_silent_cron_job_byte_exact() {
        let cli = FakeCli::new(CliMode::Healthy);
        let report = OpenClaw::new(PathBuf::from("/h"), &cli).install();

        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.agent, "openclaw");
        assert_eq!(report.currency_kind, CurrencyKind::Scheduled);
        assert_eq!(report.marker_id, MARKER_ID);
        assert!(report.touched_path.is_none(), "the trigger touches no file");

        // The exact argv (the declaration key is the idempotency marker; --no-deliver keeps the
        // job silent; the payload is the guarded sweep).
        assert_eq!(
            cli.calls(),
            vec![vec![
                "cron".to_owned(),
                "add".to_owned(),
                "--name".to_owned(),
                CRON_NAME.to_owned(),
                "--every".to_owned(),
                CRON_EVERY.to_owned(),
                "--command".to_owned(),
                CRON_COMMAND.to_owned(),
                "--no-deliver".to_owned(),
                "--declaration-key".to_owned(),
                MARKER_ID.to_owned(),
                "--json".to_owned(),
            ]],
        );
        assert_eq!(cli.keys(), vec![MARKER_ID.to_owned()]);
    }

    #[test]
    fn install_is_idempotent_by_declaration_key() {
        let cli = FakeCli::new(CliMode::Healthy);
        let a = OpenClaw::new(PathBuf::from("/h"), &cli);
        a.install();
        let report = a.install();
        assert_eq!(
            report.state,
            TriggerState::Active,
            "created:false is still registered"
        );
        assert_eq!(cli.keys().len(), 1, "never a duplicate job");
    }

    #[test]
    fn install_degrades_honestly_without_the_binary_or_gateway() {
        for mode in [CliMode::NoBinary, CliMode::GatewayDown] {
            let cli = FakeCli::new(mode);
            let report = OpenClaw::new(PathBuf::from("/h"), &cli).install();
            assert_eq!(report.state, TriggerState::Degraded, "{mode:?}");
            assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
            assert!(cli.keys().is_empty(), "nothing was registered");
        }
    }

    #[test]
    fn remove_unregisters_by_declaration_key() {
        let cli = FakeCli::with_job(CliMode::Healthy, MARKER_ID);
        let report = OpenClaw::new(PathBuf::from("/h"), &cli).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
        assert!(cli.keys().is_empty(), "our job was removed");
        let calls = cli.calls();
        assert_eq!(calls[0][..2], ["cron".to_owned(), "list".to_owned()]);
        assert_eq!(
            calls[1],
            vec!["cron".to_owned(), "rm".to_owned(), "job-1".to_owned()],
            "rm is id-only, resolved from the list"
        );
    }

    #[test]
    fn remove_treats_missing_as_clean_and_never_touches_foreign_jobs() {
        // Another tool's job is registered; ours is not.
        let cli = FakeCli::with_job(CliMode::Healthy, "someone-else:job");
        let report = OpenClaw::new(PathBuf::from("/h"), &cli).remove();
        assert_eq!(report.state, TriggerState::Inactive);
        assert_eq!(
            cli.keys(),
            vec!["someone-else:job".to_owned()],
            "foreign job kept"
        );
        assert_eq!(cli.calls().len(), 1, "no rm was attempted");
    }

    #[test]
    fn unverified_removal_always_degrades() {
        // No binary, a down gateway, or an unreadable list: in every case NO removal was proven —
        // a persisted job may survive and resume when the gateway returns, so the report is
        // Degraded, never a claimed clean.
        for mode in [CliMode::NoBinary, CliMode::GatewayDown] {
            let cli = FakeCli::new(mode);
            let report = OpenClaw::new(PathBuf::from("/h"), &cli).remove();
            assert_eq!(report.state, TriggerState::Degraded, "{mode:?}");
        }
        // A zero-exit `cron list` whose stdout does not parse proves nothing about the job.
        let cli = FakeCli::new(CliMode::UnreadableList);
        let report = OpenClaw::new(PathBuf::from("/h"), &cli).remove();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cli.calls().len(), 1, "no blind rm was attempted");
    }

    #[test]
    fn trigger_present_is_a_live_scheduler_probe_never_faith() {
        // A registered job answers true…
        let cli = FakeCli::with_job(CliMode::Healthy, MARKER_ID);
        assert!(OpenClaw::new(PathBuf::from("/h"), &cli).present());
        // …no job answers false…
        let cli = FakeCli::new(CliMode::Healthy);
        assert!(!OpenClaw::new(PathBuf::from("/h"), &cli).present());
        // …and anything unprovable answers false (health is never claimed on faith).
        for mode in [
            CliMode::NoBinary,
            CliMode::GatewayDown,
            CliMode::UnreadableList,
        ] {
            let cli = FakeCli::new(mode);
            assert!(
                !OpenClaw::new(PathBuf::from("/h"), &cli).present(),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_cron_command_is_the_guarded_sweep() {
        // The payload runs via `sh -lc` (probed), so the `command -v` guard works: an orphaned
        // job (topos uninstalled, OpenClaw later restarted) no-ops cleanly instead of erroring
        // forever in the scheduler's run log.
        assert!(CRON_COMMAND.starts_with("command -v topos"));
        assert!(CRON_COMMAND.contains("topos install --quiet --from openclaw"));
        assert!(CRON_COMMAND.ends_with("|| true"));
    }

    /// The scheduler job has NO path, so it is disclosed as the out-of-process row — always, and
    /// unprobed (a preview that ran `cron list` would be running the harness). It is the ONLY row:
    /// topos owns no file under the OpenClaw home.
    #[test]
    fn the_scheduler_job_is_the_only_disclosed_artifact() {
        let home = TempHome::new();
        let cli = FakeCli::new(CliMode::Healthy);
        let a = OpenClaw::new(home.0.clone(), &cli);
        let job = TriggerArtifact::OutOfProcess {
            harness: DISPLAY_NAME,
        };
        assert_eq!(
            a.artifacts(),
            vec![job.clone()],
            "clean home → the scheduler job alone, named before it is armed"
        );

        // Registering the cron changes nothing here: it was already named, and it is still no path.
        a.install();
        assert_eq!(a.artifacts(), vec![job.clone()]);

        // A config file in the harness's own home is never ours, and never disclosed.
        std::fs::write(home.0.join("openclaw.json"), "{\"model\": \"opus\"}\n").unwrap();
        assert_eq!(a.artifacts(), vec![job]);
    }

    /// The row a person reads names the harness the way the registry names it — one spelling, not
    /// two.
    #[test]
    fn the_disclosed_display_name_is_the_registry_rows() {
        assert_eq!(
            crate::registry::known_harness(HarnessId::OpenClaw.slug())
                .expect("openclaw is a registry row")
                .display_name,
            DISPLAY_NAME
        );
    }

    #[test]
    fn reports_label_scheduled_only_when_active() {
        let cli = FakeCli::new(CliMode::Healthy);
        let a = OpenClaw::new(PathBuf::from("/h"), &cli);
        let active = a.install();
        assert_eq!(active.currency_kind, CurrencyKind::Scheduled);
        let inactive = a.remove();
        assert_eq!(
            inactive.currency_kind,
            CurrencyKind::ExplicitPullOnly,
            "anything but Active advertises only the guaranteed floor"
        );
    }

    #[test]
    fn discover_finds_skill_dirs_and_ignores_non_skills_without_panic() {
        let home = TempHome::new();
        home.skill("pr-describe");
        home.skill("commit-msg");
        // A dir with no SKILL.md is not a skill; a dot-dir and a stray file are skipped.
        std::fs::create_dir_all(home.0.join("skills").join("not-a-skill")).unwrap();
        std::fs::create_dir_all(home.0.join("skills").join(".topos-staging-x")).unwrap();
        std::fs::write(
            home.0
                .join("skills")
                .join(".topos-staging-x")
                .join("SKILL.md"),
            b"x",
        )
        .unwrap();
        std::fs::write(home.0.join("skills").join("loose.txt"), b"x").unwrap();

        let cli = FakeCli::new(CliMode::NoBinary);
        let found = OpenClaw::new(home.0.clone(), &cli).discover();
        let names: Vec<String> = found
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["commit-msg", "pr-describe"],
            "sorted, skills only"
        );
        assert!(found.iter().all(|d| d.layer.as_deref() == Some("user")));
    }

    #[test]
    fn discover_on_absent_home_is_empty_not_an_error() {
        let cli = FakeCli::new(CliMode::NoBinary);
        let found = OpenClaw::new(PathBuf::from("/no-such-openclaw-home-xyz"), &cli).discover();
        assert!(found.is_empty());
    }

    #[test]
    fn placement_for_reuses_a_discovered_dir_and_defaults_to_the_skills_dir() {
        let cli = FakeCli::new(CliMode::NoBinary);
        let a = OpenClaw::new(PathBuf::from("/h"), &cli);
        let disc = DiscoveredPlacement {
            path: PathBuf::from("/h/skills/pr-describe"),
            layer: Some(LAYER_USER.to_owned()),
        };
        assert_eq!(
            a.placement_for("topos_abc", PlacementNaming::default(), Some(&disc))
                .dir,
            PathBuf::from("/h/skills/pr-describe")
        );
        assert_eq!(
            a.placement_for("topos_abc", PlacementNaming::default(), None)
                .dir,
            PathBuf::from("/h/skills/topos_abc")
        );
    }
}
