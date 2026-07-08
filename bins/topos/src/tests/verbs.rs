//! End-to-end verb invariants over the injected seams: deterministic `add` minting, `add → list`, the
//! `--footprint` oracle, the I/O-side rejects, `diff`/`log`, `uninstall` byte-safety, and `add` under
//! fault (draft survival + all-or-nothing). The `--json` envelopes are asserted byte-equal to the
//! committed goldens.

use std::path::{Path, PathBuf};

use serde_json::Value;
use topos_harness::{ClaudeCode, DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::Lock;
use topos_types::{CurrencyKind, HarnessId, JsonEnvelope, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::doc;
use crate::fs_seam::{FaultFs, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::sidecar::Layout;
use crate::{ops, render};

const DEVICE_ID: &str = "d_test";
const FIXED_MILLIS: u64 = 1_700_000_000_000;

/// Parse a minted skill id through the validated newtype (always charset-clean in these rigs).
fn sid(id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(id).expect("rig skill id is charset-clean")
}

/// The test shim over [`ops::pull`]: project the schema payload (warnings have dedicated tests).
fn pull_data(
    ctx: &Ctx<'_>,
    scope: ops::PullScope,
) -> Result<topos_types::results::PullData, crate::error::ClientError> {
    ops::pull(ctx, scope).map(|o| o.data)
}

/// A borrow-free no-op harness for the tests that don't exercise harness recognition: it discovers
/// nothing (so `add` never tags a plain temp source as a harness skill) and installs/removes nothing.
/// The Claude Code adapter itself is tested directly (its own crate tests + the dedicated tests below).
#[derive(Debug)]
struct NoHarness;

impl HarnessAdapter for NoHarness {
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
        _: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: PathBuf::from(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::SessionStart
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        no_harness_report()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        no_harness_report()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

fn no_harness_report() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::SessionStart,
        touched_path: None,
        marker_id: "test:none".to_owned(),
        state: TriggerState::Inactive,
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-vt-{tag}-{}-{n}", std::process::id()));
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

fn fixture_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr-describe")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Copy a directory tree (the fixture into an editable temp source).
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

fn editable_source() -> Scratch {
    let s = Scratch::new("src");
    let root = s.0.join("pr-describe");
    copy_tree(&fixture_src(), &root);
    s
}

/// Build a deterministic context over a home dir.
struct Harness {
    home: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NoHarness,
    plane: crate::plane::InertPlane,
    follow: crate::plane::InertFollow,
}
impl Harness {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(tag),
            fs: RealFs,
            ids: SeqIds::new("t"),
            clock: FixedClock(FIXED_MILLIS),
            harness: NoHarness,
            plane: crate::plane::InertPlane,
            follow: crate::plane::InertFollow,
        }
    }
    fn ctx(&self) -> Ctx<'_> {
        self.ctx_with(&self.harness)
    }
    /// A context over an explicit harness adapter (for the Claude Code recognition / hook tests).
    fn ctx_with<'a>(&'a self, harness: &'a dyn HarnessAdapter) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE_ID.to_owned(),
            layout: Layout::new(&self.home.0),
            harness,
            plane: &self.plane,
            plane_key: [0u8; 32],
            follow: &self.follow,
        }
    }
}

fn envelope_string(command: &str, value: Value) -> String {
    let env = render::ok_envelope(command, value);
    serde_json::to_string_pretty(&env).unwrap() + "\n"
}

fn assert_golden(name: &str, command: &str, value: Value) {
    let got = envelope_string(command, value);
    let path = workspace_root().join(format!("contracts/fixtures/json/{name}.json"));
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(got, want, "golden {name} mismatch.\nACTUAL:\n{got}");
}

#[test]
fn add_minting_is_deterministic_and_names_from_frontmatter() {
    let src = editable_source();
    let root = src.0.join("pr-describe");

    let h1 = Harness::new("mint1");
    let a1 = ops::add(&h1.ctx(), &root).unwrap();
    let h2 = Harness::new("mint2");
    let a2 = ops::add(&h2.ctx(), &root).unwrap();

    // Identity over bytes + device id + message is stable; the random skill id is not part of it.
    assert_eq!(a1.version_id, a2.version_id);
    assert_eq!(a1.bundle_digest, a2.bundle_digest);
    assert_eq!(
        a1.name, "pr-describe",
        "name comes from SKILL.md frontmatter"
    );
    assert_eq!(a1.skill_id, "topos_t00", "deterministic injected id");
    assert!(a1.tracked);
    assert_ne!(
        a1.version_id, a1.bundle_digest,
        "version_id and bundle_digest are distinct"
    );

    // The committed add golden equals the real output.
    assert_golden("add.ok", "add", serde_json::to_value(&a1).unwrap());
}

#[test]
fn add_then_list_finds_tracked_and_pins_lock_shape() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("addlist");
    let add = ops::add(&h.ctx(), &root).unwrap();

    let list = ops::list(&h.ctx(), None, false).unwrap().data;
    assert_eq!(list.tracked.len(), 1);
    let entry = &list.tracked[0];
    assert_eq!(entry.skill, "pr-describe");
    assert_eq!(entry.version_id, add.version_id);
    assert_eq!(entry.bundle_digest, add.bundle_digest);
    assert!(!entry.draft, "freshly added skill has no draft");
    assert!(
        entry.workspace_id.is_none(),
        "a locally-adopted skill has no workspace"
    );
    assert!(
        list.followed.is_empty() && list.published_by_you.is_empty() && list.untracked.is_empty()
    );

    // The lock.json on-disk instance shape (sorted files: path, mode, sha256, size + base_commit).
    let lock: Lock = doc::read_doc(
        h.ctx().fs,
        &h.ctx().layout.published(&sid(&add.skill_id)).lock,
    )
    .unwrap()
    .unwrap();
    assert_eq!(lock.schema_version, 1);
    assert_eq!(lock.name, "pr-describe");
    assert_eq!(lock.base_commit, add.version_id);
    assert_eq!(lock.bundle_digest, add.bundle_digest);
    let paths: Vec<&str> = lock.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["SKILL.md", "reference/usage.md"],
        "files sorted by path bytes"
    );
    assert!(
        lock.files
            .iter()
            .all(|f| f.mode == "100644" && f.sha256.len() == 64)
    );

    assert_golden("list.ok", "list", serde_json::to_value(&list).unwrap());
}

#[test]
fn footprint_oracle_equals_the_created_set_and_catches_a_stray_write() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("foot");
    let layout = h.ctx().layout.clone();

    ops::add(&h.ctx(), &root).unwrap();

    // Ground truth: every path under the home (topos never writes the user source dir).
    let mut ground = fs_tree(&h.home.0);
    let mut reported = ops::list(&h.ctx(), None, true)
        .unwrap()
        .data
        .footprint
        .unwrap();
    ground.sort();
    reported.sort();
    assert_eq!(
        reported, ground,
        "footprint must equal the created set under the home"
    );

    // Adversarial: a stray file under the home must appear in the footprint walk.
    let stray = layout.home().join("stray-unregistered");
    std::fs::write(&stray, b"x").unwrap();
    let reported = ops::list(&h.ctx(), None, true)
        .unwrap()
        .data
        .footprint
        .unwrap();
    assert!(
        reported.iter().any(|p| p == &stray.to_string_lossy()),
        "the footprint walk must reflect a stray write"
    );
}

#[test]
fn add_rejects_a_symlink_and_writes_nothing() {
    let src = Scratch::new("sym");
    let root = src.0.join("skill");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("SKILL.md"), b"# s\n").unwrap();
    std::os::unix::fs::symlink("/etc/hosts", root.join("link")).unwrap();

    let h = Harness::new("symhome");
    let err = ops::add(&h.ctx(), &root).unwrap_err();
    assert!(
        matches!(err, crate::error::ClientError::Scan(_)),
        "got {err:?}"
    );
    // Nothing tracked.
    assert!(
        ops::list(&h.ctx(), None, false)
            .unwrap()
            .data
            .tracked
            .is_empty()
    );
}

#[test]
fn add_rejects_empty_bundle_and_source_overlapping_home() {
    // Empty bundle.
    let empty = Scratch::new("empty");
    let root = empty.0.join("skill");
    std::fs::create_dir_all(&root).unwrap();
    let h = Harness::new("emptyhome");
    assert!(matches!(
        ops::add(&h.ctx(), &root).unwrap_err(),
        crate::error::ClientError::EmptyBundle
    ));

    // Source inside the home -> overlap reject (else uninstall would delete user bytes).
    let h2 = Harness::new("ovhome");
    let inside = h2.home.0.join("inside-skill");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("SKILL.md"), b"# s\n").unwrap();
    assert!(matches!(
        ops::add(&h2.ctx(), &inside).unwrap_err(),
        crate::error::ClientError::SourceOverlap
    ));
}

#[test]
fn add_rejects_a_fifo_and_handles_a_casefold_collision() {
    // A non-regular file (fifo) is rejected typed, nothing tracked.
    let src = Scratch::new("fifo");
    let root = src.0.join("skill");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("SKILL.md"), b"# s\n").unwrap();
    let made_fifo = std::process::Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if made_fifo {
        let h = Harness::new("fifohome");
        assert!(matches!(
            ops::add(&h.ctx(), &root).unwrap_err(),
            crate::error::ClientError::Scan(_)
        ));
        assert!(
            ops::list(&h.ctx(), None, false)
                .unwrap()
                .data
                .tracked
                .is_empty()
        );
    }

    // A case-fold collision (`Readme.md` vs `readme.md`): a case-insensitive FS collapses them (add
    // succeeds with one), a case-sensitive FS keeps both (the kernel rejects typed) — both honor the gate.
    let src2 = Scratch::new("cf");
    let root2 = src2.0.join("skill");
    std::fs::create_dir_all(&root2).unwrap();
    std::fs::write(root2.join("SKILL.md"), b"# s\n").unwrap();
    std::fs::write(root2.join("Readme.md"), b"a\n").unwrap();
    std::fs::write(root2.join("readme.md"), b"b\n").unwrap();
    let h2 = Harness::new("cfhome");
    match ops::add(&h2.ctx(), &root2) {
        Ok(_) => {} // the FS collapsed the colliding pair into one file
        Err(e) => assert!(matches!(e, crate::error::ClientError::Scan(_)), "got {e:?}"),
    }
}

#[test]
fn error_envelope_is_coded_retryability_aware_and_leak_free() {
    use crate::error::ClientError;

    // Ambiguous name -> the frozen code + the disambiguate next-action.
    let amb = ClientError::AmbiguousName {
        name: "x".into(),
        count: 2,
    };
    let env = render::err_envelope("list", &amb);
    assert!(!env.ok);
    let err = env.error.as_ref().unwrap();
    assert_eq!(err.code, "AMBIGUOUS_NAME");
    assert_eq!(err.outcome, topos_types::TerminalOutcome::AmbiguousName);
    assert!(!err.retryable);
    assert_eq!(env.next_actions.len(), 1);
    assert_eq!(
        env.next_actions[0].code,
        topos_types::ActionCode::DisambiguateName
    );

    // Corrupt must not leak the inner serde detail to --json or TTY.
    let corrupt = ClientError::Corrupt("secret-serde-detail-xyzzy".into());
    let env = render::err_envelope("list", &corrupt);
    let msg = env.error.unwrap().context["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!msg.contains("xyzzy"), "safe_message leaked: {msg}");
    assert!(!render::err_tty(&corrupt).contains("xyzzy"));

    // A store-side IO failure is retryable, like a client-side one.
    let io = ClientError::Gitstore(topos_gitstore::GitstoreError::Io("disk full".into()));
    assert!(render::err_envelope("add", &io).error.unwrap().retryable);
}

#[test]
fn list_by_ambiguous_name_is_typed() {
    let h = Harness::new("ambig");
    // Two DISTINCT directories that share a name -> two distinct tracked skills (legitimate), so a name
    // lookup is ambiguous. (Re-adding the SAME dir is refused as ALREADY_TRACKED — a different case,
    // covered separately.)
    let src_a = editable_source();
    let src_b = editable_source();
    ops::add(&h.ctx(), &src_a.0.join("pr-describe")).unwrap();
    ops::add(&h.ctx(), &src_b.0.join("pr-describe")).unwrap();

    let err = ops::list(&h.ctx(), Some("pr-describe"), false).unwrap_err();
    assert!(matches!(
        err,
        crate::error::ClientError::AmbiguousName { count: 2, .. }
    ));
    // No such name -> typed.
    assert!(matches!(
        ops::list(&h.ctx(), Some("nope"), false).unwrap_err(),
        crate::error::ClientError::NoSuchSkill { .. }
    ));
}

#[test]
fn diff_is_empty_when_clean_and_a_golden_when_edited() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("diff");
    ops::add(&h.ctx(), &root).unwrap();

    // Clean: an unmodified added skill has an empty diff.
    let clean = ops::diff(&h.ctx(), "pr-describe", None).unwrap();
    assert!(
        clean.diff.is_empty(),
        "unmodified -> empty diff, got: {:?}",
        clean.diff
    );

    // A fixed edit -> a fixed golden diff body.
    std::fs::write(
        root.join("SKILL.md"),
        "---\nname: pr-describe\n---\n\n# PR describe\n\nWrite a GREAT PR description.\n",
    )
    .unwrap();
    let edited = ops::diff(&h.ctx(), "pr-describe", None).unwrap();
    assert_eq!(edited.source, topos_types::results::DiffSource::Local);
    assert_golden("diff.ok", "diff", serde_json::to_value(&edited).unwrap());
}

#[test]
fn diff_reports_the_draft_digest_not_the_base() {
    // A bare `diff` compares draft ↔ current; its `bundle_digest` must be the DRAFT's digest — the
    // byte-exact value `publish <skill>@<digest>` consents to (the bytes being shipped),
    // NOT the base/current version's (which would yield CONSENT_MISMATCH on any real change).
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("diff-digest");
    let add = ops::add(&h.ctx(), &root).unwrap();

    // Edit the draft so it diverges from current.
    std::fs::write(
        root.join("SKILL.md"),
        "---\nname: pr-describe\n---\n\n# PR describe\n\nWrite a GREAT PR description.\n",
    )
    .unwrap();

    let edited = ops::diff(&h.ctx(), "pr-describe", None).unwrap();

    // The reported digest equals an independent scan of the on-disk draft (adopt-in-place → `root`)…
    let draft_digest = topos_core::digest::to_hex(&crate::scan::scan(&root).unwrap().bundle_digest);
    assert_eq!(
        edited.bundle_digest, draft_digest,
        "diff reports the draft's byte-exact digest"
    );
    // …and differs from the base/current version's digest (the value publish would have rejected).
    assert_ne!(
        edited.bundle_digest, add.bundle_digest,
        "the draft digest is not the base digest"
    );
    // The diffed endpoint (version_id) stays the base commit — only the consent digest moved.
    assert_eq!(edited.version_id, add.version_id);
}

#[test]
fn log_reports_local_action_and_git_history() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("log");
    let add = ops::add(&h.ctx(), &root).unwrap();

    let log = ops::log(&h.ctx(), "pr-describe").unwrap();
    assert_eq!(log.team, None);
    // The add action + the genesis version.
    let actions: Vec<&str> = log
        .events
        .iter()
        .filter_map(|e| e.get("action").and_then(Value::as_str))
        .collect();
    assert!(
        actions.contains(&"add") && actions.contains(&"version"),
        "actions: {actions:?}"
    );
    assert!(
        log.events
            .iter()
            .any(|e| e.get("version_id").and_then(Value::as_str) == Some(add.version_id.as_str()))
    );
    assert_golden("log.ok", "log", serde_json::to_value(&log).unwrap());
}

#[test]
fn uninstall_removes_home_and_binary_but_no_skill_bytes() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let before = fs_hashes(&root);

    let h = Harness::new("uninst");
    ops::add(&h.ctx(), &root).unwrap();
    // A fake binary to remove (never the test runner).
    let fake_bin = h.home.0.parent().unwrap().join("topos-fake-bin");
    std::fs::write(&fake_bin, b"binary").unwrap();

    let out = ops::uninstall(&h.ctx(), true, Some(&fake_bin)).unwrap();
    assert!(out.home_removed);
    assert!(out.footprint.is_some());
    assert_eq!(
        out.binary_removed.as_deref(),
        Some(fake_bin.to_string_lossy().as_ref())
    );

    assert!(!h.home.0.exists(), "~/.topos removed");
    assert!(!fake_bin.exists(), "binary removed");
    // The user's source skill dir is byte-for-byte unchanged.
    assert_eq!(
        fs_hashes(&root),
        before,
        "uninstall must not touch skill bytes"
    );
    let _ = std::fs::remove_file(&fake_bin);
}

#[test]
fn add_under_fault_preserves_draft_and_is_all_or_nothing() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let before = fs_hashes(&root);
    // The temp source is not under any harness home, so recognition is a no-op (no extra durable ops);
    // a borrow-free stub keeps the fault sweep's op count exactly the sidecar adoption's.
    let no_harness = NoHarness;
    // `add` never reads the plane/follow seams; the inert pair satisfies `Ctx` without extra durable ops.
    let no_plane = crate::plane::InertPlane;
    let no_follow = crate::plane::InertFollow;

    // How many durable ops a clean add performs (so we fault each). A non-faulting FaultFs counts them.
    let probe_home = Scratch::new("probe");
    let probe_fs = FaultFs::new(0);
    let probe_ids = SeqIds::new("t");
    let probe_clock = FixedClock(FIXED_MILLIS);
    let probe_ctx = Ctx {
        fs: &probe_fs,
        ids: &probe_ids,
        clock: &probe_clock,
        device_id: DEVICE_ID.to_owned(),
        layout: Layout::new(&probe_home.0),
        harness: &no_harness,
        plane: &no_plane,
        plane_key: [0u8; 32],
        follow: &no_follow,
    };
    ops::add(&probe_ctx, &root).unwrap();
    let max_ops = probe_fs.ops_attempted();

    for fail_at in 1..=max_ops {
        let home = Scratch::new("fault");
        let fs = FaultFs::new(fail_at);
        let ids = SeqIds::new("t");
        let clock = FixedClock(FIXED_MILLIS);
        let layout = Layout::new(&home.0);
        let ctx = Ctx {
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: DEVICE_ID.to_owned(),
            layout: layout.clone(),
            harness: &no_harness,
            plane: &no_plane,
            plane_key: [0u8; 32],
            follow: &no_follow,
        };
        let result = ops::add(&ctx, &root);

        // (d) The user's source bytes are always intact, no matter where the fault landed.
        assert_eq!(
            fs_hashes(&root),
            before,
            "fail_at={fail_at}: source must be untouched"
        );

        // Recover, then read the state with a clean fs.
        let real = RealFs;
        crate::sidecar::recover(&real, &layout, 0).unwrap();
        let clean_ids = SeqIds::new("t");
        let clean_ctx = Ctx {
            fs: &real,
            ids: &clean_ids,
            clock: &clock,
            device_id: DEVICE_ID.to_owned(),
            layout: layout.clone(),
            harness: &no_harness,
            plane: &no_plane,
            plane_key: [0u8; 32],
            follow: &no_follow,
        };
        let tracked = ops::list(&clean_ctx, None, false).unwrap().data.tracked;

        // All-or-nothing: the staging-rename is the commit point, so the skill is either absent (fault
        // before the publish) or COMPLETE (fault at/after it) — never a half/corrupt state. A faulted
        // post-publish step (a final fsync or the log append) still leaves a fully usable skill.
        assert!(
            tracked.len() <= 1,
            "fail_at={fail_at}: at most one skill, found {tracked:?}"
        );
        if let Some(entry) = tracked.first() {
            assert_eq!(
                entry.version_id,
                "d77b648d8149d63189864c6b6d06da4f7919935c4242cc197e708b1dafe941d5"
            );
            // Complete + usable: it renders + diffs without an integrity error.
            ops::diff(&clean_ctx, "pr-describe", None).unwrap_or_else(|e| {
                panic!("fail_at={fail_at}: tracked skill must be usable: {e:?}")
            });
        }
        if result.is_ok() {
            assert_eq!(
                tracked.len(),
                1,
                "fail_at={fail_at}: a clean add must be complete"
            );
        }

        // Recovery is idempotent.
        let before_fp = crate::sidecar::footprint(&real, &layout).unwrap();
        crate::sidecar::recover(&real, &layout, 0).unwrap();
        assert_eq!(
            before_fp,
            crate::sidecar::footprint(&real, &layout).unwrap()
        );
    }
}

/// Lay down a real Claude Code skill (`<claude_home>/skills/<name>/SKILL.md`) and return its dir.
fn claude_skill(claude_home: &Path, name: &str, body: &str) -> PathBuf {
    let dir = claude_home.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
    dir
}

#[test]
fn add_recognizes_a_claude_code_skill_tags_it_installs_the_hook_and_writes_nothing() {
    let h = Harness::new("cc-add");
    let claude = Scratch::new("cc-home");
    // Frontmatter `name` deliberately differs from the dir name — the DIRECTORY name (the command name)
    // must win for a recognized Claude Code skill.
    let skill = claude_skill(
        &claude.0,
        "pr-describe",
        "---\nname: not-the-command-name\n---\n\n# PR describe\n\nWrite a clear PR description.\n",
    );
    let before = fs_hashes(&skill);

    let cfg = RealFs;
    let cc = ClaudeCode::new(claude.0.clone(), &cfg);
    let ctx = h.ctx_with(&cc);

    let add = ops::add(&ctx, &skill).unwrap();
    assert_eq!(
        add.name, "pr-describe",
        "name is the directory basename, not frontmatter"
    );
    assert_eq!(
        add.harness,
        Some(HarnessId::ClaudeCode),
        "tagged as Claude Code"
    );
    let report = add.currency.expect("currency armed for a recognized skill");
    assert_eq!(report.state, TriggerState::Active);
    assert_eq!(report.currency_kind, CurrencyKind::SessionStart);

    // Adopt-in-place writes NOTHING into the skill dir — it is byte-identical.
    assert_eq!(
        fs_hashes(&skill),
        before,
        "the skill dir must stay byte-identical"
    );

    // The hook landed in the harness settings.json (the only write outside ~/.topos/).
    let settings = std::fs::read_to_string(claude.0.join("settings.json")).unwrap();
    assert!(
        settings.contains("topos pull --quiet"),
        "hook command installed"
    );
    assert!(settings.contains("# topos:currency"), "sentinel present");

    // The placement was recorded with the harness tag; a list shows it tracked.
    let tracked = ops::list(&ctx, None, false).unwrap().data.tracked;
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0].skill, "pr-describe");
}

#[test]
fn add_of_a_plain_dir_tags_no_harness_and_installs_no_hook() {
    let h = Harness::new("cc-plain");
    let claude = Scratch::new("cc-empty"); // a real (empty) Claude home — the source is NOT under it
    let src = editable_source();
    let cfg = RealFs;
    let cc = ClaudeCode::new(claude.0.clone(), &cfg);
    let ctx = h.ctx_with(&cc);

    let add = ops::add(&ctx, &src.0.join("pr-describe")).unwrap();
    assert!(
        add.harness.is_none(),
        "a plain dir is not a recognized harness skill"
    );
    assert!(add.currency.is_none(), "no currency armed for a plain dir");
    assert!(
        !claude.0.join("settings.json").exists(),
        "a plain-dir add never touches the harness config"
    );
}

#[test]
fn re_adding_the_same_dir_is_refused_as_already_tracked() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("dup");
    ops::add(&h.ctx(), &root).unwrap();

    let err = ops::add(&h.ctx(), &root).unwrap_err();
    assert!(
        matches!(err, crate::error::ClientError::AlreadyTracked { .. }),
        "re-adding the same dir must be refused, got {err:?}"
    );
    assert_eq!(
        ops::list(&h.ctx(), None, false).unwrap().data.tracked.len(),
        1,
        "no second record was minted"
    );
}

#[test]
fn uninstall_scrubs_the_hook_and_leaves_claude_skills_byte_identical() {
    let h = Harness::new("cc-uninst");
    let claude = Scratch::new("cc-uninst-home");
    let skill = claude_skill(&claude.0, "pr-describe", "# pr\nWrite a clear PR.\n");
    let before = fs_hashes(&skill);

    let cfg = RealFs;
    let cc = ClaudeCode::new(claude.0.clone(), &cfg);
    let ctx = h.ctx_with(&cc);
    ops::add(&ctx, &skill).unwrap();
    let settings_path = claude.0.join("settings.json");
    assert!(
        std::fs::read_to_string(&settings_path)
            .unwrap()
            .contains("topos pull")
    );

    let fake_bin = h.home.0.parent().unwrap().join("topos-fake-cc-bin");
    std::fs::write(&fake_bin, b"binary").unwrap();
    let out = ops::uninstall(&ctx, true, Some(&fake_bin)).unwrap();

    // The hook was scrubbed; settings.json is still valid JSON without our entry.
    assert_eq!(out.currency.as_ref().unwrap().state, TriggerState::Inactive);
    let settings = std::fs::read_to_string(&settings_path).unwrap();
    assert!(!settings.contains("topos pull"), "the managed hook is gone");
    serde_json::from_str::<Value>(&settings).expect("settings.json stays valid JSON");

    // --footprint disclosed the settings.json path (captured before the scrub), never as a delete.
    let footprint = out.footprint.unwrap();
    assert!(
        footprint.iter().any(|p| p.ends_with("settings.json")),
        "settings.json disclosed in the footprint: {footprint:?}"
    );
    assert!(
        settings_path.exists(),
        "settings.json is scrubbed, never deleted"
    );

    // The user's skill dir is byte-for-byte unchanged, and ~/.topos + the binary are gone.
    assert_eq!(
        fs_hashes(&skill),
        before,
        "uninstall must not touch skill bytes"
    );
    assert!(out.home_removed && !h.home.0.exists());
    assert!(!fake_bin.exists());
    let _ = std::fs::remove_file(&fake_bin);
}

#[test]
fn install_currency_trigger_is_crash_safe_across_the_fault_table() {
    // A realistic pre-existing settings.json: a foreign top-level key + a non-SessionStart hook.
    let claude = Scratch::new("cc-fault");
    let settings_path = claude.0.join("settings.json");
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n";

    // Count the durable ops a clean install performs, so we fault each.
    std::fs::write(&settings_path, original).unwrap();
    let probe = FaultFs::new(0);
    ClaudeCode::new(claude.0.clone(), &probe).install_currency_trigger();
    let max_ops = probe.ops_attempted();
    assert!(
        max_ops >= 4,
        "the atomic config write performs at least temp/fsync/rename/fsync-dir"
    );

    for fail_at in 1..=max_ops {
        std::fs::write(&settings_path, original).unwrap(); // reset to the pre-state
        let fs = FaultFs::new(fail_at);
        let _ = ClaudeCode::new(claude.0.clone(), &fs).install_currency_trigger();

        // After a fault at any step, settings.json is the pre- or post-state — never torn — so it always
        // parses as JSON and the user's foreign content survives intact.
        let bytes = std::fs::read(&settings_path).unwrap();
        let root: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("fail_at={fail_at}: settings.json torn (invalid JSON): {e}")
        });
        assert_eq!(
            root["model"], "opus",
            "fail_at={fail_at}: foreign key must survive"
        );
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "fail_at={fail_at}: the sibling hook must survive"
        );
    }
}

#[test]
fn pull_is_an_honest_empty_no_op() {
    // The inert follow source (production) follows nothing, so the bare sweep is an honest empty no-op.
    let h = Harness::new("pull");
    let data = pull_data(&h.ctx(), ops::PullScope::AllFollowed).unwrap();
    assert!(data.skills.is_empty(), "nothing is followed yet");
    assert_eq!(data.proposals_awaiting, 0);
}

/// Every path under a directory (files + dirs), for the footprint oracle.
fn fs_tree(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                out.push(p.to_string_lossy().into_owned());
                if p.is_dir() {
                    walk(&p, out);
                }
            }
        }
    }
    walk(root, &mut out);
    out
}

/// Per-file sha256 of a tree (relative path -> hash), for the byte-unchanged invariant.
fn fs_hashes(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                let bytes = std::fs::read(&p).unwrap();
                out.push((
                    rel,
                    topos_core::digest::to_hex(&topos_core::digest::sha256(&bytes)),
                ));
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Used by the golden tests to surface the exact JSON during bring-up.
#[allow(dead_code)]
fn dump(value: &JsonEnvelope) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

// ── unfollow: flag-flip only — bytes kept, token retained, idempotent, sweep skips ──────────────────

/// Read every file (path → bytes) under a dir, sorted — the byte-identity oracle for I-KEEP-LOCAL.
fn dir_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let p = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(&p, base, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.push((rel, std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn unfollow_flips_follow_state_keeps_bytes_and_is_idempotent() {
    use crate::enroll::{self, FollowEntry, FollowModeDoc};

    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("unfollow");
    let ctx = h.ctx();
    let a = ops::add(&ctx, &root).unwrap();

    // Seed the enrolled follow-state for the tracked skill (what a real `follow` promote writes).
    enroll::write_follows_merged(
        ctx.fs,
        &ctx.layout,
        &[FollowEntry {
            skill_id: a.skill_id.clone(),
            workspace_id: "w_acme".to_owned(),
            read_token: "rt_secret".to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following: true,
        }],
    )
    .unwrap();

    let before = dir_bytes(&root);
    let u = ops::unfollow(&ctx, "pr-describe").unwrap();
    assert_eq!(u.skill_id, a.skill_id);
    assert!(!u.following);
    assert!(u.bytes_kept);
    // The committed golden equals the real output.
    assert_golden("unfollow.ok", "unfollow", serde_json::to_value(&u).unwrap());

    // The placement bytes are byte-identical — unfollow never touches a skill file (I-KEEP-LOCAL).
    assert_eq!(before, dir_bytes(&root));

    // The entry flipped in place, retaining workspace/token/mode so a later follow resumes.
    let follows = enroll::read_follows(ctx.fs, &ctx.layout).unwrap().unwrap();
    let e = follows
        .follows
        .iter()
        .find(|e| e.skill_id == a.skill_id)
        .unwrap();
    assert!(!e.following);
    assert_eq!(e.read_token, "rt_secret");
    assert_eq!(e.workspace_id, "w_acme");

    // Idempotent: a second unfollow is the same clean success, and the doc is unchanged.
    let u2 = ops::unfollow(&ctx, "pr-describe").unwrap();
    assert!(!u2.following);
    assert!(u2.bytes_kept);
    assert_eq!(
        enroll::read_follows(ctx.fs, &ctx.layout).unwrap().unwrap(),
        follows
    );

    // The bare sweep skips the unfollowed skill (the currency subscription is off).
    let file_follow = crate::plane_http::FileFollow::new(enroll::follow_contexts(&follows));
    let sweep_ctx = Ctx {
        follow: &file_follow,
        ..h.ctx()
    };
    let data = pull_data(&sweep_ctx, ops::PullScope::AllFollowed).unwrap();
    assert!(
        data.skills.is_empty(),
        "an unfollowed skill must not be swept: {:?}",
        data.skills
    );

    // A re-follow resumes: the promote path replace-by-skill_id flips `following` back on (with a fresh
    // token), and the entry is swept again.
    enroll::write_follows_merged(
        ctx.fs,
        &ctx.layout,
        &[FollowEntry {
            following: true,
            read_token: "rt_reminted".to_owned(),
            ..e.clone()
        }],
    )
    .unwrap();
    let resumed = enroll::read_follows(ctx.fs, &ctx.layout).unwrap().unwrap();
    let r = resumed
        .follows
        .iter()
        .find(|e| e.skill_id == a.skill_id)
        .unwrap();
    assert!(r.following);
    assert_eq!(r.read_token, "rt_reminted");
}

#[test]
fn unfollow_of_a_tracked_but_never_followed_skill_is_a_clean_success() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("unfollow-nf");
    let ctx = h.ctx();
    let a = ops::add(&ctx, &root).unwrap();

    // No follows.json at all — an add-only local skill is already not followed.
    let u = ops::unfollow(&ctx, "pr-describe").unwrap();
    assert_eq!(u.skill_id, a.skill_id);
    assert!(!u.following);
    assert!(u.bytes_kept);
    assert!(
        crate::enroll::read_follows(ctx.fs, &ctx.layout)
            .unwrap()
            .is_none(),
        "unfollow writes nothing when there is nothing to flip"
    );

    // An unknown name is the sibling verbs' typed error, not a silent success.
    assert!(matches!(
        ops::unfollow(&ctx, "no-such-skill"),
        Err(crate::error::ClientError::NoSuchSkill { .. })
    ));
}

#[test]
fn list_discloses_enrollment_follow_state_and_hook() {
    use crate::enroll::{self, FollowEntry, FollowModeDoc, Instance, Membership, UserDoc};
    use topos_types::bootstrap::{DeploymentMode, VerifiedDomainStatus};

    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("listenroll");
    let ctx = h.ctx();
    let a = ops::add(&ctx, &root).unwrap();

    // Unenrolled: no header data, empty followed bucket — the accountless view is unchanged.
    let out = ops::list(&ctx, None, false).unwrap();
    assert!(out.enrollment.is_none());
    assert!(out.data.followed.is_empty());

    // Seed what a real `follow` promote writes: instance.json + user.json (the workspace membership) + a
    // followed entry for the tracked skill.
    enroll::write_instance(
        ctx.fs,
        &ctx.layout,
        &Instance {
            schema_version: 1,
            base_url: "https://topos.example".to_owned(),
            plane_key: "a".repeat(64),
            plane_key_id: "pk_demo".to_owned(),
            deployment_mode: DeploymentMode::SelfHost,
            enrollment_method: "device_code".to_owned(),
        },
    )
    .unwrap();
    enroll::write_user(
        ctx.fs,
        &ctx.layout,
        &UserDoc {
            schema_version: 1,
            email: None,
            principal: None,
            workspaces: vec![Membership {
                workspace_id: "w_acme".to_owned(),
                display_name: Some("Acme".to_owned()),
                roles: Vec::new(),
                verified_domain: None,
                verified_domain_status: VerifiedDomainStatus::Unverified,
                invite_rooted: true,
                enrolled_at: 1,
            }],
        },
    )
    .unwrap();
    enroll::write_follows_merged(
        ctx.fs,
        &ctx.layout,
        &[FollowEntry {
            skill_id: a.skill_id.clone(),
            workspace_id: "w_acme".to_owned(),
            read_token: "rt_secret".to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following: true,
        }],
    )
    .unwrap();

    let out = ops::list(&ctx, None, false).unwrap();
    let e = out.enrollment.as_ref().expect("instance.json ⇒ enrolled");
    assert_eq!(
        e.workspace_labels,
        vec![("w_acme".to_owned(), "Acme".to_owned())]
    );
    assert_eq!(e.base_url, "https://topos.example");
    assert!(!e.hook_active, "NoHarness holds no managed hook entry");
    // The followed bucket is the tracked subset follows.json selects (schema-compatible SkillEntry rows),
    // each stamped with its workspace provenance.
    assert_eq!(out.data.followed.len(), 1);
    assert_eq!(out.data.followed[0].skill, "pr-describe");
    assert_eq!(out.data.followed[0].version_id, a.version_id);
    assert_eq!(out.data.followed[0].workspace_id.as_deref(), Some("w_acme"));
    assert_eq!(out.data.tracked[0].workspace_id.as_deref(), Some("w_acme"));
    assert!(
        matches!(e.notes.as_slice(), [Some(n)] if n.following && n.mode == "auto"),
        "one tracked row, annotated with its follow state"
    );
    let text = render::list_tty(&out);
    // The header names the plane + hook; the skill sits under its workspace's group header.
    assert!(
        text.starts_with("Enrolled at https://topos.example — currency hook: not installed"),
        "{text}"
    );
    assert!(text.contains("\nAcme:\n"), "{text}");
    assert!(text.contains("(following, auto)"), "{text}");

    // A harness whose managed hook entry IS present reports the hook active.
    struct HookedHarness;
    impl HarnessAdapter for HookedHarness {
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
            _: Option<&DiscoveredPlacement>,
        ) -> PlacementTarget {
            PlacementTarget {
                dir: PathBuf::from(skill_id),
            }
        }
        fn currency_kind(&self) -> CurrencyKind {
            CurrencyKind::SessionStart
        }
        fn install_currency_trigger(&self) -> TriggerReport {
            no_harness_report()
        }
        fn remove_currency_trigger(&self) -> TriggerReport {
            no_harness_report()
        }
        fn uninstall_footprint(&self) -> Vec<PathBuf> {
            vec![PathBuf::from("/tmp/claude/settings.json")]
        }
    }
    let hooked = HookedHarness;
    let out = ops::list(&h.ctx_with(&hooked), None, false).unwrap();
    assert!(out.enrollment.expect("enrolled").hook_active);

    // Unfollowed: the entry leaves `followed` but stays tracked, disclosed as resumable on the TTY.
    enroll::set_following(ctx.fs, &ctx.layout, &a.skill_id, false).unwrap();
    let out = ops::list(&ctx, None, false).unwrap();
    assert!(out.data.followed.is_empty());
    let text = render::list_tty(&out);
    assert!(
        text.contains("(not following — `topos follow pr-describe` resumes)"),
        "{text}"
    );
}

#[test]
fn follow_approve_resumes_an_unfollowed_skill() {
    use std::collections::HashMap;

    use crate::enroll::{self, FollowEntry, FollowModeDoc, Instance};
    use crate::plane::{EnrollSource, PlaneSource};
    use crate::plane_http::SkillCred;
    use topos_types::bootstrap::DeploymentMode;

    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("resume");
    let ctx = h.ctx();
    let a = ops::add(&ctx, &root).unwrap();

    // Seed what a real `follow` promote writes (instance + follow entry), then pause it — the exact
    // state `unfollow` leaves behind.
    enroll::write_instance(
        ctx.fs,
        &ctx.layout,
        &Instance {
            schema_version: 1,
            base_url: "https://topos.example".to_owned(),
            plane_key: "a".repeat(64),
            plane_key_id: "pk_demo".to_owned(),
            deployment_mode: DeploymentMode::SelfHost,
            enrollment_method: "device_code".to_owned(),
        },
    )
    .unwrap();
    enroll::write_follows_merged(
        ctx.fs,
        &ctx.layout,
        &[FollowEntry {
            skill_id: a.skill_id.clone(),
            workspace_id: "w_acme".to_owned(),
            read_token: "rt_secret".to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: false,
            following: true,
        }],
    )
    .unwrap();
    let u = ops::unfollow(&ctx, "pr-describe").unwrap();
    assert!(!u.following);

    // `follow <skill>` resumes the paused entry. The skill has already been received (its
    // base is the adopted genesis — no pending first-receive offer), so no transport is touched: the
    // connectors panic if reached, and the inert ctx plane would error on any fetch.
    let enroll_connect =
        |_b: &str| -> Box<dyn EnrollSource> { unreachable!("the skill path never enrolls") };
    let plane_connect = |_b: &str, _c: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
        unreachable!("the skill path builds no offer-disclosure transport")
    };
    let connectors = ops::FollowConnectors {
        enroll: &enroll_connect,
        plane: &plane_connect,
    };
    let out = ops::follow(
        &ctx,
        &connectors,
        Some("pr-describe".to_owned()),
        ops::FollowOpts {
            manual: false,
            workspace: None,
        },
    )
    .unwrap();

    // The `--json` payload stays the schema-pinned FollowData shape; the resume rides alongside.
    assert!(out.data.enrolled);
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(out.data.skills[0].name, "pr-describe");
    assert_eq!(out.resumed, vec!["pr-describe".to_owned()]);
    let text = render::follow_tty(&out);
    assert!(
        text.contains("Resumed following pr-describe"),
        "the resume is disclosed on the TTY: {text}"
    );

    // The durable flag flipped back on, credentials retained.
    let follows = enroll::read_follows(ctx.fs, &ctx.layout).unwrap().unwrap();
    let e = follows
        .follows
        .iter()
        .find(|e| e.skill_id == a.skill_id)
        .unwrap();
    assert!(e.following, "the retained entry resumed");
    assert_eq!(e.read_token, "rt_secret");

    // `list` shows (following, mode) again…
    let listed = ops::list(&ctx, None, false).unwrap();
    let en = listed.enrollment.as_ref();
    assert!(
        en.is_some_and(
            |en| matches!(en.notes.as_slice(), [Some(n)] if n.following && n.mode == "auto")
        ),
        "list discloses the resumed follow state"
    );

    // …and a subsequent bare sweep includes the skill again (an up-to-date row, not a skip).
    let file_follow = crate::plane_http::FileFollow::new(enroll::follow_contexts(&follows));
    let sweep_ctx = Ctx {
        follow: &file_follow,
        ..h.ctx()
    };
    let data = pull_data(&sweep_ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        data.skills.len(),
        1,
        "the resumed skill is swept again: {:?}",
        data.skills
    );
}
