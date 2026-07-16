//! `uninstall [--yes]` — remove topos from this machine, two-phase.
//!
//! Bare = a DESCRIBE of exactly what goes: the harness currency-hook entry (named by its config path),
//! the `~/.topos/` sidecar tree (which holds the signed-in credential), and the note that SKILL FILES IN
//! AGENT DIRS STAY (uninstall never deletes a skill byte). `--yes` scrubs the currency hook
//! (`remove_currency_trigger`, whose report is surfaced honestly), then deletes the `~/.topos/` tree via
//! the fs seam. The `topos` binary is NOT self-deleted (a package manager may own it) — its path is
//! disclosed with a "remove it with your installer (or `rm <path>`)" note. A maintenance command: it needs
//! no sign-in, mints no identity, and touches no plane.

use std::path::PathBuf;

use serde::Serialize;
use topos_types::TriggerReport;

use crate::ctx::Ctx;
use crate::error::ClientError;

/// The bare `uninstall` DESCRIBE — what `--yes` would remove (nothing has changed).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UninstallDescribe {
    /// The harness config path(s) the currency hook would be scrubbed from (empty = none armed).
    pub hook_paths: Vec<String>,
    /// The `~/.topos/` sidecar tree that would be deleted (the signed-in credential lives inside it).
    pub sidecar_path: String,
    /// Whether the sidecar tree currently exists (a fresh/already-removed install has none).
    pub sidecar_present: bool,
    /// The running binary's own path — NOT deleted; disclosed so the human can remove it themselves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

/// The applied `uninstall` — what was removed.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UninstallApplied {
    /// The currency-hook scrub report (surfaced honestly — `Inactive` when nothing was armed).
    pub hook: TriggerReport,
    /// Whether the `~/.topos/` sidecar tree was deleted (false = there was nothing to delete).
    pub sidecar_removed: bool,
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
    let hook_paths: Vec<String> = ctx
        .harness
        .uninstall_footprint()
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
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

    if !yes {
        return Ok(UninstallOutcome::Described {
            describe: UninstallDescribe {
                hook_paths,
                sidecar_path,
                sidecar_present,
                binary_path: binary,
            },
            yes_argv: vec![
                "topos".to_owned(),
                "uninstall".to_owned(),
                "--yes".to_owned(),
            ],
        });
    }

    // ---- APPLY (`--yes`) ----
    // Scrub the currency hook FIRST (its config lives in the harness home, not `~/.topos/`), then delete
    // the sidecar tree. Idempotent: a second run finds no hook to remove and no sidecar to delete.
    let hook = ctx.harness.remove_currency_trigger();
    let sidecar_removed = if ctx.fs.exists(home) {
        ctx.fs.remove_dir_all(home)?;
        true
    } else {
        false
    };

    Ok(UninstallOutcome::Applied(UninstallApplied {
        hook,
        sidecar_removed,
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
    use std::sync::atomic::{AtomicU32, Ordering};

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

    /// A harness fake that RECORDS whether `remove_currency_trigger` was called and reports a fixed
    /// footprint (a config path). It touches no real config — the op orchestration is what's under test.
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
        fn currency_kind(&self) -> CurrencyKind {
            CurrencyKind::SessionStart
        }
        fn install_currency_trigger(&self) -> TriggerReport {
            self.report(TriggerState::Active, true)
        }
        fn remove_currency_trigger(&self) -> TriggerReport {
            self.removed.set(self.removed.get() + 1);
            self.report(TriggerState::Inactive, true)
        }
        fn uninstall_footprint(&self) -> Vec<PathBuf> {
            vec![self.config.clone()]
        }
    }
    impl FakeHarness {
        fn report(&self, state: TriggerState, touched: bool) -> TriggerReport {
            TriggerReport {
                harness: HarnessId::ClaudeCode,
                currency_kind: CurrencyKind::SessionStart,
                touched_path: touched.then(|| self.config.to_string_lossy().into_owned()),
                marker_id: "topos:test".into(),
                state,
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
        Ctx {
            fs,
            ids,
            clock,
            // Uninstall must NEVER require or mint identity — an empty device id models the app dispatch.
            device_id: String::new(),
            layout: Layout::new(home),
            harness,
            plane,
            follow,
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
                    describe.hook_paths,
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
                assert_eq!(harness.removed.get(), 1, "the currency hook is scrubbed");
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
}
