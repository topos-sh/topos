//! `uninstall [--yes]` — remove topos from this machine, two-phase.
//!
//! Bare = a DESCRIBE of exactly what goes: EVERY harness auto-update-trigger artifact the apply
//! reaches, not just the active harness's — each named by its path, or, where a harness keeps the
//! trigger in its own program instead of a file, by what the scrub will dial there — the
//! `~/.topos/` sidecar tree (which holds the signed-in credential), and the note that SKILL FILES IN
//! AGENT DIRS STAY (uninstall never deletes a skill byte). `--yes` scrubs the active harness's
//! trigger, deletes the built-in skill's copies and then the `~/.topos/` tree via the fs seam, and
//! LAST scrubs every other supported harness's trigger (all reports surfaced honestly) — last
//! because those deletions can fail, and a teardown that dies halfway must not have disarmed the
//! machine's other agents on its way. The `topos` binary is NOT self-deleted (a package manager may
//! own it) — its path is disclosed with a "remove it with your installer (or `rm <path>`)" note. A
//! maintenance command: it needs no sign-in, mints no identity, and touches no plane.

use std::path::PathBuf;

use serde::Serialize;
use topos_types::TriggerReport;

use crate::ctx::Ctx;
use crate::error::ClientError;

/// The bare `uninstall` DESCRIBE — what `--yes` would remove (nothing has changed).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UninstallDescribe {
    /// Every artifact the auto-update-trigger scrub would REACH, across EVERY harness the apply
    /// touches — not just the active one, and not only the ones that leave a file behind: a path
    /// where the trigger is one, and the named registration where it lives in the harness's own
    /// program (empty = nothing anywhere). Rendered rows, in the disclosure's own order and
    /// wording — the preview says what the apply will attempt, so a person reads the same sentence
    /// the receipt answers for.
    pub trigger_artifacts: Vec<String>,
    /// The `~/.topos/` sidecar tree that would be deleted (the signed-in credential lives inside it).
    pub sidecar_path: String,
    /// Whether the sidecar tree currently exists (a fresh/already-removed install has none).
    pub sidecar_present: bool,
    /// The running binary's own path — NOT deleted; disclosed so the human can remove it themselves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// The BUILT-IN `topos` skill's placed copies — topos-authored artifacts (like the hook entry),
    /// so they go with the teardown; YOUR skill files still stay untouched.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub builtin_dirs: Vec<String>,
}

/// The applied `uninstall` — what was removed.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UninstallApplied {
    /// The ACTIVE harness's trigger scrub report (surfaced honestly — `Inactive` when nothing was
    /// armed).
    pub hook: TriggerReport,
    /// The breadth scrub's outcomes — other agents whose trigger the sweep removed (or could not,
    /// disclosed); clean no-ops stay off the receipt.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<TriggerReport>,
    /// Whether the `~/.topos/` sidecar tree was deleted (false = there was nothing to delete).
    pub sidecar_removed: bool,
    /// The built-in `topos` skill's placed copies that were removed (topos-authored artifacts).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub builtin_dirs: Vec<String>,
    /// The running binary's own path — left in place; the human removes it with their installer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

/// The verb's outcome — the two-phase pair.
#[derive(Debug)]
pub(crate) enum UninstallOutcome {
    Described {
        describe: UninstallDescribe,
        yes_argv: Vec<String>,
    },
    Applied(UninstallApplied),
}

/// `uninstall [--yes]`. `binary_path` is the running executable's path (the composition root passes
/// `std::env::current_exe()`), disclosed but never deleted.
///
/// # Errors
/// An [`FsOps`](crate::fs_seam::FsOps) failure removing the sidecar tree.
pub(crate) fn uninstall(
    ctx: &Ctx<'_>,
    binary_path: Option<PathBuf>,
    yes: bool,
) -> Result<UninstallOutcome, ClientError> {
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
        )));
    }

    // The built-in `topos` skill's placed copies (best-effort read — a torn/newer sidecar must
    // never block the one command that deletes it).
    let builtin_dirs = super::builtin::placement_dirs(ctx).unwrap_or_default();

    if !yes {
        return Ok(UninstallOutcome::Described {
            describe: UninstallDescribe {
                trigger_artifacts,
                sidecar_path,
                sidecar_present,
                binary_path: binary,
                builtin_dirs,
            },
            yes_argv: vec![
                "topos".to_owned(),
                "uninstall".to_owned(),
                "--yes".to_owned(),
            ],
        });
    }

    // ---- APPLY (`--yes`) ----
    // The ACTIVE harness's trigger goes first (its artifact lives in the harness home, not
    // `~/.topos/`), then the built-in skill's placed copies (topos-authored, recorded in the sidecar
    // we are about to delete), then the sidecar tree itself. Idempotent: a second run finds nothing
    // to remove.
    let hook = ctx.triggers.active().remove();
    let mut builtin_removed = Vec::new();
    for dir in &builtin_dirs {
        let p = std::path::Path::new(dir);
        if ctx.fs.exists(p) {
            ctx.fs.remove_dir_all(p)?;
            builtin_removed.push(dir.clone());
        }
    }
    let sidecar_removed = if ctx.fs.exists(home) {
        ctx.fs.remove_dir_all(home)?;
        true
    } else {
        false
    };
    // The breadth scrub runs LAST — after every fallible deletion above has SUCCEEDED. Those
    // deletions can fail (a permission error, an unremovable tree), and this verb fails with them;
    // disarming the machine's other agents on the way out of a teardown that did not happen would
    // leave them silently un-updated with nothing removed to show for it. The set is exactly the one
    // the describe disclosed.
    let breadth = ctx.triggers.scrub_others();

    Ok(UninstallOutcome::Applied(UninstallApplied {
        hook,
        triggers: breadth,
        sidecar_removed,
        builtin_dirs: builtin_removed,
        binary_path: binary,
    }))
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
    }
    impl OtherTrigger {
        fn report(&self) -> TriggerReport {
            TriggerReport {
                agent: self.slug.to_owned(),
                currency_kind: CurrencyKind::ExplicitPullOnly,
                touched_path: None,
                marker_id: "topos:test:other".into(),
                state: TriggerState::Inactive,
                note: None,
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
            UninstallOutcome::Applied(_) => panic!("a bare uninstall describes"),
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
            UninstallOutcome::Applied(applied) => {
                assert_eq!(harness.removed.get(), 1, "the auto-update hook is scrubbed");
                assert_eq!(applied.hook.state, TriggerState::Inactive);
                assert!(applied.sidecar_removed, "the sidecar tree is deleted");
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
            UninstallOutcome::Applied(applied) => {
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
            let err = uninstall(&ctx, None, yes).expect_err("the fence refuses");
            assert_eq!(err.code(), "INVALID_ARGUMENT", "yes={yes}");
            let msg = crate::render::safe_message(&err);
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
        let others: Vec<Box<dyn TriggerAdapter>> = vec![Box::new(OtherTrigger {
            slug: "openclaw",
            artifact: TriggerArtifact::OutOfProcess {
                harness: "OpenClaw",
            },
            removed: Rc::clone(&removed),
        })];
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
                == "the OpenClaw scheduled update job, if armed (removed through OpenClaw's \
                    scheduler)"),
            "the out-of-process trigger must be named: {:?}",
            describe.trigger_artifacts
        );
        assert_eq!(removed.get(), 0, "a describe scrubs nothing");
    }

    /// The teardown ORDER: the breadth scrub runs only after every fallible deletion SUCCEEDED. A
    /// deletion that fails aborts the verb — and must leave every OTHER harness's trigger armed,
    /// because disarming a machine's agents is not a thing to do on the way out of a teardown that
    /// did not happen.
    #[test]
    fn a_failed_deletion_leaves_every_other_harnesss_trigger_armed() {
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
                pre_existing_sha: None,
                swap_capability: SwapCapability::Unsupported,
                harness: None,
                harness_layer: None,
                harness_slug: None,
                placement_state: vec![PlacementState {
                    kind: PlacementKind::Native,
                    agent: None,
                    materialized_sha: Some("e".repeat(64)),
                    pre_existing_sha: None,
                    swap_capability: SwapCapability::Unsupported,
                    adopted_source: false,
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
        let others: Vec<Box<dyn TriggerAdapter>> = vec![Box::new(OtherTrigger {
            slug: "cursor",
            artifact: TriggerArtifact::Path(home.0.join("cursor-hooks.json")),
            removed: Rc::clone(&removed),
        })];
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

        uninstall(&ctx, None, true).expect_err("the failed deletion fails the verb");

        assert_eq!(
            removed.get(),
            0,
            "the breadth scrub never ran — the other harness keeps its trigger"
        );
        assert!(
            home.0.join("identity/credentials.json").exists(),
            "the sidecar tree is intact: the verb died before its delete"
        );
    }
}
