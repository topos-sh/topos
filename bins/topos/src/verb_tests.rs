//! End-to-end verb invariants over the injected seams: deterministic `add` minting, `add → list`, the
//! `--footprint` oracle, the I/O-side rejects, `diff`/`log`, `uninstall` byte-safety, and `add` under
//! fault (draft survival + all-or-nothing). The `--json` envelopes are asserted byte-equal to the
//! committed goldens.

use std::path::{Path, PathBuf};

use serde_json::Value;
use topos_types::JsonEnvelope;
use topos_types::persisted::Lock;

use crate::ctx::Ctx;
use crate::doc;
use crate::fs_seam::{FaultFs, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::sidecar::Layout;
use crate::{ops, render};

const DEVICE_ID: &str = "d_test";
const FIXED_MILLIS: u64 = 1_700_000_000_000;

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
}
impl Harness {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(tag),
            fs: RealFs,
            ids: SeqIds::new("t"),
            clock: FixedClock(FIXED_MILLIS),
        }
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE_ID.to_owned(),
            layout: Layout::new(&self.home.0),
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

    let list = ops::list(&h.ctx(), None, false).unwrap();
    assert_eq!(list.tracked.len(), 1);
    let entry = &list.tracked[0];
    assert_eq!(entry.skill, "pr-describe");
    assert_eq!(entry.version_id, add.version_id);
    assert_eq!(entry.bundle_digest, add.bundle_digest);
    assert!(!entry.draft, "freshly added skill has no draft");
    assert!(
        list.followed.is_empty() && list.published_by_you.is_empty() && list.untracked.is_empty()
    );

    // The lock.json on-disk instance shape (sorted files: path, mode, sha256, size + base_commit).
    let lock: Lock = doc::read_doc(h.ctx().fs, &h.ctx().layout.published(&add.skill_id).lock)
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
    let mut reported = ops::list(&h.ctx(), None, true).unwrap().footprint.unwrap();
    ground.sort();
    reported.sort();
    assert_eq!(
        reported, ground,
        "footprint must equal the created set under the home"
    );

    // Adversarial: a stray file under the home must appear in the footprint walk.
    let stray = layout.home().join("stray-unregistered");
    std::fs::write(&stray, b"x").unwrap();
    let reported = ops::list(&h.ctx(), None, true).unwrap().footprint.unwrap();
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
    assert!(ops::list(&h.ctx(), None, false).unwrap().tracked.is_empty());
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
fn list_by_ambiguous_name_is_typed() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("ambig");
    // Two adds of the same bytes/name -> two distinct tracked skills.
    ops::add(&h.ctx(), &root).unwrap();
    ops::add(&h.ctx(), &root).unwrap();

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
    let clean = ops::diff(&h.ctx(), "pr-describe").unwrap();
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
    let edited = ops::diff(&h.ctx(), "pr-describe").unwrap();
    assert_eq!(edited.source, topos_types::results::DiffSource::Local);
    assert_golden("diff.ok", "diff", serde_json::to_value(&edited).unwrap());
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
    assert!(!out.skill_bytes_touched);
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
        crate::sidecar::recover(&real, &layout).unwrap();
        let clean_ids = SeqIds::new("t");
        let clean_ctx = Ctx {
            fs: &real,
            ids: &clean_ids,
            clock: &clock,
            device_id: DEVICE_ID.to_owned(),
            layout: layout.clone(),
        };
        let tracked = ops::list(&clean_ctx, None, false).unwrap().tracked;

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
            ops::diff(&clean_ctx, "pr-describe").unwrap_or_else(|e| {
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
        crate::sidecar::recover(&real, &layout).unwrap();
        assert_eq!(
            before_fp,
            crate::sidecar::footprint(&real, &layout).unwrap()
        );
    }
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
