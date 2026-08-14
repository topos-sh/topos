//! `uninstall [--yes]` — remove topos from this machine, two-phase.
//!
//! Bare = a DESCRIBE of exactly what goes: EVERY harness auto-update-trigger artifact the apply
//! reaches, not just the active harness's — each named by its path, or, where a harness keeps the
//! trigger in its own program instead of a file, by what the scrub will dial there — the MCP config
//! files topos placed server entries into, the `~/.topos/` sidecar tree (which holds the signed-in
//! credential), and the note that SKILL FILES IN AGENT DIRS STAY (uninstall never deletes a skill
//! byte).
//!
//! `--yes` deletes the built-in skill's copies, retires topos's own MCP config entries, deletes the
//! `~/.topos/` tree via the fs seam, and LAST scrubs the auto-update triggers — the active
//! harness's and then every other supported harness's (all reports surfaced honestly). The trigger
//! scrubs go LAST because the deletions before them can fail, and **a teardown that dies halfway
//! must not have disarmed a single agent on its way**: an agent left un-updated with nothing removed
//! to show for it is the worst of both outcomes. The MCP scrub is the one destructive act that must
//! precede the sidecar delete, because the ownership ledger that proves which entries are topos's
//! lives inside the tree being deleted — after it, nothing could ever tell them from a hand edit.
//!
//! The `topos` binary is NOT self-deleted (a package manager may own it) — its path is disclosed
//! with a "remove it with your installer (or `rm <path>`)" note. A maintenance command: it needs no
//! sign-in, mints no identity, and touches no plane.

use std::path::PathBuf;

use topos_types::{Message, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;

// The two payload shapes live in `topos-types` (the ONE contract crate): the committed
// JSON-Schema is generated from there, and `topos --schema` serves it. This module keeps only the
// teardown itself.
pub(crate) use topos_types::results::{UninstallApplied, UninstallDescribe};

/// The verb's outcome — the two-phase pair.
#[derive(Debug)]
pub(crate) enum UninstallOutcome {
    Described {
        describe: UninstallDescribe,
        yes_argv: Vec<String>,
    },
    Applied {
        applied: UninstallApplied,
        /// One `failure` per thing the teardown could NOT remove — empty on a clean teardown, and
        /// the ONE value the finisher reads for `ok`, the exit status, and the headline. A
        /// teardown that left a trigger armed did not uninstall topos, whatever else it managed.
        messages: Vec<Message>,
    },
}

/// A teardown that FAILED partway: the error, plus the receipt of the destructive work that had
/// already landed. `partial` is `None` only where nothing had been touched at all (the sidecar
/// fence, a bare describe) — reporting an empty receipt there would answer a question nobody
/// asked. Everywhere else the failure path owes the same rows the success path prints: the whole
/// complaint about the old behaviour was a teardown that deleted things and then said nothing but
/// "a filesystem operation failed".
#[derive(Debug)]
pub(crate) struct UninstallFailure {
    pub error: ClientError,
    /// Boxed: the receipt is the wide half of this pair, and every non-teardown verb pays for the
    /// `Result`'s error size on its own hot path.
    pub partial: Option<Box<UninstallApplied>>,
}

impl From<ClientError> for Box<UninstallFailure> {
    fn from(error: ClientError) -> Self {
        Box::new(UninstallFailure {
            error,
            partial: None,
        })
    }
}

/// `uninstall [--yes]`. `binary_path` is the running executable's path (the composition root passes
/// `std::env::current_exe()`), disclosed but never deleted.
///
/// # Errors
/// An [`FsOps`](crate::fs_seam::FsOps) failure removing the sidecar tree — carrying the receipt of
/// whatever the teardown had already removed.
pub(crate) fn uninstall(
    ctx: &Ctx<'_>,
    binary_path: Option<PathBuf>,
    yes: bool,
) -> Result<UninstallOutcome, Box<UninstallFailure>> {
    // The whole machine's trigger artifacts — the ACTIVE harness's plus every other one the apply
    // scrubs — so the describe can never name less than `--yes` touches. Rendered through the
    // artifact's own `Display`, the ONE spelling every disclosure surface prints.
    let trigger_artifacts: Vec<String> = ctx
        .triggers
        .artifacts()
        .iter()
        .map(ToString::to_string)
        .collect();
    let home = ctx.layout.home();
    let sidecar_path = home.to_string_lossy().into_owned();
    let sidecar_present = ctx.fs.exists(home);
    let binary = binary_path.map(|p| p.to_string_lossy().into_owned());

    // The destructive remove is FENCED on the tree actually LOOKING like a topos sidecar: any of
    // the layout's own entries present, or an empty directory. A NON-empty directory carrying none
    // of them is almost certainly a mispointed `TOPOS_HOME` (e.g. `TOPOS_HOME=$HOME`), and deleting
    // it would be an unbounded loss — refused typed on BOTH phases, so the describe never promises
    // a remove the apply would refuse.
    if sidecar_present && !looks_like_sidecar(ctx, home) {
        return Err(ClientError::InvalidArgument(format!(
            "`{sidecar_path}` does not look like a topos sidecar (none of skills/, identity/, \
             ops/, state/, locks/, or log.jsonl inside) — refusing to delete it. If TOPOS_HOME is \
             set, point it at the real topos home"
        ))
        .into());
    }

    // The built-in `topos` skill's placed copies (best-effort read — a torn/newer sidecar must
    // never block the one command that deletes it).
    let builtin_dirs = super::builtin::placement_dirs(ctx).unwrap_or_default();
    // The MCP config files this machine's ownership ledger records entries in — read-only, and the
    // same split the apply acts on, so the preview promises exactly what the scrub does.
    let surfaces = mcp_surfaces(ctx);

    if !yes {
        return Ok(UninstallOutcome::Described {
            describe: UninstallDescribe {
                trigger_artifacts,
                sidecar_path,
                sidecar_present,
                binary_path: binary,
                builtin_dirs,
                mcp_files: surfaces.owned,
                mcp_drifted: surfaces.drifted,
            },
            yes_argv: vec![
                "topos".to_owned(),
                "uninstall".to_owned(),
                "--yes".to_owned(),
            ],
        });
    }

    // ---- APPLY (`--yes`) ----
    // The order is the module doc's: the built-in skill's placed copies, then topos's own MCP
    // config entries (BEFORE the ledger that proves they are topos's is deleted with the tree),
    // then the sidecar itself, and only after all of that the trigger scrubs.
    //
    // `applied` is built up as the work lands, so a failure hands back the receipt of exactly what
    // was done rather than nothing at all.
    let mut applied = UninstallApplied {
        binary_path: binary,
        ..UninstallApplied::default()
    };
    for dir in &builtin_dirs {
        let p = std::path::Path::new(dir);
        if ctx.fs.exists(p) {
            ctx.fs.remove_dir_all(p).map_err(|e| {
                Box::new(UninstallFailure {
                    error: fs_failure(e, dir),
                    partial: Some(Box::new(applied.clone())),
                })
            })?;
            applied.builtin_dirs.push(dir.clone());
        }
    }
    // Retire topos's own MCP entries through the SAME owned-entry mechanics `remove` uses: the
    // sentinel-clean rows the ledger records go, a hand-edited one is left byte-identical and
    // disclosed. Best-effort — a teardown is not blocked by an agent's config, and every failure
    // becomes a line rather than a dead uninstall.
    let mcp_messages = scrub_mcp_entries(ctx, &mut applied);
    if ctx.fs.exists(home) {
        ctx.fs.remove_dir_all(home).map_err(|e| {
            Box::new(UninstallFailure {
                error: fs_failure(e, &sidecar_path),
                partial: Some(Box::new(applied.clone())),
            })
        })?;
        applied.sidecar_removed = true;
    }
    // The trigger scrubs run LAST — after every fallible deletion above has SUCCEEDED. Those
    // deletions can fail (a permission error, an unremovable tree), and this verb fails with them;
    // disarming this machine's agents on the way out of a teardown that did not happen would leave
    // them silently un-updated with nothing removed to show for it. The breadth set is exactly the
    // one the describe disclosed.
    let active = ctx.triggers.scrub_active();
    let breadth = ctx.triggers.scrub_others();
    // Any trigger still armed means the teardown did not finish, whatever else it managed. The
    // messages are the ONE value the finisher reads for `ok`, the exit status and the headline.
    let mut messages: Vec<Message> = std::iter::once(&active)
        .chain(breadth.iter())
        .filter_map(trigger_failure)
        .collect();
    messages.extend(mcp_messages);
    applied.hook = Some(active.report);
    applied.triggers = breadth.into_iter().map(|s| s.report).collect();

    Ok(UninstallOutcome::Applied { applied, messages })
}

/// A filesystem failure with the PATH folded into the diagnostic detail. `safe_message` still
/// redacts the sentence a person reads, and this is what the appended log event (and the
/// `details:` pointer under it) carries: "a filesystem operation failed" with nothing else on the
/// record left a failed teardown unactionable.
fn fs_failure(e: std::io::Error, path: &str) -> ClientError {
    ClientError::IoKind {
        kind: e.kind(),
        context: format!("removing {path}: {e}"),
    }
}

/// One trigger the scrub could NOT complete, as the failure line the receipt and the machine
/// channel share. `Degraded` is the only unscrubbed state: a foreign hook was never topos's to
/// remove, and every other state is a scrub that happened.
fn trigger_failure(scrubbed: &crate::ops::Scrubbed) -> Option<Message> {
    if scrubbed.report.state != TriggerState::Degraded {
        return None;
    }
    let agent = &scrubbed.report.agent;
    Some(crate::message::failure(
        "TRIGGER_NOT_REMOVED",
        match &scrubbed.config_file {
            Some(path) => format!(
                "{agent}: the auto-update trigger could not be removed from {path} ({}) — remove \
                 it by hand, or fix the permission and run 'topos uninstall' again.",
                scrubbed
                    .report
                    .note
                    .as_deref()
                    .unwrap_or("topos could not edit that file")
            ),
            // The trigger lives in the harness's OWN scheduler, so there is no file to fix and no
            // permission to change: the only way in is the harness's own command, and topos could
            // not reach it.
            None => format!(
                "{agent}: the scheduled update job could not be checked or removed — the {agent} \
                 command is not available on this machine. If {} is installed, remove the job with \
                 its own scheduler.",
                display_name(agent)
            ),
        },
    ))
}

/// A harness's display name from the registry, falling back to its slug. Read off the TEARDOWN
/// table (the loaded rows plus the bundled floor), because the row this names may be one a newer
/// downloaded table dropped — the scrub still reached that harness, so the failure line still owes
/// a person its name.
fn display_name(slug: &str) -> String {
    topos_harness::registry::teardown_harnesses()
        .iter()
        .find(|h| h.slug == slug)
        .map_or_else(|| slug.to_owned(), |h| h.display_name.to_owned())
}

/// The MACHINE scope's MCP config view, for both phases. A project checkout keeps its own store
/// and its own ledger, and `uninstall` deletes neither — so this scope is the machine's, and only
/// the machine's. No roots (no `$HOME`) means no config surface resolves at all.
fn mcp_io<'a>(ctx: &'a Ctx<'a>) -> Option<crate::mcp_engine::ScopeIo<'a>> {
    let roots = ctx.roots.as_ref()?;
    Some(crate::mcp_engine::ScopeIo {
        fs: ctx.fs,
        runtimes: &crate::mcp_render::PathRuntimes,
        layout: &ctx.layout,
        home: roots.home.clone(),
        project_root: None,
    })
}

/// The read-only split the DESCRIBE promises: the MCP config files this teardown will take
/// topos-placed entries out of, and the ones whose hand-edited entries it will leave.
fn mcp_surfaces(ctx: &Ctx<'_>) -> crate::mcp_engine::RecordedSurfaces {
    let Some(io) = mcp_io(ctx) else {
        return crate::mcp_engine::RecordedSurfaces::default();
    };
    crate::mcp_engine::recorded_surfaces(
        &io,
        &topos_harness::mcp::descriptor::mcp_harnesses_for_teardown(),
    )
}

/// Retire every ledger-recorded topos MCP entry from this machine's agent configs, bundle by
/// bundle, through the SAME [`crate::mcp_engine::remove_bundle`] mechanics the `remove` verb runs:
/// only prior-matched keys move, a drifted entry is left byte-identical, and a wholly-topos-owned
/// file follows the existing last-entry-deletion rule. Nothing here is special-cased for the
/// teardown — the never-clobber rules do not lapse because the command is `uninstall`.
///
/// Best-effort by construction: the rows land on the receipt and the failures on the message
/// channel; neither blocks the teardown, which has a sidecar to delete either way.
fn scrub_mcp_entries(ctx: &Ctx<'_>, applied: &mut UninstallApplied) -> Vec<Message> {
    let Some(io) = mcp_io(ctx) else {
        return Vec::new();
    };
    let descriptors = topos_harness::mcp::descriptor::mcp_harnesses_for_teardown();
    let detected: std::collections::BTreeSet<String> = topos_harness::registry::detected_harnesses(
        &io.home,
        ctx.roots.as_ref().and_then(|r| r.cwd.as_deref()),
    )
    .iter()
    .map(|h| h.slug.to_owned())
    .collect();
    let mut messages = Vec::new();
    for bundle_id in crate::mcp_engine::recorded_bundles(&io) {
        let name = bundle_display_name(ctx, &bundle_id);
        let outcome =
            crate::mcp_engine::remove_bundle(&io, &descriptors, &detected, &bundle_id, &name);
        for removed in &outcome.removed {
            let Some(file) = removed.state.file.clone() else {
                continue;
            };
            match removed.state.state {
                topos_types::results::TargetOutcome::Drifted => applied.mcp_drifted.push(file),
                _ => applied.mcp_files.push(file),
            }
        }
        messages.extend(outcome.warnings);
    }
    for rows in [&mut applied.mcp_files, &mut applied.mcp_drifted] {
        rows.sort();
        rows.dedup();
    }
    messages
}

/// The word a person calls a bundle, read off its own record; the opaque id is the fallback for a
/// record this teardown can no longer read (which is exactly the state an uninstall runs in).
fn bundle_display_name(ctx: &Ctx<'_>, bundle_id: &str) -> String {
    crate::id::SkillId::parse(bundle_id)
        .ok()
        .and_then(|sid| {
            crate::doc::read_doc::<topos_types::persisted::Lock>(
                ctx.fs,
                &ctx.layout.published(&sid).lock,
            )
            .ok()
            .flatten()
        })
        .map_or_else(|| bundle_id.to_owned(), |lock| lock.name)
}

/// Whether `home` looks like a topos sidecar: any of the layout's own entries present, or an
/// EMPTY directory (a fresh/never-used home — harmless to remove either way). A read failure
/// counts as "does not look like one" (fail closed — never delete what could not be inspected).
fn looks_like_sidecar(ctx: &Ctx<'_>, home: &std::path::Path) -> bool {
    let markers = [
        ctx.layout.skills_dir(),
        ctx.layout.identity_dir(),
        ctx.layout.ops_dir(),
        ctx.layout.state_dir(),
        ctx.layout.locks_dir(),
        ctx.layout.log_path(),
    ];
    if markers.iter().any(|m| ctx.fs.exists(m)) {
        return true;
    }
    matches!(ctx.fs.read_dir(home), Ok(entries) if entries.is_empty())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
    use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
    use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

    use super::*;
    use crate::fs_seam::RealFs;
    use crate::ids::test_sources::{FixedClock, SeqIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;

    /// A self-cleaning scratch dir.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("topos-uninstall-ut-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A harness fake carrying BOTH ports: placement targets nothing real, and the trigger half
    /// RECORDS whether `remove` was called and discloses one fixed artifact (a config path). It
    /// touches no real config — the op orchestration is what's under test.
    struct FakeHarness {
        config: PathBuf,
        removed: Cell<u32>,
    }
    impl HarnessAdapter for FakeHarness {
        fn id(&self) -> HarnessId {
            HarnessId::ClaudeCode
        }

        fn discover(&self) -> Vec<DiscoveredPlacement> {
            Vec::new()
        }

        fn placement_for(
            &self,
            skill_id: &str,
            _n: topos_harness::PlacementNaming<'_>,
            _d: Option<&DiscoveredPlacement>,
        ) -> PlacementTarget {
            PlacementTarget {
                dir: PathBuf::from("/nonexistent").join(skill_id),
            }
        }
    }

    impl TriggerAdapter for FakeHarness {
        fn slug(&self) -> &'static str {
            HarnessId::ClaudeCode.slug()
        }

        fn install(&self) -> TriggerReport {
            self.report(TriggerState::Active, true)
        }

        fn remove(&self) -> TriggerReport {
            self.removed.set(self.removed.get() + 1);
            self.report(TriggerState::Inactive, true)
        }

        fn artifacts(&self) -> Vec<TriggerArtifact> {
            vec![TriggerArtifact::Path(self.config.clone())]
        }

        fn present(&self) -> bool {
            !self.artifacts().is_empty()
        }
    }
    impl FakeHarness {
        fn report(&self, state: TriggerState, touched: bool) -> TriggerReport {
            TriggerReport {
                agent: "claude-code".to_owned(),
                currency_kind: CurrencyKind::SessionStart,
                touched_path: touched.then(|| self.config.to_string_lossy().into_owned()),
                marker_id: "topos:test".into(),
                state,
                note: None,
            }
        }
    }

    /// A trigger fake for ANOTHER harness — the breadth half. It RECORDS its removals through a
    /// shared counter (the rig keeps a handle after the adapter is boxed) and discloses whichever
    /// artifact the test hands it, so a rig can state an out-of-process breadth without a scheduler.
    struct OtherTrigger {
        slug: &'static str,
        artifact: TriggerArtifact,
        removed: Rc<Cell<u32>>,
        /// What the scrub reports — `Inactive` for a clean removal, `Degraded` for one that could
        /// not happen (the receipt's whole subject in the partial-teardown rigs).
        state: TriggerState,
        /// The reason a degraded scrub carries, exactly as a real adapter's `note` does.
        note: Option<&'static str>,
        /// The config file this trigger lives in — `None` models a trigger living in the
        /// harness's own program.
        config: Option<PathBuf>,
    }
    impl OtherTrigger {
        /// A clean, file-less breadth trigger (the historic rig's shape).
        fn clean(slug: &'static str, artifact: TriggerArtifact, removed: Rc<Cell<u32>>) -> Self {
            Self {
                slug,
                artifact,
                removed,
                state: TriggerState::Inactive,
                note: None,
                config: None,
            }
        }
    }
    impl TriggerAdapter for OtherTrigger {
        fn slug(&self) -> &'static str {
            self.slug
        }

        fn install(&self) -> TriggerReport {
            self.report()
        }

        fn remove(&self) -> TriggerReport {
            self.removed.set(self.removed.get() + 1);
            self.report()
        }

        fn artifacts(&self) -> Vec<TriggerArtifact> {
            vec![self.artifact.clone()]
        }

        fn present(&self) -> bool {
            true
        }

        fn config_file(&self) -> Option<PathBuf> {
            self.config.clone()
        }
    }
    impl OtherTrigger {
        fn report(&self) -> TriggerReport {
            TriggerReport {
                agent: self.slug.to_owned(),
                currency_kind: CurrencyKind::ExplicitPullOnly,
                touched_path: None,
                marker_id: "topos:test:other".into(),
                state: self.state,
                note: self.note.map(str::to_owned),
            }
        }
    }

    fn ctx_with<'a>(
        fs: &'a RealFs,
        ids: &'a SeqIds,
        clock: &'a FixedClock,
        harness: &'a FakeHarness,
        plane: &'a InertPlane,
        follow: &'a InertFollow,
        home: &Path,
    ) -> Ctx<'a> {
        // The same fake on both ports, no breadth — most of this rig's subject is the verb's
        // orchestration of the active pair.
        ctx_with_triggers(
            fs,
            ids,
            clock,
            harness,
            plane,
            follow,
            home,
            crate::ops::Triggers::active_only(harness),
        )
    }

    #[allow(clippy::too_many_arguments)] // A composition root's rig: every port is passed, none resolved.
    fn ctx_with_triggers<'a>(
        fs: &'a RealFs,
        ids: &'a SeqIds,
        clock: &'a FixedClock,
        harness: &'a FakeHarness,
        plane: &'a InertPlane,
        follow: &'a InertFollow,
        home: &Path,
        triggers: crate::ops::Triggers<'a>,
    ) -> Ctx<'a> {
        Ctx {
            progress: crate::progress::silent(),
            fs,
            ids,
            clock,
            // Uninstall must NEVER require or mint identity — an empty device id models the app dispatch.
            device_id: String::new(),
            layout: Layout::new(home),
            harness,
            triggers,
            plane,
            follow,
            roots: None,
        }
    }

    #[test]
    fn describe_lists_the_hook_and_sidecar_and_mutates_nothing() {
        let home = Scratch::new();
        let cfg = home.0.join("harness-settings.json");
        // A file that stands in for a skill living OUTSIDE `~/.topos/` — it must survive.
        let skill_file = Scratch::new();
        std::fs::write(skill_file.0.join("SKILL.md"), b"keep me").unwrap();

        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = FakeHarness {
            config: cfg.clone(),
            removed: Cell::new(0),
        };
        let plane = InertPlane;
        let follow = InertFollow;
        let ctx = ctx_with(&fs, &ids, &clock, &harness, &plane, &follow, &home.0);

        let bin = Some(PathBuf::from("/usr/local/bin/topos"));
        let out = uninstall(&ctx, bin.clone(), false).unwrap();
        match out {
            UninstallOutcome::Described { describe, yes_argv } => {
                assert_eq!(
                    describe.trigger_artifacts,
                    vec![cfg.to_string_lossy().into_owned()]
                );
                assert_eq!(describe.sidecar_path, home.0.to_string_lossy());
                assert!(describe.sidecar_present);
                assert_eq!(
                    describe.binary_path.as_deref(),
                    Some("/usr/local/bin/topos")
                );
                assert_eq!(yes_argv.last().map(String::as_str), Some("--yes"));
            }
            UninstallOutcome::Applied { .. } => panic!("a bare uninstall describes"),
        }
        // A describe mutates nothing: the sidecar home stays, the hook was never scrubbed.
        assert!(
            home.0.exists(),
            "the sidecar tree is untouched by a describe"
        );
        assert_eq!(harness.removed.get(), 0, "a describe scrubs no hook");
        assert!(
            skill_file.0.join("SKILL.md").exists(),
            "skill files untouched"
        );
    }

    #[test]
    fn yes_scrubs_the_hook_and_removes_the_sidecar_and_is_idempotent() {
        let home = Scratch::new();
        // Seed a sidecar tree with a nested file (a stand-in credential).
        std::fs::create_dir_all(home.0.join("identity")).unwrap();
        std::fs::write(home.0.join("identity/credentials.json"), b"secret").unwrap();
        let cfg = home.0.join("harness-settings.json");
        // A skill file OUTSIDE `~/.topos/` must survive the uninstall.
        let skill_file = Scratch::new();
        std::fs::write(skill_file.0.join("SKILL.md"), b"keep me").unwrap();

        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = FakeHarness {
            config: cfg.clone(),
            removed: Cell::new(0),
        };
        let plane = InertPlane;
        let follow = InertFollow;
        let ctx = ctx_with(&fs, &ids, &clock, &harness, &plane, &follow, &home.0);

        let out = uninstall(&ctx, Some(PathBuf::from("/usr/local/bin/topos")), true).unwrap();
        match out {
            UninstallOutcome::Applied { applied, messages } => {
                assert_eq!(harness.removed.get(), 1, "the auto-update hook is scrubbed");
                assert_eq!(
                    applied.hook.as_ref().map(|h| h.state),
                    Some(TriggerState::Inactive)
                );
                assert!(applied.sidecar_removed, "the sidecar tree is deleted");
                assert!(messages.is_empty(), "a clean teardown reports no failure");
            }
            UninstallOutcome::Described { .. } => panic!("--yes applies"),
        }
        assert!(!home.0.exists(), "the sidecar tree is gone");
        assert!(
            skill_file.0.join("SKILL.md").exists(),
            "skill files untouched"
        );

        // A SECOND run is graceful: nothing to delete (the tree is already gone).
        let out = uninstall(&ctx, None, true).unwrap();
        match out {
            UninstallOutcome::Applied { applied, .. } => {
                assert!(!applied.sidecar_removed, "nothing left to remove");
            }
            UninstallOutcome::Described { .. } => panic!("--yes applies"),
        }
    }

    #[test]
    fn a_home_that_does_not_look_like_a_sidecar_is_refused_on_both_phases() {
        // A mispointed TOPOS_HOME (e.g. `TOPOS_HOME=$HOME`): the directory EXISTS and is non-empty
        // but carries none of the sidecar's own entries. Deleting it would be an unbounded loss —
        // both the describe and the apply refuse typed, and nothing is touched.
        let home = Scratch::new();
        std::fs::write(home.0.join("precious.txt"), b"not topos data").unwrap();

        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = FakeHarness {
            config: home.0.join("harness-settings.json"),
            removed: Cell::new(0),
        };
        let plane = InertPlane;
        let follow = InertFollow;
        let ctx = ctx_with(&fs, &ids, &clock, &harness, &plane, &follow, &home.0);

        for yes in [false, true] {
            let failure = uninstall(&ctx, None, yes).expect_err("the fence refuses");
            assert_eq!(failure.error.code(), "INVALID_ARGUMENT", "yes={yes}");
            assert!(
                failure.partial.is_none(),
                "nothing was touched, so there is no receipt to print"
            );
            let msg = crate::render::safe_message(&failure.error);
            assert!(msg.contains("does not look like a topos sidecar"), "{msg}");
            assert!(msg.contains("TOPOS_HOME"), "{msg}");
        }
        assert!(
            home.0.join("precious.txt").exists(),
            "nothing was deleted behind the fence"
        );
        assert_eq!(
            harness.removed.get(),
            0,
            "the hook was never scrubbed either"
        );
    }

    /// The describe names an OUT-OF-PROCESS trigger in words. A harness whose trigger lives in its
    /// own program leaves no path to print, and a preview that prints only paths says nothing at all
    /// about it — then `--yes` reaches into that program anyway. The row is the promise the apply
    /// answers for.
    #[test]
    fn the_describe_names_a_trigger_that_lives_outside_the_filesystem() {
        let home = Scratch::new();
        std::fs::create_dir_all(home.0.join("identity")).unwrap();

        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = FakeHarness {
            config: home.0.join("harness-settings.json"),
            removed: Cell::new(0),
        };
        let plane = InertPlane;
        let follow = InertFollow;
        let removed = Rc::new(Cell::new(0));
        let others: Vec<Box<dyn TriggerAdapter>> = vec![Box::new(OtherTrigger::clean(
            "openclaw",
            TriggerArtifact::OutOfProcess {
                harness: "OpenClaw",
            },
            Rc::clone(&removed),
        ))];
        let ctx = ctx_with_triggers(
            &fs,
            &ids,
            &clock,
            &harness,
            &plane,
            &follow,
            &home.0,
            crate::ops::Triggers::machine_of(&harness, &others),
        );

        let out = uninstall(&ctx, None, false).unwrap();
        let UninstallOutcome::Described { describe, .. } = out else {
            panic!("a bare uninstall describes")
        };
        assert!(
            describe.trigger_artifacts.iter().any(|row| row
                == "the OpenClaw scheduled update job, if registered (removed through OpenClaw's \
                    scheduler)"),
            "the out-of-process trigger must be named: {:?}",
            describe.trigger_artifacts
        );
        assert_eq!(removed.get(), 0, "a describe scrubs nothing");
    }

    /// The teardown ORDER: EVERY trigger scrub runs only after every fallible deletion SUCCEEDED.
    /// A deletion that fails aborts the verb — and must leave the machine fully armed, the ACTIVE
    /// harness included, because disarming a machine's agents is not a thing to do on the way out
    /// of a teardown that did not happen. The active scrub used to run first, so exactly the
    /// harness the person is sitting in front of was the one left silently un-updated.
    ///
    /// And the failure OWES A RECEIPT: it deleted the built-in copies on its way down, and used to
    /// report nothing but "a filesystem operation failed" with no path anywhere.
    #[test]
    fn a_failed_deletion_leaves_every_harnesss_trigger_armed_and_still_reports_its_work() {
        use topos_types::persisted::{PlacementKind, PlacementMap, PlacementState, SwapCapability};

        let home = Scratch::new();
        std::fs::create_dir_all(home.0.join("identity")).unwrap();
        std::fs::write(home.0.join("identity/credentials.json"), b"secret").unwrap();
        // The built-in skill's recorded placement is a regular FILE where a directory belongs (a
        // corrupted agent dir) — so the tree delete fails, deterministically and for every user.
        let placed = Scratch::new();
        let placement = placed.0.join("topos");
        std::fs::write(&placement, b"not a directory").unwrap();
        let sid = crate::id::SkillId::parse("topos").unwrap();
        let layout = Layout::new(&home.0);
        std::fs::create_dir_all(layout.published(&sid).map.parent().unwrap()).unwrap();
        crate::doc::write_map(
            &RealFs,
            &layout.published(&sid).map,
            &PlacementMap {
                schema_version: 2,
                placements: vec![placement.to_string_lossy().into_owned()],
                applied_commit: "b".repeat(64),
                materialized_sha: "e".repeat(64),
                harness: None,
                harness_slug: None,
                placement_state: vec![PlacementState {
                    kind: PlacementKind::Native,
                    agent: None,
                    materialized_sha: Some("e".repeat(64)),
                    pre_existing_sha: None,
                    swap_capability: SwapCapability::Unsupported,
                    adopted_source: false,
                    claim: None,
                }],
            },
        )
        .unwrap();

        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = FakeHarness {
            config: home.0.join("harness-settings.json"),
            removed: Cell::new(0),
        };
        let plane = InertPlane;
        let follow = InertFollow;
        let removed = Rc::new(Cell::new(0));
        let others: Vec<Box<dyn TriggerAdapter>> = vec![Box::new(OtherTrigger::clean(
            "cursor",
            TriggerArtifact::Path(home.0.join("cursor-hooks.json")),
            Rc::clone(&removed),
        ))];
        let ctx = ctx_with_triggers(
            &fs,
            &ids,
            &clock,
            &harness,
            &plane,
            &follow,
            &home.0,
            crate::ops::Triggers::machine_of(&harness, &others),
        );

        let failure = uninstall(&ctx, None, true).expect_err("the failed deletion fails the verb");

        assert_eq!(
            removed.get(),
            0,
            "the breadth scrub never ran — the other harness keeps its trigger"
        );
        assert_eq!(
            harness.removed.get(),
            0,
            "the ACTIVE harness keeps its trigger too — a failed teardown disarms nobody"
        );
        assert!(
            home.0.join("identity/credentials.json").exists(),
            "the sidecar tree is intact: the verb died before its delete"
        );
        // The receipt of the work that DID happen — the same rows the success receipt spells.
        let partial = failure
            .partial
            .expect("a failure that deleted things owes a receipt");
        assert!(
            partial.hook.is_none(),
            "no scrub ran, so the receipt claims none"
        );
        assert!(!partial.sidecar_removed);
        // The error names the failing PATH in its diagnostic detail (the log + `details:` pointer),
        // where "a filesystem operation failed" alone left nothing to act on.
        assert_eq!(failure.error.code(), "IO_ERROR");
        assert!(
            failure
                .error
                .detail()
                .contains(&placement.to_string_lossy().into_owned()),
            "the detail names the path that failed: {}",
            failure.error.detail()
        );
    }

    /// A teardown that could not scrub a trigger DID NOT uninstall topos. It used to print
    /// `Uninstalled topos.`, exit 0 and hand `--json` an `ok: true` with an empty `warnings` —
    /// while an agent on that machine went on auto-updating from a topos that was supposedly gone.
    #[test]
    fn a_trigger_it_could_not_scrub_makes_the_teardown_incomplete() {
        let home = Scratch::new();
        std::fs::create_dir_all(home.0.join("identity")).unwrap();

        let fs = RealFs;
        let ids = SeqIds::new("s");
        let clock = FixedClock(1);
        let harness = FakeHarness {
            config: home.0.join("harness-settings.json"),
            removed: Cell::new(0),
        };
        let plane = InertPlane;
        let follow = InertFollow;
        let cursor_config = home.0.join("cursor-hooks.json");
        let removed = Rc::new(Cell::new(0));
        let others: Vec<Box<dyn TriggerAdapter>> = vec![
            // A config file topos could read but not write.
            Box::new(OtherTrigger {
                slug: "cursor",
                artifact: TriggerArtifact::Path(cursor_config.clone()),
                removed: Rc::clone(&removed),
                state: TriggerState::Degraded,
                note: Some("permission denied"),
                config: Some(cursor_config.clone()),
            }),
            // A trigger living in the harness's OWN scheduler, unreachable from here.
            Box::new(OtherTrigger {
                slug: "openclaw",
                artifact: TriggerArtifact::OutOfProcess {
                    harness: "OpenClaw",
                },
                removed: Rc::clone(&removed),
                state: TriggerState::Degraded,
                note: None,
                config: None,
            }),
        ];
        let ctx = ctx_with_triggers(
            &fs,
            &ids,
            &clock,
            &harness,
            &plane,
            &follow,
            &home.0,
            crate::ops::Triggers::machine_of(&harness, &others),
        );

        let out = uninstall(&ctx, None, true).unwrap();
        let UninstallOutcome::Applied { applied, messages } = out else {
            panic!("--yes applies")
        };
        assert_eq!(messages.len(), 2, "one failure per harness: {messages:?}");
        for m in &messages {
            assert_eq!(m.kind, topos_types::MessageKind::Failure);
            assert_eq!(m.code.as_deref(), Some("TRIGGER_NOT_REMOVED"));
        }
        let mut cursor_line = format!(
            "cursor: the auto-update trigger could not be removed from {}",
            cursor_config.display()
        );
        cursor_line.push_str(" (permission denied) — remove it by hand, or fix the permission ");
        cursor_line.push_str("and run 'topos uninstall' again.");
        assert_eq!(messages[0].text, cursor_line);
        let mut openclaw_line =
            String::from("openclaw: the scheduled update job could not be checked or removed — ");
        openclaw_line.push_str("the openclaw command is not available on this machine. If ");
        openclaw_line.push_str("OpenClaw is installed, remove the job with its own scheduler.");
        assert_eq!(messages[1].text, openclaw_line);
        // The degraded JSON rows carry the same reason as a note.
        assert_eq!(
            applied
                .triggers
                .iter()
                .find(|t| t.agent == "cursor")
                .and_then(|t| t.note.as_deref()),
            Some("permission denied")
        );
        // The headline does not claim completion, and each failure states its own way out.
        let tty = crate::render::uninstall_applied_tty(&applied, &messages, false);
        assert!(
            tty.starts_with("Uninstall incomplete — topos could not remove everything it placed."),
            "{tty}"
        );
        assert!(tty.contains("permission denied"), "{tty}");
        assert!(tty.contains("its own scheduler."), "{tty}");
        // A CLEAN teardown still leads with the plain headline and carries no failures.
        let clean = crate::render::uninstall_applied_tty(&applied, &[], true);
        assert!(clean.starts_with("Uninstalled topos."), "{clean}");
    }
}
