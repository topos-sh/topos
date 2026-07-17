//! The `OpenClaw` [`HarnessAdapter`] — discovery, byte-exact placement targeting, and the
//! idempotent **silent-cron auto-update trigger** registered through OpenClaw's own CLI.
//!
//! OpenClaw reads native AgentSkills-spec `SKILL.md` bundles from `~/.openclaw/skills` (probed
//! live against openclaw@2026.7.1 in a container: recognized offline, ungated, source
//! `openclaw-managed`; the skills watcher defaults ON with a 250 ms debounce, so changed bytes
//! surface on the next agent turn mid-session). Placement therefore needs NO injection surface —
//! the old topos-owned bootstrap-inject plugin (a context file registered in `openclaw.json`) is
//! RETIRED: this adapter never writes it again, and install/remove SCRUB any legacy artifacts (the
//! config registration + the marker-confirmed plugin file) so old installs converge. On current
//! builds the config is JSON5 and the old top-level registration key no longer exists — which
//! independently confirms the retirement; the scrub only ever touches the strict-JSON shapes the
//! old topos itself wrote, and leaves anything else byte-untouched.
//!
//! **The auto-update trigger is a silent OpenClaw cron job** (probed live): `openclaw cron add
//! --command <shell> --no-deliver --declaration-key <key> --json` registers a deterministic,
//! model-free shell job persisted in OpenClaw's own SQLite, idempotent by declaration key (a
//! re-add answers `created:false`, same job — the key IS the ownership marker), firing on a
//! 1-minute cadence. The registered shell line is the same guarded sweep the Claude Code hook
//! runs — `topos update --quiet` behind a `command -v` guard with an exit-0 tail (the job runs
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
//! are composed here); the only FILE writes are the legacy scrub of the config registration this
//! tool itself once wrote (through the injected [`ConfigStore`]) and the unlink of its own
//! marker-confirmed plugin file — never a skill dir, never a foreign byte.

use std::path::PathBuf;

use serde_json::Value;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::{
    CommandRunner, ConfigStore, DiscoveredPlacement, HarnessAdapter, PlacementNaming,
    PlacementTarget,
};

/// The user-scope layer label recorded for a discovered/placed OpenClaw skill (the resolved layer;
/// a project/enterprise layer stays representable later — `DiscoveredPlacement.layer` is already
/// `Option<String>`).
const LAYER_USER: &str = "user";

/// The structured marker identity reported in [`TriggerReport::marker_id`] — AND the cron job's
/// `--declaration-key`, which makes the registration idempotent (probed: a re-add with the same
/// key answers `created:false`, the same job id, never a duplicate) and is the ownership marker
/// the remove path resolves the job by. Schema 2 = the silent-cron trigger; schema 1 was the
/// retired bootstrap-inject surface (still recognized by the legacy scrub below).
const MARKER_ID: &str = "topos:openclaw:currency:2";

/// The OpenClaw management CLI, resolved from `PATH` by the injected runner.
const OPENCLAW_BIN: &str = "openclaw";

/// The cron job's human-facing name (shows in `openclaw cron list`; identity rides the
/// declaration key, never this label).
const CRON_NAME: &str = "topos-currency";

/// The cadence (probed: sub-minute is allowed; one minute is the deliberate floor — the client's
/// own TTL gate throttles the sweeps this fires).
const CRON_EVERY: &str = "1m";

/// The job's shell payload (OpenClaw runs it via `sh -lc`). The `command -v` guard + exit-0 tail
/// mirror the Claude Code hook line: a machine that lost the `topos` binary (an uninstall; the
/// job surviving in OpenClaw's store) no-ops CLEANLY, so the job never accumulates error state.
const CRON_COMMAND: &str = "command -v topos >/dev/null 2>&1 && topos update --quiet || true";

/// The RETIRED inject surface's marker — recognized only to scrub artifacts an earlier topos
/// wrote: a plugin file carrying it is ours to unlink (after de-referencing), a file without it
/// is foreign and never touched.
const LEGACY_MARKER: &str = "topos:openclaw:currency:1";

/// The retired inject plugin's file name, flat under the OpenClaw home (where the old adapter
/// wrote it).
const LEGACY_PLUGIN_FILE_NAME: &str = "topos-currency.mjs";

/// The retired `openclaw.json` registration key the old adapter appended the plugin's path to.
/// (Current builds moved this surface elsewhere entirely; only the exact strict-JSON shape the
/// old topos wrote is ever scrubbed.)
const LEGACY_EXTRA_FILES_KEY: &str = "bootstrap-extra-files";

/// The `OpenClaw` [`HarnessAdapter`]. Holds the resolved config home, the [`ConfigStore`] port
/// (the legacy scrub's durable write), and the [`CommandRunner`] port (the `openclaw cron` CLI) —
/// all injected, so tests point the home at a temp dir and drive a fake CLI.
pub struct OpenClaw<'a> {
    /// `$HOME/.openclaw` — injected in tests; see [`OpenClaw::resolve_home`].
    home: PathBuf,
    cfg: &'a dyn ConfigStore,
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
    /// Construct over an explicit config home + the two ports. Production passes
    /// [`OpenClaw::resolve_home`] and the CLI crate's real process runner; tests pass a temp dir
    /// and fakes so a real `~/.openclaw` (or a real `openclaw` binary) is never touched.
    #[must_use]
    pub fn new(home: PathBuf, cfg: &'a dyn ConfigStore, cli: &'a dyn CommandRunner) -> Self {
        Self { home, cfg, cli }
    }

    /// Resolve OpenClaw's config home: `$HOME/.openclaw` (falling back to `./.openclaw` if `$HOME`
    /// is unset).
    #[must_use]
    pub fn resolve_home() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".openclaw")
    }

    fn skills_dir(&self) -> PathBuf {
        self.home.join("skills")
    }

    fn config_path(&self) -> PathBuf {
        self.home.join("openclaw.json")
    }

    fn legacy_plugin_path(&self) -> PathBuf {
        self.home.join(LEGACY_PLUGIN_FILE_NAME)
    }

    /// Read the current config bytes, `None` if absent, `Err` only on a genuine I/O failure.
    fn read_config(&self) -> std::io::Result<Option<Vec<u8>>> {
        self.cfg.read(&self.config_path())
    }

    /// Probe the legacy plugin path through the port: absent, ours (bytes carry
    /// [`LEGACY_MARKER`]), or a foreign file on that path (never touched).
    fn plugin_file(&self) -> std::io::Result<PluginFile> {
        Ok(match self.cfg.read(&self.legacy_plugin_path())? {
            None => PluginFile::Absent,
            Some(bytes) if String::from_utf8_lossy(&bytes).contains(LEGACY_MARKER) => {
                PluginFile::Ours
            }
            Some(_) => PluginFile::Foreign,
        })
    }

    /// Scrub the RETIRED inject surface's artifacts, best-effort and fail-closed: remove the
    /// exact registration the old topos wrote from `openclaw.json` (strict-JSON shapes only — a
    /// JSON5/malformed/wrong-typed config is left byte-untouched, and then the plugin file is
    /// left too, since an unreadable config may still reference it), then unlink the plugin file
    /// ONLY when it carries the legacy marker and is provably de-referenced. Returns the config
    /// path when the scrub wrote it (for the report's `touched_path`). Never decides the trigger
    /// STATE — the cron does; leftovers stay disclosed via the footprint.
    fn scrub_legacy(&self) -> Option<PathBuf> {
        let Ok(plugin) = self.plugin_file() else {
            return None; // unreadable plugin path — touch nothing
        };
        let mut touched = None;
        match self.read_config() {
            Err(_) => return None, // unreadable config — may still reference the plugin
            Ok(None) => {}         // no config: nothing referenced; fall through to the unlink
            Ok(Some(bytes)) if bytes.iter().all(u8::is_ascii_whitespace) => {}
            Ok(Some(bytes)) => {
                let Ok(mut root) = serde_json::from_slice::<Value>(&bytes) else {
                    return None; // not strict JSON (incl. JSON5) — never ours to edit
                };
                let obj = root.as_object_mut()?;
                let canonical = self.legacy_plugin_path().to_string_lossy().into_owned();
                match obj.get(LEGACY_EXTRA_FILES_KEY) {
                    None => {} // nothing registered
                    Some(Value::Array(entries)) => {
                        if entries
                            .iter()
                            .any(|e| e.as_str() == Some(canonical.as_str()))
                        {
                            // The openclaw#51789-safe fresh-array scrub of ONLY our exact entry;
                            // prune the key when our removal emptied it.
                            let fresh: Vec<Value> = entries
                                .iter()
                                .filter(|e| e.as_str() != Some(canonical.as_str()))
                                .cloned()
                                .collect();
                            if fresh.is_empty() {
                                obj.remove(LEGACY_EXTRA_FILES_KEY);
                            } else {
                                obj.insert(LEGACY_EXTRA_FILES_KEY.to_owned(), Value::Array(fresh));
                            }
                            let out = serialize(&root)?;
                            // De-reference BEFORE delete: if the scrub write fails, the config
                            // still points at the file — never unlink it then.
                            if self.cfg.replace(&self.config_path(), &out).is_err() {
                                return None;
                            }
                            touched = Some(self.config_path());
                        }
                    }
                    Some(_) => return None, // wrong-typed while retired — unprovable, leave all
                }
            }
        }
        if plugin == PluginFile::Ours {
            // Best-effort: a failed unlink leaves an inert orphan the footprint still discloses.
            let _ = std::fs::remove_file(self.legacy_plugin_path());
        }
        touched
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

    fn report(&self, state: TriggerState, touched: Option<PathBuf>) -> TriggerReport {
        TriggerReport {
            harness: HarnessId::OpenClaw,
            // Honest labeling: Scheduled only for a verified registration round-trip; every other
            // state advertises just the guaranteed floor — an explicit `topos update`.
            currency_kind: if state == TriggerState::Active {
                CurrencyKind::Scheduled
            } else {
                CurrencyKind::ExplicitPullOnly
            },
            touched_path: touched.map(|p| p.to_string_lossy().into_owned()),
            marker_id: MARKER_ID.to_owned(),
            state,
        }
    }

    /// Whether the RETIRED registration entry is still present (drives `--footprint` disclosure).
    /// A missing/unreadable/malformed config means "not present" — we never claim to own a path
    /// we cannot confirm.
    fn has_legacy_entry(&self) -> bool {
        let Ok(Some(bytes)) = self.read_config() else {
            return false;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        let canonical = self.legacy_plugin_path().to_string_lossy().into_owned();
        root.as_object()
            .and_then(|o| o.get(LEGACY_EXTRA_FILES_KEY))
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|e| e.as_str() == Some(canonical.as_str()))
            })
    }
}

/// What the legacy plugin path currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginFile {
    Absent,
    /// A file carrying [`LEGACY_MARKER`] — ours to unlink once de-referenced.
    Ours,
    /// A file WITHOUT our marker on that path — never touched.
    Foreign,
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

    fn discover(&self) -> Vec<DiscoveredPlacement> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.skills_dir()) else {
            return out; // no skills dir (or unreadable) → nothing discovered, never an error
        };
        for entry in entries.flatten() {
            // The skill name is the directory name, so a non-UTF-8 name can't be a skill we manage.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Skip dot-prefixed entries: a transient `.topos-staging-*` / `.topos-old-*` dir the
            // materializer builds beside a skill dir is never a real skill, even with a `SKILL.md`
            // inside — so a concurrent discovery during the sub-second swap window can't surface it.
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // A skill is a directory (follow symlinks — a symlinked skill dir is valid) whose root
            // `SKILL.md` is a regular file. SKILL.md's existence confirms skill-ness — never the
            // frontmatter (all-optional, and we never parse it), so a malformed SKILL.md can't
            // mislead.
            if path.is_dir() && path.join("SKILL.md").is_file() {
                out.push(DiscoveredPlacement {
                    path,
                    layer: Some(LAYER_USER.to_owned()),
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path)); // read_dir order is OS-dependent — pin it
        out
    }

    fn placement_for(
        &self,
        skill_id: &str,
        // Name-based placement is the reference (Claude Code) adapter's; this adapter's concrete
        // dir shape stays id-keyed until the cross-harness placement work lands.
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

    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::Scheduled
    }

    fn install_currency_trigger(&self) -> TriggerReport {
        // The legacy inject surface is scrubbed FIRST (best-effort, fail-closed) so an upgraded
        // install converges on the one trigger; its outcome never decides the state — the cron
        // registration does.
        let touched = self.scrub_legacy();
        if self.register_cron() {
            self.report(TriggerState::Active, touched)
        } else {
            // No binary / gateway down / CLI error: nothing is registered, nothing fires on its
            // own — the floor is an explicit `topos update` (the watcher then surfaces the bytes).
            self.report(TriggerState::Degraded, touched)
        }
    }

    fn remove_currency_trigger(&self) -> TriggerReport {
        let touched = self.scrub_legacy();
        match self.remove_cron() {
            CronRemoval::Removed | CronRemoval::NotPresent => {
                self.report(TriggerState::Inactive, touched)
            }
            // Removal was NOT verified (no binary / gateway down / unreadable list): a persisted
            // job may survive and resume — disclosed as Degraded, never claimed clean.
            CronRemoval::Unavailable => self.report(TriggerState::Degraded, touched),
        }
    }

    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        // Disclosure-only, and LEGACY-only: the cron job is OpenClaw-owned scheduler state
        // (removed via `remove_currency_trigger`, not a filesystem path); what topos may still
        // own on disk are the retired inject artifacts — the config registration (never a delete
        // target; scrubbed surgically) and the marker-confirmed plugin file.
        let mut out = Vec::new();
        if self.has_legacy_entry() {
            out.push(self.config_path());
        }
        if matches!(self.plugin_file(), Ok(PluginFile::Ours)) {
            out.push(self.legacy_plugin_path());
        }
        out
    }

    /// The hook-health probe: our trigger lives in OpenClaw's SCHEDULER, not the filesystem, so
    /// the default footprint-based answer would call a healthy cron "not installed". A live
    /// `cron list` proves presence; anything unprovable (no binary, a down gateway, unreadable
    /// output) answers `false` — health is never claimed on faith.
    fn trigger_present(&self) -> bool {
        match self.cli.run(OPENCLAW_BIN, &["cron", "list", "--json"]) {
            Ok(out) if out.success => matches!(read_jobs(&out.stdout), ListRead::Jobs(Some(_))),
            _ => false,
        }
    }
}

/// Serialize the scrubbed config: 2-space pretty + a trailing newline, keys alphabetical
/// (`serde_json`'s default — `preserve_order` stays off, a workspace-global feature flip). A
/// write happens only on a real change, so any normalization is one-time and action-triggered.
fn serialize(root: &Value) -> Option<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(root).ok()?;
    text.push('\n');
    Some(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunOutput;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A path-keyed in-memory [`ConfigStore`] for the pure scrub tests; the unlink paths need
    /// [`DiskConfig`] because the legacy plugin delete uses `std::fs::remove_file`.
    #[derive(Debug, Default)]
    struct MemConfig {
        files: RefCell<HashMap<PathBuf, Vec<u8>>>,
        writes: RefCell<u32>,
    }
    impl MemConfig {
        fn with_config(bytes: &str) -> Self {
            let me = Self::default();
            me.files
                .borrow_mut()
                .insert(PathBuf::from("/h/openclaw.json"), bytes.as_bytes().to_vec());
            me
        }
        fn text(&self, path: &str) -> Option<String> {
            self.files
                .borrow()
                .get(Path::new(path))
                .map(|b| String::from_utf8(b.clone()).unwrap())
        }
        fn writes(&self) -> u32 {
            *self.writes.borrow()
        }
    }
    impl ConfigStore for MemConfig {
        fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            Ok(self.files.borrow().get(path).cloned())
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), bytes.to_vec());
            *self.writes.borrow_mut() += 1;
            Ok(())
        }
    }

    /// A real-disk [`ConfigStore`] over a temp home, for the tests where the `std::fs` unlink
    /// (and `discover`'s `read_dir`) must be observable.
    #[derive(Debug)]
    struct DiskConfig;
    impl ConfigStore for DiskConfig {
        fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
            match std::fs::read(path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)
        }
    }

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

    const CONFIG: &str = "/h/openclaw.json";

    /// The legacy plugin bytes an OLD topos wrote (the marker is what the scrub keys on).
    fn legacy_plugin_bytes() -> String {
        format!(
            "// {LEGACY_MARKER} — the retired topos-managed bootstrap-inject surface.\nexport default {{}};\n"
        )
    }

    #[test]
    fn install_registers_the_silent_cron_job_byte_exact() {
        let cfg = MemConfig::default();
        let cli = FakeCli::new(CliMode::Healthy);
        let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).install_currency_trigger();

        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(report.harness, HarnessId::OpenClaw);
        assert_eq!(report.currency_kind, CurrencyKind::Scheduled);
        assert_eq!(report.marker_id, MARKER_ID);
        assert!(report.touched_path.is_none(), "no config file was edited");
        assert_eq!(cfg.writes(), 0, "the trigger writes no file");

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
        let cfg = MemConfig::default();
        let cli = FakeCli::new(CliMode::Healthy);
        let a = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli);
        a.install_currency_trigger();
        let report = a.install_currency_trigger();
        assert_eq!(
            report.state,
            TriggerState::Active,
            "created:false is still registered"
        );
        assert_eq!(cli.keys().len(), 1, "never a duplicate job");
        assert_eq!(cfg.writes(), 0);
    }

    #[test]
    fn install_degrades_honestly_without_the_binary_or_gateway() {
        for mode in [CliMode::NoBinary, CliMode::GatewayDown] {
            let cfg = MemConfig::default();
            let cli = FakeCli::new(mode);
            let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).install_currency_trigger();
            assert_eq!(report.state, TriggerState::Degraded, "{mode:?}");
            assert_eq!(report.currency_kind, CurrencyKind::ExplicitPullOnly);
            assert_eq!(cfg.writes(), 0, "nothing is written on a degrade");
            assert!(cli.keys().is_empty(), "nothing was registered");
        }
    }

    #[test]
    fn install_scrubs_the_legacy_inject_surface_first() {
        // A home an OLD topos armed: the config registration + the marker plugin, on real disk.
        let home = TempHome::new();
        let cfg = DiskConfig;
        std::fs::write(
            home.0.join("openclaw.json"),
            format!(
                "{{\n  \"bootstrap-extra-files\": [\"/keep/notes.md\", \"{}\"],\n  \"model\": \"opus\"\n}}\n",
                home.0.join(LEGACY_PLUGIN_FILE_NAME).display()
            ),
        )
        .unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), legacy_plugin_bytes()).unwrap();

        let cli = FakeCli::new(CliMode::Healthy);
        let report = OpenClaw::new(home.0.clone(), &cfg, &cli).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(
            report.touched_path.as_deref(),
            Some(home.0.join("openclaw.json").to_str().unwrap()),
            "the legacy scrub's config write is disclosed"
        );
        assert!(
            !home.0.join(LEGACY_PLUGIN_FILE_NAME).exists(),
            "the de-referenced marker plugin was unlinked"
        );
        let root: Value =
            serde_json::from_slice(&std::fs::read(home.0.join("openclaw.json")).unwrap()).unwrap();
        assert_eq!(root["model"], "opus", "foreign key survives");
        assert_eq!(
            root[LEGACY_EXTRA_FILES_KEY],
            serde_json::json!(["/keep/notes.md"]),
            "only our exact entry was scrubbed; the sibling survives"
        );
        assert_eq!(
            cli.keys(),
            vec![MARKER_ID.to_owned()],
            "the cron registered"
        );
    }

    #[test]
    fn legacy_scrub_prunes_the_key_it_emptied_and_unlinks_the_orphan() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        std::fs::write(
            home.0.join("openclaw.json"),
            format!(
                "{{\"bootstrap-extra-files\": [\"{}\"]}}",
                home.0.join(LEGACY_PLUGIN_FILE_NAME).display()
            ),
        )
        .unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), legacy_plugin_bytes()).unwrap();

        let cli = FakeCli::new(CliMode::Healthy);
        OpenClaw::new(home.0.clone(), &cfg, &cli).install_currency_trigger();
        let root: Value =
            serde_json::from_slice(&std::fs::read(home.0.join("openclaw.json")).unwrap()).unwrap();
        assert!(
            root.get(LEGACY_EXTRA_FILES_KEY).is_none(),
            "the key our removal emptied is pruned"
        );
        assert!(!home.0.join(LEGACY_PLUGIN_FILE_NAME).exists());

        // A marker orphan with NO config at all is unlinked too.
        std::fs::remove_file(home.0.join("openclaw.json")).unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), legacy_plugin_bytes()).unwrap();
        OpenClaw::new(home.0.clone(), &cfg, &cli).install_currency_trigger();
        assert!(!home.0.join(LEGACY_PLUGIN_FILE_NAME).exists());
    }

    #[test]
    fn legacy_scrub_never_touches_an_unprovable_config_or_a_foreign_file() {
        // A JSON5-ish config (current OpenClaw builds) referencing who-knows-what: byte-untouched,
        // and the marker plugin stays too (it may still be referenced in a shape we can't read).
        let home = TempHome::new();
        let cfg = DiskConfig;
        let json5 = "{ // comment\n  theme: \"dark\",\n}\n";
        std::fs::write(home.0.join("openclaw.json"), json5).unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), legacy_plugin_bytes()).unwrap();

        let cli = FakeCli::new(CliMode::Healthy);
        let report = OpenClaw::new(home.0.clone(), &cfg, &cli).install_currency_trigger();
        assert_eq!(
            report.state,
            TriggerState::Active,
            "the cron is independent"
        );
        assert_eq!(
            std::fs::read_to_string(home.0.join("openclaw.json")).unwrap(),
            json5,
            "an unprovable config is never edited"
        );
        assert!(
            home.0.join(LEGACY_PLUGIN_FILE_NAME).exists(),
            "the plugin stays while the config is unreadable"
        );

        // A marker-LESS file on the legacy path is foreign — never unlinked, even with no config.
        std::fs::remove_file(home.0.join("openclaw.json")).unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), "export default {};\n").unwrap();
        OpenClaw::new(home.0.clone(), &cfg, &cli).install_currency_trigger();
        assert!(
            home.0.join(LEGACY_PLUGIN_FILE_NAME).exists(),
            "foreign file kept"
        );
    }

    #[test]
    fn legacy_scrub_leaves_foreign_registrations_alone() {
        // Registrations that are NOT our exact canonical path (another home's plugin, a
        // hand-rolled note) are left verbatim — adopt-or-leave, and the cron proceeds.
        let before =
            "{\n  \"bootstrap-extra-files\": [\n    \"/elsewhere/topos-currency.mjs\"\n  ]\n}\n";
        let cfg = MemConfig::with_config(before);
        let cli = FakeCli::new(CliMode::Healthy);
        let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).install_currency_trigger();
        assert_eq!(report.state, TriggerState::Active);
        assert_eq!(cfg.writes(), 0, "a foreign registration is never scrubbed");
        assert_eq!(cfg.text(CONFIG).as_deref(), Some(before));
    }

    #[test]
    fn remove_unregisters_by_declaration_key() {
        let cfg = MemConfig::default();
        let cli = FakeCli::with_job(CliMode::Healthy, MARKER_ID);
        let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).remove_currency_trigger();
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
        let cfg = MemConfig::default();
        // Another tool's job is registered; ours is not.
        let cli = FakeCli::with_job(CliMode::Healthy, "someone-else:job");
        let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).remove_currency_trigger();
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
        let cfg = MemConfig::default();
        for mode in [CliMode::NoBinary, CliMode::GatewayDown] {
            let cli = FakeCli::new(mode);
            let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).remove_currency_trigger();
            assert_eq!(report.state, TriggerState::Degraded, "{mode:?}");
        }
        // A zero-exit `cron list` whose stdout does not parse proves nothing about the job.
        let cli = FakeCli::new(CliMode::UnreadableList);
        let report = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Degraded);
        assert_eq!(cli.calls().len(), 1, "no blind rm was attempted");
    }

    #[test]
    fn trigger_present_is_a_live_scheduler_probe_never_faith() {
        let cfg = MemConfig::default();
        // A registered job answers true…
        let cli = FakeCli::with_job(CliMode::Healthy, MARKER_ID);
        assert!(OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).trigger_present());
        // …no job answers false…
        let cli = FakeCli::new(CliMode::Healthy);
        assert!(!OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).trigger_present());
        // …and anything unprovable answers false (health is never claimed on faith).
        for mode in [
            CliMode::NoBinary,
            CliMode::GatewayDown,
            CliMode::UnreadableList,
        ] {
            let cli = FakeCli::new(mode);
            assert!(
                !OpenClaw::new(PathBuf::from("/h"), &cfg, &cli).trigger_present(),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn remove_scrubs_legacy_artifacts_too() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        std::fs::write(
            home.0.join("openclaw.json"),
            format!(
                "{{\"bootstrap-extra-files\": [\"{}\"]}}",
                home.0.join(LEGACY_PLUGIN_FILE_NAME).display()
            ),
        )
        .unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), legacy_plugin_bytes()).unwrap();
        let cli = FakeCli::new(CliMode::Healthy);
        let report = OpenClaw::new(home.0.clone(), &cfg, &cli).remove_currency_trigger();
        assert_eq!(report.state, TriggerState::Inactive);
        assert!(!home.0.join(LEGACY_PLUGIN_FILE_NAME).exists());
        assert!(
            report.touched_path.is_some(),
            "the legacy config scrub is disclosed"
        );
    }

    #[test]
    fn the_cron_command_is_the_guarded_sweep() {
        // The payload runs via `sh -lc` (probed), so the `command -v` guard works: an orphaned
        // job (topos uninstalled, OpenClaw later restarted) no-ops cleanly instead of erroring
        // forever in the scheduler's run log.
        assert!(CRON_COMMAND.starts_with("command -v topos"));
        assert!(CRON_COMMAND.contains("topos update --quiet"));
        assert!(CRON_COMMAND.ends_with("|| true"));
    }

    #[test]
    fn footprint_discloses_only_legacy_artifacts() {
        let home = TempHome::new();
        let cfg = DiskConfig;
        let cli = FakeCli::new(CliMode::Healthy);
        let a = OpenClaw::new(home.0.clone(), &cfg, &cli);
        assert!(a.uninstall_footprint().is_empty(), "clean home → nothing");

        // A live cron registration is OpenClaw-owned state, never a footprint path.
        a.install_currency_trigger();
        assert!(a.uninstall_footprint().is_empty());

        // Legacy artifacts ARE disclosed (marker-confirmed only).
        std::fs::write(
            home.0.join("openclaw.json"),
            format!(
                "{{\"bootstrap-extra-files\": [\"{}\"]}}",
                home.0.join(LEGACY_PLUGIN_FILE_NAME).display()
            ),
        )
        .unwrap();
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), legacy_plugin_bytes()).unwrap();
        assert_eq!(
            a.uninstall_footprint(),
            vec![
                home.0.join("openclaw.json"),
                home.0.join(LEGACY_PLUGIN_FILE_NAME)
            ]
        );
        // A foreign file on the legacy path is never claimed.
        std::fs::write(home.0.join(LEGACY_PLUGIN_FILE_NAME), "export default {};\n").unwrap();
        assert_eq!(a.uninstall_footprint(), vec![home.0.join("openclaw.json")]);
    }

    #[test]
    fn reports_label_scheduled_only_when_active() {
        let cfg = MemConfig::default();
        let cli = FakeCli::new(CliMode::Healthy);
        let a = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli);
        assert_eq!(a.currency_kind(), CurrencyKind::Scheduled);
        let active = a.install_currency_trigger();
        assert_eq!(active.currency_kind, CurrencyKind::Scheduled);
        let inactive = a.remove_currency_trigger();
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

        let cfg = MemConfig::default();
        let cli = FakeCli::new(CliMode::NoBinary);
        let found = OpenClaw::new(home.0.clone(), &cfg, &cli).discover();
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
        let cfg = MemConfig::default();
        let cli = FakeCli::new(CliMode::NoBinary);
        let found =
            OpenClaw::new(PathBuf::from("/no-such-openclaw-home-xyz"), &cfg, &cli).discover();
        assert!(found.is_empty());
    }

    #[test]
    fn placement_for_reuses_a_discovered_dir_and_defaults_to_the_skills_dir() {
        let cfg = MemConfig::default();
        let cli = FakeCli::new(CliMode::NoBinary);
        let a = OpenClaw::new(PathBuf::from("/h"), &cfg, &cli);
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
