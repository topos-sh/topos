//! The forge arms: the silent round's own clock and budget, the probe that never downloads, the
//! deleted/renamed/emptied repo verdicts and how long they stick, the per-scope schedule, and the
//! member-level refresh refusals a sibling's move may never settle.

use std::sync::{Arc, Mutex};

use topos_types::results::PullAction;

use crate::error::FetchFault;
use crate::fs_seam::FsOps;
use crate::ops;
use crate::plane::DeliverySnapshot;
use crate::sessions::{self, SESSION_ENDED};

use super::rig::*;

// =================================================================================================
// The forge arms.
// =================================================================================================

#[test]
fn a_floating_repo_row_advances_through_the_silent_sweep_alone() {
    // ACCEPTANCE 1. No human command anywhere in this test after the row is in place: the sweep
    // that runs at a session start is what carries a GitHub row to a new upstream commit, exactly
    // as it carries a workspace-delivered one.
    let rig = Rig::new("repo");
    // No session: a pure forge recipe.
    rig.write_global("[skills]\nalpha = \"github:o/r\"\nbeta = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v1\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
        ],
    ));

    // The FIRST sweep installs what the row demands — no prior `add`, no consent moment, no
    // ceremony: a committed row is the demand, and the automatic update honors it.
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let alpha = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(
        alpha.exists() && beta.exists(),
        "the sweep installs a cloned project's row: {:?}",
        out.warnings
    );

    // Upstream moves. Inside the interval the sweep does not even ask.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v2\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
            ("skills/gamma/SKILL.md", b"# gamma v1\n"),
        ],
    ));
    let (probes, fetches) = (git.probes(), git.fetches());
    let quiet = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        (git.probes(), git.fetches()),
        (probes, fetches),
        "inside the interval the silent sweep asks the forge nothing"
    );
    assert_eq!(
        std::fs::read_to_string(&alpha).unwrap(),
        "# alpha v1\n",
        "and nothing moves"
    );
    assert!(
        quiet
            .data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        quiet.data.skills
    );

    // Once the interval has elapsed, the NEXT session start lands it — and says what moved.
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let line = crate::message::legacy_lines(&out.disclosures)
        .into_iter()
        .find(|w| w.starts_with("GIT_UPDATED"))
        .unwrap_or_else(|| panic!("the moved-source line: {:?}", out.disclosures));
    assert!(line.contains("github.com/o/r"), "{line}");
    assert!(
        line.contains("aaaaaaaaaaaa"),
        "names the old commit: {line}"
    );
    assert!(
        line.contains("bbbbbbbbbbbb"),
        "names the new commit: {line}"
    );
    assert!(line.contains("~alpha"), "names the carried member: {line}");
    assert_eq!(
        std::fs::read_to_string(&alpha).unwrap(),
        "# alpha v2\n",
        "the silent sweep lands the new bytes"
    );
    // A member NO ROW NAMES is not delivered by the move: v2 spells one row per skill, so the
    // repo growing a `gamma` says nothing to this machine until a row asks for it.
    assert!(!line.contains("gamma"), "{line}");
    assert!(!rig.home.0.join(".claude/skills/gamma").exists());
}

#[test]
fn a_fresh_unpinned_repo_install_fills_the_lock_commit() {
    // The fill contract: the FIRST install of an unpinned repo row records the commit the fetch
    // resolved — bytes on disk with no lock entry would make the very next `--frozen` refuse
    // the checkout this run just converged.
    let rig = Rig::new("repo-lockfill");
    let proj = project("repo-lockfill-proj", "[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts {
            forge: ops::ForgeCadence::Scheduled,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();

    let text = std::fs::read_to_string(proj.0.join("topos.lock")).unwrap_or_default();
    assert!(text.contains("[skills.alpha]"), "{text}");
    assert!(text.contains("commit = \""), "{text}");
    assert!(text.contains("aaaaaaaaaaaa1"), "{text}");
    assert!(text.contains("github:o/r"), "{text}");
}

#[test]
fn an_unchanged_repo_is_probed_and_never_downloaded() {
    // ACCEPTANCE 2, asserted on the TRANSPORT SEAM rather than by timing: a repo that has not
    // moved costs one probe and zero archives. This is the whole reason the lane can run on a
    // clock at all — the check has to be cheap enough to make automatic.
    let rig = Rig::new("repo-cheap");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());

    let fetches = git.fetches();
    let probes = git.probes();
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.fetches(),
        fetches,
        "an unchanged repo downloads NOTHING: {:?}",
        out.warnings
    );
    assert_eq!(git.probes(), probes + 1, "it costs exactly one probe");
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn the_forge_clock_is_separate_from_the_workspace_cadence() {
    // ACCEPTANCE 3. In ONE run: the workspace lane dials (its own throttle governs it, and it is
    // not this one), while the forge lane — inside its much longer interval — asks nothing.
    let rig = Rig::new("repo-two-clocks");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\nalpha = \"github:o/r\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    quiet_sweep(&ctx, &plane, &dir, &git);
    let (probes, fetches) = (git.probes(), git.fetches());
    log.lock().unwrap().clear();

    // A second session start, inside the forge interval.
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        (git.probes(), git.fetches()),
        (probes, fetches),
        "zero forge requests inside the interval"
    );
    assert!(
        log.lock().unwrap().iter().any(|l| l == "report s_deploy"),
        "the workspace lane keeps its own cadence in the same run: {:?}",
        log.lock().unwrap()
    );
    assert!(
        out.data.skills.iter().any(|s| s.skill == "deploy"),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn a_failed_check_advances_the_clock_like_a_successful_one() {
    // ACCEPTANCE 4 — the highest-risk detail in the whole change. A check that FAILED has still
    // had its turn. If the clock only moved on success, one unreachable forge would be re-dialed
    // at every single session start, which is precisely the traffic the interval exists to
    // prevent. So: the lane ran, therefore it waits.
    let rig = Rig::new("repo-clock-on-failure");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // A round that reaches nothing at all.
    git.fail_with(FetchFault::Unreachable);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let attempted = git.probes() + git.fetches();
    assert!(attempted > 0, "the round really did try");
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);

    // The clock moved anyway — a whole interval out, exactly as a successful round would leave it.
    assert_eq!(
        forge_next_due(&rig, "github.com/o/r", "", "person"),
        rig_now(&rig) + crate::forge_check::CHECK_INTERVAL_MS,
        "a failed round waits exactly as long as one that worked"
    );

    // THE REGRESSION: the next session start inside the window asks nothing and says nothing.
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        attempted,
        "the failure is not re-dialed at the next session start"
    );
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert!(lines.is_empty(), "and emits no line: {lines:?}");
}

#[test]
fn a_dead_network_costs_one_timeout_for_the_whole_round() {
    // ACCEPTANCE 5. Five rows, one dead forge: the breaker short-circuits after the first fault
    // that never reached it, so a session start pays ONE connect timeout rather than five. The
    // clock still advances, and every row is recorded as checked — a skipped source had its turn.
    let rig = Rig::new("repo-breaker");
    rig.write_global(
        "[skills]\na = \"github:o/a\"\nb = \"github:o/b\"\n\
         c = \"github:o/c\"\nd = \"github:o/d\"\ne = \"github:o/e\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Unreachable);

    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        1,
        "ONE dial for five rows behind one dead forge"
    );
    // Every source is nonetheless recorded as checked this round, so none of them comes straight
    // back at the next session start.
    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    assert_eq!(doc.sources.len(), 5, "{:?}", doc.sources);
    assert!(
        doc.sources
            .values()
            .all(|c| c.checked_at_ms == rig_now(&rig)),
        "{:?}",
        doc.sources
    );
    assert!(
        doc.sources.values().all(|c| c
            .next_check_at
            .values()
            .all(|at| *at == rig_now(&rig) + crate::forge_check::CHECK_INTERVAL_MS)),
        "each skipped source schedules its own next turn: {:?}",
        doc.sources
    );
    // The skipped rows say nothing of their own: the source that actually failed already did, and
    // "we skipped this because something else broke" is noise about someone else's problem.
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("already unreachable")),
        "{:?}",
        out.warnings
    );

    // A forge that ANSWERS about one repo never trips it — the answer says nothing about the next.
    let rig = Rig::new("repo-breaker-http");
    rig.write_global("[skills]\na = \"github:o/a\"\nb = \"github:o/b\"\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::unavailable());
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        2,
        "an HTTP-level answer about one repo must not short-circuit the other"
    );
}

#[test]
fn a_machine_with_no_prior_grant_auto_updates_a_cloned_projects_row() {
    // ACCEPTANCE 6. The first-trust registry is gone: a committed row IS the demand, and a fresh
    // machine that has never seen the source converges it automatically. The interactive `add`
    // keeps its describe — that is where a person is present to read the answer.
    let rig = Rig::new("repo-no-grant");
    let proj = project("proj-cloned", "[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let pctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    // Nothing has ever granted this origin, and no such registry exists to grant it.
    assert!(!rig.layout().state_dir().join("forge_trust.json").exists());

    let out = quiet_sweep(&pctx, &plane, &dir, &git);
    assert!(
        proj.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "the clone's row converges on its own: {:?}",
        out.warnings
    );
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("FIRST_TRUST")),
        "{:?}",
        out.warnings
    );
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).is_some(),
        "installed through the project's own store"
    );

    // The interactive add still DESCRIBES first — including for this now-tracked source.
    let outcome = ops::add_reference(
        &pctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/alpha",
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap();
    match outcome {
        ops::AddRefOutcome::Described { data, yes_argv } => {
            assert_eq!(data.source, "github.com/o/r");
            assert!(yes_argv.contains(&"--yes".to_owned()), "{yes_argv:?}");
        }
        ops::AddRefOutcome::Applied { .. } => {
            panic!("an interactive add of a git source always describes first")
        }
    }
}

#[test]
fn a_deleted_repo_is_reported_once_and_then_left_alone() {
    // ACCEPTANCE 7a. A repo the forge says is gone is a fact about the ROW, not about the
    // network: saying it every session would train a person to stop reading. Said once, then the
    // lane stops asking until the row that names it changes.
    let rig = Rig::new("repo-deleted");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    let alpha = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    assert!(alpha.exists());

    // The repo is deleted upstream.
    git.fail_with(FetchFault::Gone);
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let dialed = git.probes() + git.fetches();
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("REMOTE_FETCH")),
        "the refusal is said: {:?}",
        out.warnings
    );
    assert!(alpha.exists(), "the bytes already here keep working");

    // Round two: not asked, not said.
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        dialed,
        "a settled verdict stops the dialing"
    );
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("REMOTE_FETCH")),
        "and is not repeated: {:?}",
        out.warnings
    );
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "the row still converges in place: {:?}",
        out.data.skills
    );

    // EDITING the row re-opens the question — the verdict was about what the row said. (The pin
    // names a commit nothing here holds, so the row is genuinely unsettled and does want an
    // answer; a pin already satisfied would rightly never dial at all.)
    rig.write_global("[skills]\nalpha = \"github:o/r#ffffffffffff9\"\n");
    forge_interval_elapsed(&rig);
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert!(
        git.probes() + git.fetches() > dialed,
        "a changed row asks again"
    );
}

#[test]
fn a_renamed_repo_keeps_working_and_says_where_it_went() {
    // ACCEPTANCE 7b. The forge redirects a renamed repo and the check follows it, so the row goes
    // on resolving. A rename is never reported as a missing repo — and never as a failure at all:
    // the person is simply told the canonical spelling, on the channel for things that WORKED.
    let rig = Rig::new("repo-renamed");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    git.rename_to("newowner", "newrepo");
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the renamed repo keeps delivering: {:?}",
        out.warnings
    );
    let line = crate::message::legacy_lines(&out.disclosures)
        .into_iter()
        .find(|d| d.starts_with("GIT_RENAMED"))
        .unwrap_or_else(|| panic!("the rename note: {:?}", out.disclosures));
    assert!(line.contains("github.com/newowner/newrepo"), "{line}");
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.contains("not found")),
        "a rename is never reported as a missing repo: {:?}",
        out.warnings
    );
}

#[test]
fn five_rows_behind_one_quiet_forge_produce_one_line_and_only_when_stale() {
    // ACCEPTANCE 8. The silent sweep's only channel is text injected into an agent's context
    // window, so its budget is a person's attention: five rows behind one dead forge is ONE thing
    // that happened. And it is said only once the silence has run long enough to mean something —
    // a blip must never interrupt a session.
    let rig = Rig::new("repo-one-line");
    rig.write_global(
        "[skills]\na = \"github:o/a\"\nb = \"github:o/b\"\n\
         c = \"github:o/c\"\nd = \"github:o/d\"\ne = \"github:o/e\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let archive = build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/a/SKILL.md", b"# a v1\n"),
            ("skills/b/SKILL.md", b"# b v1\n"),
            ("skills/c/SKILL.md", b"# c v1\n"),
            ("skills/d/SKILL.md", b"# d v1\n"),
            ("skills/e/SKILL.md", b"# e v1\n"),
        ],
    );
    // All five answer once, so each has a last-answered time to be stale FROM.
    for name in ["a", "b", "c", "d", "e"] {
        let git = FakeGit::new(archive.clone());
        forge_interval_elapsed(&rig);
        update_now(&ctx, &plane, &dir, &git);
        let _ = name;
    }
    let git = FakeGit::new(archive);
    git.fail_with(FetchFault::Unreachable);
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(out.stale_forge.len(), 1, "one host: {:?}", out.stale_forge);
    assert_eq!(out.stale_forge[0].host, "github.com");
    assert_eq!(out.stale_forge[0].sources, 5);

    // A fresh miss says NOTHING — the copies still work, and a transient blip is not news.
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert!(lines.is_empty(), "not stale yet: {lines:?}");

    // Long after the last answer, ONE line — naming the host, the count, and the consequence.
    let stale_now = rig_now(&rig) + crate::forge_check::STALE_AFTER_MS + DAY_MS;
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("github.com"), "{:?}", lines[0]);
    // SOURCES, not skills: one repository row can hold any number of skills, so counting the
    // repositories and calling them skills would be a number about the wrong thing.
    assert!(lines[0].contains("5 sources"), "{:?}", lines[0]);
    assert!(
        lines[0].contains("They still work."),
        "the consequence, so `is unreachable` does not read as breakage: {:?}",
        lines[0]
    );
}

#[test]
fn a_repo_set_member_reads_as_current_in_list_not_detached() {
    // ACCEPTANCE 9 — the two commands must not contradict each other. `update` keeps a
    // GitHub-sourced skill current; `list` used to render the very same copy as
    // "[detached] … removed from the skill list", because the set row's expansion was never
    // itemized and the ghost walk therefore read a live, managed copy as an abandoned leftover.
    // Asserted against BOTH commands in one test, because the bug WAS the disagreement.
    let rig = Rig::new("repo-list-current");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\nbeta = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v1\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
        ],
    ));

    // What `update` says: both members are managed and current.
    let out = update_now(&ctx, &plane, &dir, &git);
    for name in ["alpha", "beta"] {
        assert!(
            out.data.skills.iter().any(|s| s.skill == name),
            "update manages {name}: {:?}",
            out.data.skills
        );
        assert!(
            rig.home
                .0
                .join(format!(".claude/skills/{name}/SKILL.md"))
                .exists(),
            "{name}'s bytes are placed"
        );
    }

    // What `list` says: the SAME thing. A member is a live row of the set that delivers it —
    // never a ghost, never detached, never "removed from the skill list".
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let rows: Vec<&topos_types::results::SkillEntry> = listed
        .data
        .scopes
        .iter()
        .flat_map(|sc| sc.rows.iter())
        .collect();
    for name in ["alpha", "beta"] {
        let row = rows
            .iter()
            .find(|r| r.skill == name)
            .unwrap_or_else(|| panic!("list itemizes {name}: {rows:?}"));
        assert_eq!(
            row.status,
            Some(topos_types::results::SkillStatus::Current),
            "{name} is managed by update, so list must call it current: {row:?}"
        );
        // The source column names WHERE THE BYTES COME FROM — the repository, exactly as for
        // every other kind of line. That is the whole point: the member is an ordinary delivered
        // row now, and it says its own origin instead of leaning on a block below the list.
        assert_eq!(row.source.as_deref(), Some("github.com/o/r"), "{row:?}");
    }

    // And the rendered text carries no contradiction either.
    let text = crate::render::list_tty(&listed);
    assert!(!text.contains("[detached]"), "{text}");
    assert!(!text.contains("removed from the skill list"), "{text}");
    // ONE list: the repository is named on the rows it delivers, never as a trailing block of
    // its own — the listing has exactly one place a bundle can be read from.
    assert!(text.contains("alpha  alpha@"), "{text}");
    assert!(text.contains("from github.com/o/r"), "{text}");
    assert!(!text.contains("external sources:"), "{text}");
}

#[test]
fn two_rows_over_one_gone_repo_ask_once_and_say_it_once() {
    // A set line and a member line of the SAME repository are two rows about one subject. A
    // verdict reached for the first must hold the second — otherwise a deleted repo named twice
    // costs two requests and says the same sentence twice in one breath.
    let rig = Rig::new("repo-two-rows-gone");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\nbeta = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Gone);

    let out = update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        1,
        "one repository, one question"
    );
    let said: Vec<String> = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .filter(|w| w.starts_with("REMOTE_FETCH"))
        .collect();
    assert_eq!(said.len(), 1, "said once, not once per row: {said:?}");
}

#[test]
fn a_pinned_rows_fetch_is_recorded_as_a_check_like_any_other() {
    // A pinned row never probes: it is settled and silent, or it fetches. The fetch is still a
    // check, and "recorded always" has to mean always — otherwise `status` would show nothing
    // about a source topos had just contacted.
    let rig = Rig::new("repo-pinned-record");
    rig.write_global("[skills]\nalpha = \"github:o/r#aaaaaaaaaaaa1\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(git.probes(), 0, "a pinned row is never probed");
    assert!(git.fetches() > 0, "it fetched the pinned bytes");

    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let rec = doc
        .sources
        .get(&crate::forge_check::question(
            "github.com/o/r",
            "aaaaaaaaaaaa1",
        ))
        .unwrap_or_else(|| panic!("the pinned fetch is recorded: {:?}", doc.sources));
    assert_eq!(rec.checked_at_ms, rig_now(&rig));
    assert_eq!(rec.answered_at_ms, Some(rig_now(&rig)));
    assert!(rec.failure.is_none(), "{rec:?}");
}

#[test]
fn a_machine_with_no_external_rows_schedules_nothing() {
    // A clock is about a subject. Nothing was checked — no external rows at all — so there is no
    // turn to have taken and no next turn to schedule; the document is not even born.
    let rig = Rig::new("repo-no-rows");
    rig.write_global("[skills]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz("o-r-aaaaaaaaaaaa1", &[]));
    update_now(&ctx, &plane, &dir, &git);
    assert!(
        !rig.layout().forge_check_path().exists(),
        "no source, no clock"
    );
}

#[test]
fn a_failed_check_on_an_uninstalled_member_says_one_thing_not_two() {
    // The fault is the whole story. Adding "this machine has not fetched it yet" beside it would
    // be a second, weaker sentence about the same event — and would read as a separate problem.
    let rig = Rig::new("repo-one-sentence");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Unreachable);
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("REMOTE_FETCH")),
        "the fault is named: {:?}",
        out.warnings
    );
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("NOT_INSTALLED")),
        "and only once: {:?}",
        out.warnings
    );
}

#[test]
fn editing_a_sibling_rows_ref_reopens_it_past_another_rows_settled_verdict() {
    // Editing the ref is the documented way to reopen a settled verdict. A SECOND row naming the
    // same repository at its old ref must not be able to take that away: the one-turn rule is
    // about the QUESTION asked, and two rows at different refs ask different questions.
    let rig = Rig::new("repo-sibling-reopen");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\nbeta = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    // Both rows settle on a gone verdict at the floating ref.
    git.fail_with(FetchFault::Gone);
    update_now(&ctx, &plane, &dir, &git);
    let settled = git.probes() + git.fetches();
    assert!(settled > 0);

    // Now ONE of them is edited to a pin. The other still spells the old floating ref and is
    // reconciled first — it must not silence the edited one.
    rig.write_global("[skills]\nalpha = \"github:o/r\"\nbeta = \"github:o/r#ffffffffffff9\"\n");
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert!(
        git.probes() + git.fetches() > settled,
        "the edited row asks again despite its sibling's standing verdict"
    );
}

#[test]
fn a_round_that_only_carried_verdicts_schedules_nothing() {
    // A carried verdict is not a dial. Writing it back into the round's outcomes would make a run
    // that asked nobody anything look like one that did — and push the clock out for sources that
    // were never contacted.
    let rig = Rig::new("repo-carried-only");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Gone);
    update_now(&ctx, &plane, &dir, &git);
    let due_after_the_real_round = forge_next_due(&rig, "github.com/o/r", "", "person");

    // A later round finds only the standing verdict: it dials nothing, and moves nothing.
    forge_interval_elapsed(&rig);
    let parked = forge_next_due(&rig, "github.com/o/r", "", "person");
    let dialed = git.probes() + git.fetches();
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(git.probes() + git.fetches(), dialed, "nothing was asked");
    assert_eq!(
        forge_next_due(&rig, "github.com/o/r", "", "person"),
        parked,
        "and nothing was scheduled: {due_after_the_real_round}"
    );
}

#[test]
fn a_reachable_but_failing_forge_is_not_called_unreachable() {
    // "Unreachable" names a network. A forge that answers 403 or 500 was reached and answered
    // badly, which is a different thing to go look at — and the line must not say the wrong one.
    let rig = Rig::new("repo-answered-badly");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    git.fail_with(FetchFault::unavailable());
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(out.stale_forge.len(), 1, "{:?}", out.stale_forge);
    assert!(out.stale_forge[0].reached, "the forge answered");
    let stale_now = rig_now(&rig) + crate::forge_check::STALE_AFTER_MS + DAY_MS;
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        !lines[0].contains("unreachable"),
        "a host that answered is not unreachable: {:?}",
        lines[0]
    );
    assert!(lines[0].contains("github.com"), "{:?}", lines[0]);
    assert!(lines[0].contains("They still work."), "{:?}", lines[0]);
}

#[test]
fn an_unreadable_archive_is_a_failed_check_not_a_passed_one() {
    // The request succeeded; the answer did not. Recording a 200 as a good check would let
    // `status` report success for a round that could not install anything — and would file the
    // PREVIOUS commit as the one just seen.
    let rig = Rig::new("repo-bad-archive");
    rig.write_global("[skills]\nalpha = \"github:o/r#aaaaaaaaaaaa1\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(b"this is not a gzip archive".to_vec());
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(!out.warnings.is_empty(), "the round failed");

    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let rec = doc
        .sources
        .get(&crate::forge_check::question(
            "github.com/o/r",
            "aaaaaaaaaaaa1",
        ))
        .unwrap_or_else(|| panic!("the attempt is recorded: {:?}", doc.sources));
    assert!(
        rec.failure.is_some(),
        "an unreadable archive is a FAILED check: {rec:?}"
    );
    assert_eq!(
        rec.answered_at_ms, None,
        "it never really answered: {rec:?}"
    );
}

#[test]
fn a_signed_out_workspaces_leftover_resolves_once_then_retires() {
    // The one-time orphan resolution: a store record no row claims and nothing delivers gets ONE
    // closing statement — workspace-named, files-led — and then leaves every surface. Nothing on
    // disk is deleted: the placed copy stays, and belongs to the person now.
    let rig = Rig::new("orphan-signed-out");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# alpha v1\n");
    let plane = FakePlane::new(log).with_version("s_alpha", &v1);
    plane.serves(vec![delivered("s_alpha", "alpha", &v1)]);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("alpha");
    assert!(placed.join("SKILL.md").exists(), "alpha's bytes are placed");

    // Sign out (the session ends; a deliberate state) and drop the feed line: nothing claims the
    // record and nothing can deliver it. BEFORE the resolution, the record still owns its name.
    sessions::set_session_status(&rig.fs, &rig.layout(), HOST, WS, SESSION_ENDED).unwrap();
    rig.write_global("[skills]\n");
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: None,
    };
    assert!(
        matches!(
            ops::plan_bare_add(
                &ctx,
                &connect(&plane, &dir),
                &roots,
                "alpha",
                bare_opts(true)
            ),
            Ok(ops::BareAddPlan::Reference { .. })
        ),
        "before the resolution the record still resolves the bare name — to the workspace \
         spelling it was delivered under"
    );

    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha")
        .unwrap_or_else(|| {
            panic!(
                "the one-time resolution rides the receipt: {:?}",
                out.data.skills
            )
        });
    assert_eq!(row.action, PullAction::Released);
    // Byte-exact: the workspace-named, single-folder path form of the released fact.
    assert_eq!(
        row.note.as_deref(),
        Some(
            format!(
                "{WS_NAME} stopped sharing this — topos will not update it any more\nthe files \
                 stay where they are, and are yours to keep or delete: {}",
                rig.pretty(&placed)
            )
            .as_str()
        )
    );
    assert_eq!(
        row.display.as_deref(),
        Some(format!("{HOST}/{WS_NAME}/alpha").as_str()),
        "no live host, so the full spelling qualifies the name"
    );
    // The marker is durable; the files and the store bytes stay untouched.
    let sid = crate::id::SkillId::parse("s_alpha").unwrap();
    assert!(rig.fs.exists(&rig.layout().published(&sid).retired));
    assert!(
        placed.join("SKILL.md").exists(),
        "the resolution deletes nothing"
    );
    assert!(
        rig.fs.exists(&rig.layout().skill_dir(&sid)),
        "the record's bytes stay on disk forever"
    );

    // RETIREMENT DURABILITY: the next sweep says nothing; `list` shows nothing; the record no
    // longer resolves a bare name (the kept folder is adoptable again instead).
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "a retired record is never mentioned again: {:?}",
        out.data.skills
    );
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    assert!(
        !listed
            .data
            .scopes
            .iter()
            .flat_map(|sc| sc.rows.iter())
            .any(|r| r.skill == "alpha"),
        "no row claims it, so no line originates from it"
    );
    // The retired record no longer answers for the name at all. The folder it left behind is the
    // person's now, sitting in an agent's own skills dir — so the honest answer is the ordinary
    // untracked-folder adopt, never the record's "already tracked" or its workspace reference.
    let plan = ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        bare_opts(true),
    );
    match plan {
        Ok(ops::BareAddPlan::Adopt { path, name, .. }) => {
            assert_eq!(
                path, placed,
                "the released folder, adopted as anyone's would be"
            );
            assert_eq!(name, "alpha");
        }
        other => panic!("a retired record must not resolve the bare name: {other:?}"),
    }
}

#[test]
fn an_unreached_workspace_freezes_its_records_and_a_withdrawal_resolves_later() {
    // The freeze gates, end to end: a live session with NO fresh snapshot this run freezes its
    // records (an outage never reads as an orphan); the run a withdrawal actually lands in speaks
    // through the withdraw row alone; and only the NEXT full sweep resolves the leftover — once.
    let rig = Rig::new("orphan-freeze");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# alpha v1\n");
    let plane = FakePlane::new(log).with_version("s_alpha", &v1);
    plane.serves(vec![delivered("s_alpha", "alpha", &v1)]);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let sid = crate::id::SkillId::parse("s_alpha").unwrap();

    // The workspace is unreachable: everything freezes — no withdrawal, no resolution.
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data.skills.iter().any(|s| {
            s.skill == "alpha" && matches!(s.action, PullAction::Withdrawn | PullAction::Released)
        }),
        "an outage must never read as an orphan: {:?}",
        out.data.skills
    );
    assert!(!rig.fs.exists(&rig.layout().published(&sid).retired));

    // The workspace answers fresh and no longer serves alpha: the withdraw arm speaks this run,
    // and the record is fresh news — not yet a standing orphan.
    plane.serves(Vec::new());
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    assert!(
        !rig.fs.exists(&rig.layout().published(&sid).retired),
        "a record that earned a receipt row this run is not retired under it"
    );

    // The NEXT full sweep resolves the leftover once: no copies remain (the withdrawal cleaned
    // them), the record retires, and a further sweep says nothing.
    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha" && s.action == PullAction::Released)
        .unwrap_or_else(|| panic!("the one-time resolution: {:?}", out.data.skills));
    assert_eq!(
        row.note.as_deref(),
        Some("no copies remain — nothing left to manage")
    );
    assert!(rig.fs.exists(&rig.layout().published(&sid).retired));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn a_targeted_update_never_resolves_orphans() {
    // A targeted update narrows within the driven scope: it cannot know the whole demand set, so
    // the orphan pass never runs under it.
    let rig = Rig::new("orphan-targeted");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# alpha v1\n");
    let v2 = one_file(b"# beta v1\n");
    let plane = FakePlane::new(log)
        .with_version("s_alpha", &v1)
        .with_version("s_beta", &v2);
    plane.serves(vec![
        delivered("s_alpha", "alpha", &v1),
        delivered("s_beta", "beta", &v2),
    ]);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // Upstream withdraws alpha; a full sweep lands the withdrawal (fresh news, not yet an
    // orphan). From here alpha's record is orphan-eligible.
    plane.serves(vec![delivered("s_beta", "beta", &v2)]);
    sweep(&ctx, &plane, &dir);

    // The targeted form: converge beta alone — the eligible record is left exactly as it is.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["beta".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "{:?}",
        out.data.skills
    );
    let sid = crate::id::SkillId::parse("s_alpha").unwrap();
    assert!(
        !rig.fs.exists(&rig.layout().published(&sid).retired),
        "a targeted update never resolves orphans"
    );

    // The full sweep still owns the resolution.
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::Released),
        "{:?}",
        out.data.skills
    );
    assert!(rig.fs.exists(&rig.layout().published(&sid).retired));
}

#[test]
fn the_released_fact_shapes_are_byte_exact() {
    // Every final-copy shape, pinned at the composer (the renderer prints the fact verbatim, one
    // line per line). The two facts a person needs, in order: who stopped sharing, and where that
    // leaves the files — the second line naming the ONE folder, or listing several beneath itself.
    assert_eq!(
        ops::orphan_fact(
            Some("acme"),
            &["~/.claude/skills/legacy-deploy".to_owned()],
            &["~/.claude/skills/legacy-deploy".to_owned()],
        ),
        "acme stopped sharing this — topos will not update it any more\nthe files stay where they \
         are, and are yours to keep or delete: ~/.claude/skills/legacy-deploy"
    );
    let folders = |n: usize| -> Vec<String> { (0..n).map(|i| format!("/tmp/f{i}")).collect() };
    assert_eq!(
        ops::orphan_fact(Some("acme"), &folders(2), &folders(2)),
        "acme stopped sharing this — topos will not update it any more\nthe files stay where they \
         are, and are yours to keep or delete:\n  /tmp/f0\n  /tmp/f1"
    );
    // No workspace name on record: the row states only what it knows — where the files stand —
    // rather than a sentence with a hole where the name belongs.
    assert_eq!(
        ops::orphan_fact(
            None,
            &["~/.claude/skills/runbook".to_owned()],
            &["~/.claude/skills/runbook".to_owned()],
        ),
        "the files stay where they are, and are yours to keep or delete: ~/.claude/skills/runbook"
    );
    // Nothing left on disk: the fact states the DISCOVERY (the copies were already gone), never an
    // act this run performed. One recorded copy names it; several name the last of them.
    assert_eq!(
        ops::orphan_fact(None, &folders(0), &["/private/tmp/deploy".to_owned()]),
        "its only copy, /private/tmp/deploy, no longer exists — nothing left to manage"
    );
    assert_eq!(
        ops::orphan_fact(
            None,
            &folders(0),
            &[
                "~/.claude/skills/deploy".to_owned(),
                "~/.cursor/skills/deploy".to_owned(),
                "/private/tmp/deploy".to_owned(),
            ],
        ),
        "its 3 copies, last at /private/tmp/deploy, no longer exist — nothing left to manage"
    );
    // Nothing on disk AND nothing recorded (the withdrawal already cleaned the map): the same
    // consequence, minimally stated.
    assert_eq!(
        ops::orphan_fact(None, &folders(0), &folders(0)),
        "no copies remain — nothing left to manage"
    );
}

#[test]
fn re_adding_a_repo_that_came_back_re_enables_its_automatic_update() {
    // A verdict is a record of what a forge SAID. A forge that has just handed over the bytes has
    // said something newer — so a repository deleted, written off, and later restored must not stay
    // permanently un-updated because a refusal is still on file.
    let rig = Rig::new("repo-came-back");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // Gone. The verdict settles and the lane stops asking.
    git.fail_with(FetchFault::Gone);
    update_now(&ctx, &plane, &dir, &git);
    forge_interval_elapsed(&rig);
    let settled = git.probes() + git.fetches();
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        settled,
        "a settled verdict stops the dialing"
    );

    // The repository comes back, and the person re-adds it. The add succeeds, which is newer
    // evidence than the refusal.
    *git.fault.lock().unwrap() = None;
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r/alpha");
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());

    // THE POINT: automatic updates work again. Without the forgetting the lane would go on
    // refusing to dial a source the person just proved is alive.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    let before = git.probes() + git.fetches();
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert!(
        git.probes() + git.fetches() > before,
        "the restored source is checked again"
    );
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "and the silent sweep carries it forward again"
    );
}

#[test]
fn a_deletion_the_silent_sweep_finds_is_actually_said() {
    // "Report once" has to mean once OUT LOUD. The silent sweep discards the warnings channel, so
    // a deletion discovered by the sweep — the ordinary way it is discovered — would be recorded
    // as reported, never shown, and then suppressed forever.
    let rig = Rig::new("repo-quiet-deletion");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    git.fail_with(FetchFault::Gone);
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert_eq!(lines.len(), 1, "the sweep says it: {lines:?}");
    assert!(lines[0].contains("github.com/o/r"), "{:?}", lines[0]);
    assert!(lines[0].contains("still work"), "{:?}", lines[0]);

    // And exactly once: the next sweep neither dials nor repeats it.
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert!(lines.is_empty(), "and only once: {lines:?}");
}

#[test]
fn scoped_forge_health_answers_about_the_scopes_own_pin() {
    // Two scopes can track one repository at different pins. Those are different questions with
    // different answers, and a scoped read must report the one IT asks. A project-only `status`
    // showing the machine-only pin's failure would be a report about somebody else's row.
    let rig = Rig::new("repo-scoped-health");
    // The MACHINE pins a commit that is gone; the PROJECT pins one that is fine. The machine's
    // question sorts LAST, so a source-level filter would let it win the "newest" fold.
    rig.write_global("[skills]\nalpha = \"github:o/r#ffffffffffff9\"\n");
    let proj = project(
        "proj-scoped-health",
        "[skills]\nalpha = \"github:o/r#aaaaaaaaaaaa1\"\n",
    );
    let pctx = rig.ctx_at(Some(&proj.0));
    let now = rig_now(&rig);
    let fine = crate::forge_check::SourceCheck {
        next_check_at: Default::default(),
        checked_at_ms: now,
        answered_at_ms: Some(now),
        commit: Some("aaaaaaaaaaaa1".to_owned()),
        failure: None,
        settled_at: Default::default(),
    };
    let gone = crate::forge_check::SourceCheck {
        next_check_at: Default::default(),
        checked_at_ms: now,
        answered_at_ms: None,
        commit: None,
        failure: Some(crate::forge_check::CheckFailure {
            reason: "repo not found".to_owned(),
            gone: true,
            reached: true,
            git_ref: "ffffffffffff9".to_owned(),
            reported: true,
        }),
        settled_at: Default::default(),
    };
    crate::forge_check::record_round(
        &rig.fs,
        &rig.layout(),
        None,
        &[
            (
                crate::forge_check::question("github.com/o/r", "aaaaaaaaaaaa1"),
                fine,
            ),
            (
                crate::forge_check::question("github.com/o/r", "ffffffffffff9"),
                gone,
            ),
        ],
    )
    .unwrap();

    // The project's own question answered fine, so its `status` says nothing is wrong.
    let here = ops::status_snapshot(&pctx, ops::ScopeView::Here).unwrap();
    assert_eq!(here.forge.len(), 1, "{:?}", here.forge);
    assert_eq!(here.forge[0].source, "github.com/o/r");
    assert!(
        here.forge[0].error.is_none() && !here.forge[0].gone,
        "a project-only read must not carry the machine-only pin's refusal: {:?}",
        here.forge[0]
    );
    assert_eq!(here.forge[0].commit.as_deref(), Some("aaaaaaaaaaaa1"));

    // The MACHINE scope asks the other question, and gets the other answer.
    let machine = ops::status_snapshot(&pctx, ops::ScopeView::Machine).unwrap();
    assert_eq!(machine.forge.len(), 1, "{:?}", machine.forge);
    assert!(
        machine.forge[0].gone,
        "the machine's own pin is the one that is gone: {:?}",
        machine.forge[0]
    );
}

#[test]
fn the_deep_dive_names_only_its_own_external_source() {
    // The one-skill dive is the only `list` view that PRINTS this block, and it prints one line at
    // most. Every line the dive shows is a fact about the skill on screen, so an unrelated
    // repository's last check would read as one more of them — "what is this machine tracking?"
    // answering a question nobody asked here. It shows the repository that DELIVERS this skill, or
    // nothing. The resolution still computes the field for every view (the wire contract must not
    // depend on which view fell through); the LISTING simply names each row's origin in its `from`
    // column instead of enumerating sources under the list.
    let rig = Rig::new("repo-focused-views");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    let dive = |name: &str| {
        crate::ops::list_with(
            &ctx,
            &ops::ListRequest {
                name: Some(name.to_owned()),
                ..ops::ListRequest::default()
            },
            None,
            None,
            crate::ops::RowPage::unlimited(),
        )
        .unwrap()
        .data
        .forge
    };

    // The repo delivers `alpha`, so `alpha`'s own answer names it.
    let mine = dive("alpha");
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert_eq!(mine[0].source, "github.com/o/r");

    // Nothing external delivers the built-in — and the machine's unrelated GitHub row is not news
    // about it. THE reported bug: this block used to ride every dive on the machine.
    assert!(dive("topos").is_empty(), "{:?}", dive("topos"));

    // The dive PRINTS the one line it kept — the block is not merely computed there.
    let printed = crate::render::list_tty(
        &crate::ops::list_with(
            &ctx,
            &ops::ListRequest {
                name: Some("alpha".to_owned()),
                ..ops::ListRequest::default()
            },
            None,
            None,
            crate::ops::RowPage::unlimited(),
        )
        .unwrap(),
    );
    assert!(printed.contains("external sources:"), "{printed}");
    assert!(printed.contains("github.com/o/r"), "{printed}");

    // The LISTING still carries the field on the wire — a `--json` reader asks about source health
    // from any view — while its TTY says the same repository per row, in the `from` column.
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    assert!(
        listed
            .data
            .forge
            .iter()
            .any(|f| f.source == "github.com/o/r"),
        "{:?}",
        listed.data.forge
    );
    let text = crate::render::list_tty(&listed);
    assert!(!text.contains("external sources:"), "{text}");
    assert!(text.contains("from github.com/o/r"), "{text}");
}

#[test]
fn a_second_checkout_is_never_starved_by_the_first_ones_sweeps() {
    // THE failure this increment exists to remove, reintroduced by a machine-wide clock. A sweep
    // only ever visits the checkout it starts in plus the machine manifest. If one global due time
    // governed the lane, the first checkout active after each due moment would spend the turn, and
    // a second project — never visited at a due moment — would never be checked at all.
    let rig = Rig::new("repo-two-checkouts");
    let a = project("proj-a", "[skills]\nalpha = \"github:o/a\"\n");
    let b = project("proj-b", "[skills]\nalpha = \"github:o/b\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let actx = rig.ctx_at(Some(&a.0));
    let bctx = rig.ctx_at(Some(&b.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // Both check once, so both have a clock.
    quiet_sweep(&actx, &plane, &dir, &git);
    quiet_sweep(&bctx, &plane, &dir, &git);
    assert!(b.0.join(".claude/skills/alpha/SKILL.md").exists());

    // Now A is the only checkout anyone works in. Its sweeps run at every due moment.
    for _ in 0..3 {
        forge_interval_elapsed(&rig);
        quiet_sweep(&actx, &plane, &dir, &git);
    }
    // B has not been visited since, so its clock must still be due — A cannot have spent B's turn.
    assert!(
        crate::forge_check::due(
            &crate::forge_check::read(&rig.fs, &rig.layout()),
            &crate::forge_check::question("github.com/o/b", ""),
            &b.0.display().to_string(),
            rig_now(&rig)
        ),
        "the second checkout's row is still owed a check"
    );

    // And the moment somebody opens B, upstream really does reach it — through the lock's own
    // door: the quiet sweep HOLDS the locked commit (a project is deterministic), and the
    // explicit `topos update` is what takes the new one.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    quiet_sweep(&bctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(b.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v1\n",
        "a quiet sweep never moves a locked checkout"
    );
    ops::manifest_update(
        &bctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts {
            lock: ops::LockMode::Update,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(b.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the second checkout is not starved: update reaches it"
    );
}

#[test]
fn a_fresh_checkout_installs_now_rather_than_waiting_out_another_scopes_interval() {
    // A turn belongs to a SCOPE. Two checkouts share a question but not a state, so a clone must
    // fetch on its first sweep instead of sitting empty until an interval another checkout started
    // runs out — while a scope that HAS taken its turn still waits, whatever the answer was.
    let rig = Rig::new("repo-fresh-clone");
    let a = project("clone-a", "[skills]\nalpha = \"github:o/r\"\n");
    let b = project("clone-b", "[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    quiet_sweep(&rig.ctx_at(Some(&a.0)), &plane, &dir, &git);
    assert!(a.0.join(".claude/skills/alpha/SKILL.md").exists());

    // B has never asked. Its sweep — well inside the interval A started — must still install.
    quiet_sweep(&rig.ctx_at(Some(&b.0)), &plane, &dir, &git);
    assert!(
        b.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "a fresh checkout is waiting on bytes, not on somebody else's cadence"
    );

    // Both have now had their turn: neither dials again inside the interval.
    let dialed = git.probes() + git.fetches();
    quiet_sweep(&rig.ctx_at(Some(&a.0)), &plane, &dir, &git);
    quiet_sweep(&rig.ctx_at(Some(&b.0)), &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        dialed,
        "a scope that has taken its turn waits"
    );
}

/// A repo whose members refuse differently: `alpha` refreshes cleanly, `beta` carries local edits
/// and its refresh is refused. Two members are the point — with one, the sole tracked import IS the
/// refused one, and a "first recorded commit matches" test can never wrongly pass.
fn two_member_repo(top: &str, alpha: &[u8], beta: &[u8]) -> Vec<u8> {
    build_repo_targz(
        top,
        &[
            ("skills/alpha/SKILL.md", alpha),
            ("skills/beta/SKILL.md", beta),
        ],
    )
}

#[test]
fn a_sibling_moving_on_never_settles_a_member_whose_refresh_was_refused() {
    // `forge_imports` sorts by name, so a row's "recorded commit" read from the FIRST tracked
    // import is alphabetically first — `alpha`. Once alpha moves to the new head, a row-level
    // comparison says the whole row is current while `beta`, whose refresh was refused, sits a
    // commit behind reporting up to date. On a quiet repository that lasts until the next push.
    let rig = Rig::new("repo-sibling-settles");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\nbeta = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(two_member_repo(
        "o-r-aaaaaaaaaaaa1",
        b"# alpha v1\n",
        b"# beta v1\n",
    ));
    update_now(&ctx, &plane, &dir, &git);
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(beta.exists());

    // beta gets local edits; upstream moves. alpha refreshes, beta's refresh is refused.
    std::fs::write(&beta, "# beta v1 MY EDITS\n").unwrap();
    git.serve(two_member_repo(
        "o-r-bbbbbbbbbbbb2",
        b"# alpha v2\n",
        b"# beta v2\n",
    ));
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the sibling moved on"
    );

    // The person resolves the edits. The very next update must carry beta forward — nothing
    // upstream has changed, so a row that called itself settled would never revisit it.
    std::fs::write(&beta, "# beta v1\n").unwrap();
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(&beta).unwrap(),
        "# beta v2\n",
        "a member left behind by a refusal is still owed the update"
    );
}

#[test]
fn a_pinned_row_keeps_trying_a_member_whose_refresh_was_refused() {
    // Strictly worse than the floating case: a pin never moves, so there is no later push to heal
    // it. A presence-only settled test here means the member never reaches the pin — ever — while
    // the archive is re-downloaded every interval to skip it again.
    let rig = Rig::new("repo-pinned-refusal");
    rig.write_global(
        "[skills]\nalpha = \"github:o/r#aaaaaaaaaaaa1\"\nbeta = \"github:o/r#aaaaaaaaaaaa1\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(two_member_repo(
        "o-r-aaaaaaaaaaaa1",
        b"# alpha v1\n",
        b"# beta v1\n",
    ));
    update_now(&ctx, &plane, &dir, &git);
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(beta.exists());

    // Repin to a new commit, with beta carrying edits so its refresh is refused.
    std::fs::write(&beta, "# beta v1 MY EDITS\n").unwrap();
    git.serve(two_member_repo(
        "o-r-bbbbbbbbbbbb2",
        b"# alpha v2\n",
        b"# beta v2\n",
    ));
    rig.write_global(
        "[skills]\nalpha = \"github:o/r#bbbbbbbbbbbb2\"\nbeta = \"github:o/r#bbbbbbbbbbbb2\"\n",
    );
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the sibling reached the pin"
    );

    // Edits resolved. The pin has not moved and never will, so this update is the only chance
    // beta will ever get.
    std::fs::write(&beta, "# beta v1\n").unwrap();
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(&beta).unwrap(),
        "# beta v2\n",
        "a pin with no healing event must keep trying the member it could not place"
    );
}

#[test]
fn a_wiped_agent_directory_is_re_placed_not_declared_current() {
    // A record is not bytes. The sidecar survives a wiped agent directory — an ordinary harness
    // reinstall, or somebody clearing `~/.claude` — and every import goes on claiming its commit
    // for a copy that is no longer anywhere. A settled test resting on records alone would call
    // the row current forever while the skills are simply gone.
    //
    // FAILING against a REPO-SKILL row: `reconcile_repo_skill`'s `settled` predicate compares only
    // the recorded commit, with no bytes-on-disk proof — the repo-SET arm's `tracked_at` has one
    // (`m.placements.iter().any(|pl| fs.exists(pl))`). v2 makes the skill row the only spellable
    // forge row, so the weaker predicate is now the only one anybody reaches.
    let rig = Rig::new("repo-wiped-agent-dir");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    let placed = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    assert!(placed.exists());

    // The agent directory goes; the sidecar stays exactly as it was.
    std::fs::remove_dir_all(rig.home.0.join(".claude")).unwrap();
    assert!(
        crate::ops::forge_imports(&ctx)
            .iter()
            .any(|i| i.lock.name == "alpha"),
        "the record still claims the member"
    );

    forge_interval_elapsed(&rig);
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        placed.exists(),
        "bytes that are gone are not current: {:?}",
        out.warnings
    );
}

#[test]
fn a_refused_refresh_is_not_a_convergence() {
    // A settlement mark says "there is nothing to do here at this head", and the probe trusts it.
    // So it must only ever be written when the work really happened: a refresh the engine REFUSED
    // — a member carrying local edits — leaves that member at the OLD commit, and recording
    // settlement over it would suppress the retry even after the edits are resolved.
    let rig = Rig::new("repo-refused-refresh");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    let placed = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    assert!(placed.exists());

    // Local edits, then upstream moves. The refresh keeps the edits rather than clobbering them,
    // so the member stays at the old commit — this round converged nothing.
    std::fs::write(&placed, "# alpha v1 WITH MY EDITS\n").unwrap();
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);

    // Whatever the engine chose to do with the edits, it must not have recorded the new head as
    // settled unless the member really sits on it.
    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let rec = doc
        .sources
        .get(&crate::forge_check::question("github.com/o/r", ""))
        .expect("recorded");
    let landed_on_new_head = crate::ops::forge_imports(&ctx)
        .iter()
        .any(|i| i.lock.name == "alpha" && i.origin.commit.as_deref() == Some("bbbbbbbbbbbb2"));
    if !landed_on_new_head {
        // The EARLIER round really did converge at the old head, and that mark rightly stands.
        // What must not exist is a mark claiming the NEW head, which nothing reached.
        assert!(
            rec.settled_at.values().all(|m| m.head != "bbbbbbbbbbbb2"),
            "a refusal is not a convergence: {:?}",
            rec.settled_at
        );
    }

    // And the row keeps being fetched until it really is converged.
    let fetches = git.fetches();
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    if !landed_on_new_head {
        assert!(
            git.fetches() > fetches,
            "unfinished work is retried, not written off"
        );
    }
}

#[test]
fn every_check_is_recorded_and_visible_to_status_and_list() {
    // ACCEPTANCE 4's first two tiers: recorded ALWAYS, and shown on demand — the answer to "is
    // this even working?", from local state alone.
    let rig = Rig::new("repo-recorded");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    let status = ops::status_snapshot(&ctx, ops::ScopeView::All).unwrap();
    let row = status
        .forge
        .iter()
        .find(|f| f.source == "github.com/o/r")
        .unwrap_or_else(|| panic!("status shows the source: {:?}", status.forge));
    assert_eq!(row.checked_at, rig_now(&rig));
    assert_eq!(row.answered_at, Some(rig_now(&rig)));
    assert_eq!(row.commit.as_deref(), Some("aaaaaaaaaaaa1"));
    assert!(row.error.is_none(), "{row:?}");

    // A failure is visible in exactly the same place, rather than only in a receipt nobody kept.
    git.fail_with(FetchFault::Unreachable);
    forge_interval_elapsed(&rig);
    quiet_sweep(&ctx, &plane, &dir, &git);
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let row = listed
        .data
        .forge
        .iter()
        .find(|f| f.source == "github.com/o/r")
        .unwrap_or_else(|| panic!("list shows the source: {:?}", listed.data.forge));
    assert!(row.error.is_some(), "{row:?}");
    assert_eq!(
        row.answered_at,
        Some(rig_now(&rig)),
        "the last ANSWER survives a later failure"
    );
    // And the LISTING says it ON THE ROWS THE SOURCE FROZE, never as a block under the list: the
    // reader is already looking at the row, and the row is what stopped moving. `[not responding]`
    // REPLACES `[current]` — current would be measured against an answer nobody got — and the last
    // ANSWER is what says how stale the copy is (the sweep's clock advances on failure, so a
    // "last checked" here would always be recent and read like a blip to ignore).
    let text = crate::render::list_tty(&listed);
    assert!(text.contains("[not responding]"), "{text}");
    assert!(text.contains("from github.com/o/r"), "{text}");
    assert!(text.contains("last answered:"), "{text}");
    assert!(!text.contains("[current]"), "{text}");
    assert!(!text.contains("external sources:"), "{text}");
}

#[test]
fn a_healthy_external_source_is_named_on_its_rows_and_nowhere_else() {
    // The other half of the rule above: nothing is wrong, so the listing is ONE list of bundles.
    // The repository appears in the `from` column of the rows it delivers and gets no block of its
    // own — the block used to restate, under the list, an identity every row already carried.
    let rig = Rig::new("repo-healthy-quiet");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    assert!(
        listed.data.forge.iter().all(|f| f.error.is_none()),
        "{:?}",
        listed.data.forge
    );
    let text = crate::render::list_tty(&listed);
    assert!(text.contains("from github.com/o/r"), "{text}");
    assert!(!text.contains("external sources:"), "{text}");
}

#[test]
fn a_granted_origin_flows_in_both_scopes() {
    // The add's consent moment records the origin in the MACHINE registry; a later project-scope
    // row of the same origin then flows on explicit update — no second ceremony, no refusal —
    // installing into the checkout's own store.
    let rig = Rig::new("trust-both-scopes");
    rig.write_global("[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r/alpha");

    // A fresh checkout spells the same repo row: the update installs into the PROJECT's own store.
    // Scopes are unblended, so the machine's landing neither helps nor hinders the checkout's.
    let proj = project("proj-granted", "[skills]\nalpha = \"github:o/r\"\n");
    let pctx = rig.ctx_at(Some(&proj.0));
    let out = update_now(&pctx, &plane, &dir, &git);
    assert!(
        proj.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "the granted origin's member lands in the checkout: {:?}",
        out.warnings
    );
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).is_some(),
        "installed through the project's own store"
    );
}
