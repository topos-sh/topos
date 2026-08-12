//! End-to-end verb invariants over the injected seams: deterministic `add` minting, `add → list`, the
//! `--footprint` oracle, the I/O-side rejects, `diff`/`log`, and `add` under fault (draft survival +
//! all-or-nothing). The `--json` envelopes are asserted byte-equal to the committed goldens.

use std::path::{Path, PathBuf};

use serde_json::Value;
use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
use topos_harness::{ClaudeCode, DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::Lock;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

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
}

impl TriggerAdapter for NoHarness {
    fn slug(&self) -> &'static str {
        HarnessId::ClaudeCode.slug()
    }

    fn install(&self) -> TriggerReport {
        no_harness_report()
    }

    fn remove(&self) -> TriggerReport {
        no_harness_report()
    }

    fn artifacts(&self) -> Vec<TriggerArtifact> {
        Vec::new()
    }

    fn present(&self) -> bool {
        !self.artifacts().is_empty()
    }
}

fn no_harness_report() -> TriggerReport {
    TriggerReport {
        agent: "claude-code".to_owned(),
        currency_kind: CurrencyKind::SessionStart,
        touched_path: None,
        marker_id: "test:none".to_owned(),
        state: TriggerState::Inactive,

        note: None,
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
        // Canonical, so recorded/derived/discovered paths compare equal on macOS's symlinked
        // `$TMPDIR` (the same rule every other rig here takes).
        Self(dir.canonicalize().unwrap_or(dir))
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
        self.ctx_with(&self.harness, &self.harness)
    }
    /// A context over an explicit pair of ports — the placement adapter and the trigger — so the
    /// Claude Code recognition tests can swap either half alone.
    fn ctx_with<'a>(
        &'a self,
        harness: &'a dyn HarnessAdapter,
        trigger: &'a dyn TriggerAdapter,
    ) -> Ctx<'a> {
        Ctx {
            progress: crate::progress::silent(),
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE_ID.to_owned(),
            layout: Layout::new(&self.home.0),
            harness,
            triggers: crate::ops::Triggers::active_only(trigger),
            plane: &self.plane,
            follow: &self.follow,
            roots: None,
        }
    }
}

fn envelope_string(command: &str, value: Value) -> String {
    let env = render::ok_envelope(command, value);
    serde_json::to_string_pretty(&env).unwrap() + "\n"
}

/// The machine store's tracked skills as `(name, base_commit)` — the CUSTODY oracle the
/// crash-safety and dedup assertions read (the `list` inventory itself is manifest-resolved now,
/// so a store record without a manifest row deliberately does not appear there).
fn store_records(ctx: &Ctx<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = ctx.fs.read_dir(&ctx.layout.skills_dir()) {
        for entry in entries {
            let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(sid) = crate::id::SkillId::parse(id) else {
                continue;
            };
            if let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&sid).lock)
            {
                out.push((lock.name, lock.base_commit));
            }
        }
    }
    out.sort();
    out
}

fn assert_golden(name: &str, command: &str, value: Value) {
    let got = envelope_string(command, value);
    let path = workspace_root().join(format!("contracts/fixtures/json/{name}.json"));
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(got, want, "golden {name} mismatch.\nACTUAL:\n{got}");
}

/// Byte-equality over a WHOLE envelope a finisher built — payload, line channels, next actions.
/// [`assert_golden`] rebuilds the envelope from `data` alone, so a golden it guards can carry an
/// empty `warnings`/`messages` pair forever while the binary prints a populated one and nothing
/// notices. A golden asserted through here is asserted against the finisher's own bytes.
fn assert_golden_envelope(name: &str, envelope: &topos_types::JsonEnvelope) {
    let got = serde_json::to_string_pretty(envelope).unwrap() + "\n";
    let path = workspace_root().join(format!("contracts/fixtures/json/{name}.json"));
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(got, want, "golden {name} mismatch.\nACTUAL:\n{got}");
}

/// The committed `diff.truncated` golden IS what `topos diff --json` prints on a capped body —
/// including its POPULATED line channels. The fixture generator and this assertion both go
/// through the binary's own producer and its own legacy derivation, so a channel that stops
/// being populated (or a kind, code, or sentence that moves) breaks the golden instead of
/// silently draining it.
#[test]
fn the_capped_diff_golden_is_the_envelope_the_finisher_prints() {
    let want: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            workspace_root().join("contracts/fixtures/json/diff.truncated.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let data: topos_types::results::DiffData =
        serde_json::from_value(want["data"].clone()).unwrap();
    assert!(data.truncated, "the fixture's whole point is the cap");
    let envelope = crate::app::diff_envelope(
        "diff",
        &data,
        vec![
            "topos".to_owned(),
            "diff".to_owned(),
            "pr-describe".to_owned(),
            "--max-bytes".to_owned(),
            "0".to_owned(),
            "--json".to_owned(),
        ],
    );
    assert!(
        !envelope.warnings.is_empty() && !envelope.messages.is_empty(),
        "the golden must exercise a populated pair"
    );
    assert_golden_envelope("diff.truncated", &envelope);
}

/// The same byte-equality over a REFUSAL: the committed example must be exactly what
/// [`render::err_envelope`] emits for that error — code, message, `data`, and the runnable
/// next actions alike.
fn assert_golden_err(name: &str, command: &str, err: &crate::error::ClientError) {
    // The golden refusals are all single-target invocations — the shape a rebuilt command may
    // restate (see `render::err_hint_tty`); a richer one would legitimately offer fewer actions.
    let argv = vec![command.to_owned()];
    let got =
        serde_json::to_string_pretty(&render::err_envelope(command, &argv, err)).unwrap() + "\n";
    let path = workspace_root().join(format!("contracts/fixtures/json/{name}.json"));
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(got, want, "golden {name} mismatch.\nACTUAL:\n{got}");
}

/// A fake remote source: hands back a fixed `.tar.gz` regardless of the spec (the fetch transport is
/// proven separately over loopback; here we exercise the op's extract→place→adopt→origin flow).
struct FakeGit(Vec<u8>);
impl crate::git_source::GitTarballSource for FakeGit {
    fn fetch(
        &self,
        _spec: &crate::source::RemoteSpec,
    ) -> Result<Vec<u8>, crate::error::ClientError> {
        Ok(self.0.clone())
    }
    fn probe(
        &self,
        _spec: &crate::source::RemoteSpec,
    ) -> Result<crate::git_source::RepoHead, crate::error::ClientError> {
        Ok(crate::git_source::RepoHead {
            commit: crate::git_source::extract_tree(&self.0)
                .ok()
                .and_then(|t| t.commit)
                .unwrap_or_default(),
            renamed_to: None,
            retry_after_ms: None,
        })
    }
}

/// Build a `.tar.gz` with a `TOP/` prefix over `(repo-relative path, bytes, mode)` entries.
fn build_repo_tarball(top: &str, entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    for (name, bytes, mode) in entries {
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(*mode);
        h.set_mtime(0);
        tar.append_data(&mut h, format!("{top}/{name}"), *bytes)
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

fn github_spec(owner: &str, repo: &str, git_ref: Option<&str>) -> crate::source::RemoteSpec {
    crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: owner.into(),
        repo: repo.into(),
        git_ref: git_ref.map(str::to_owned),
        subdir: None,
    }
}

#[test]
fn add_remote_imports_a_repo_skill_places_bytes_and_records_origin() {
    let targz = build_repo_tarball(
        "vercel-labs-agent-skills-3f9a2c1",
        &[
            (
                "skills/web-design-guidelines/SKILL.md",
                b"---\nname: web-design-guidelines\n---\n# guide\n",
                0o644,
            ),
            (
                "skills/web-design-guidelines/reference/tokens.md",
                b"tokens",
                0o644,
            ),
            ("LICENSE", b"MIT\n", 0o644),
            ("README.md", b"top-level readme\n", 0o644),
        ],
    );
    let git = FakeGit(targz);
    let spec = github_spec("vercel-labs", "agent-skills", Some("main"));

    let h = Harness::new("remote");
    let project = Scratch::new("remote-proj");
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    // A monorepo with several skills would need `--skill`; here it's the sole skill, so it self-selects.
    let opts = ops::AddRemoteOpts {
        skill: Some("web-design-guidelines".into()),
        harness: None, // default = the active harness (claude-code) → project `.claude/skills`
        dest_root: None,
        global: false,
    };

    let data = ops::add_remote(&h.ctx(), &git, &spec, &roots, &opts).expect("import succeeds");
    assert_eq!(data.name, "web-design-guidelines");
    let origin = data.origin.expect("a remote import records its origin");
    assert_eq!(origin.source, "github.com/vercel-labs/agent-skills");
    assert_eq!(origin.git_ref.as_deref(), Some("main"));
    assert_eq!(origin.commit.as_deref(), Some("3f9a2c1"));
    assert_eq!(
        origin.subdir.as_deref(),
        Some("skills/web-design-guidelines")
    );
    assert_eq!(origin.license.as_deref(), Some("LICENSE"));

    // The bytes landed in the project's claude-code skills dir — only the skill subtree, byte-exact.
    let dest = project.0.join(".claude/skills/web-design-guidelines");
    assert_eq!(
        std::fs::read(dest.join("SKILL.md")).unwrap(),
        b"---\nname: web-design-guidelines\n---\n# guide\n"
    );
    assert!(dest.join("reference/tokens.md").is_file());
    assert!(
        !dest.join("README.md").exists(),
        "only the skill subtree lands, not the repo root"
    );

    // origin.json is persisted in the sidecar for a later re-sync to build on.
    let skill_id = sid(data.skill_id.as_deref().unwrap());
    let origin_doc = h
        .home
        .0
        .join("skills")
        .join(skill_id.as_str())
        .join("origin.json");
    assert!(origin_doc.is_file(), "origin.json written into the sidecar");

    // Re-importing the same skill refuses to clobber — the tracked dir reports ALREADY_TRACKED.
    let err = ops::add_remote(&h.ctx(), &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
}

/// P2 (a previously supported dotfiles setup): a USER-scope skills root that IS a symlink must
/// stage, land, and adopt end to end. The held root handle resolves the link ONCE and pins the
/// real directory, and every staging path is spelled from that handle's canonical path — so the
/// fd-walk's lexical containment check cannot trip over the link spelling. The recorded
/// placement is the canonical path (the convention `add` has always applied by canonicalizing
/// the adopted source).
#[test]
fn add_remote_lands_and_adopts_through_a_symlinked_user_scope_skills_root() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n", 0o644),
            ("skills/alpha/reference/notes.md", b"notes\n", 0o644),
        ],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("symlink-root");
    // The dotfiles shape: a user home whose ~/.claude/skills is a symlink to a real dir kept
    // elsewhere (a separate scratch from the sidecar home — a source may never overlap it).
    let userhome = Scratch::new("symlink-root-home");
    let real_root = userhome.0.join("dotfiles/claude-skills");
    std::fs::create_dir_all(&real_root).unwrap();
    std::fs::create_dir_all(userhome.0.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&real_root, userhome.0.join(".claude/skills")).unwrap();
    let roots = ops::DiscoveryRoots {
        home: userhome.0.clone(),
        cwd: None,
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: true, // user scope → the symlinked ~/.claude/skills
    };
    let data = ops::add_remote(&h.ctx(), &git, &spec, &roots, &opts)
        .expect("a symlinked user-scope root stages, lands, and adopts");
    assert_eq!(data.name, "alpha");
    // One directory object: the bytes are reachable through both spellings, byte-exact.
    let real_dest = real_root.join("alpha");
    assert_eq!(
        std::fs::read(real_dest.join("SKILL.md")).unwrap(),
        b"# alpha\n"
    );
    assert!(
        userhome
            .0
            .join(".claude/skills/alpha/reference/notes.md")
            .is_file()
    );
    // No staging sibling (or park) is left behind under the real root.
    assert_eq!(
        std::fs::read_dir(&real_root).unwrap().count(),
        1,
        "only the landed skill remains under the real root"
    );
    // The recorded placement is the CANONICAL spelling — the pre-existing convention (`add`
    // canonicalizes the adopted source before recording it).
    let sp = Layout::new(&h.home.0).published(&sid(data.skill_id.as_deref().unwrap()));
    let map = doc::read_map(&h.fs, &sp.map)
        .expect("map reads")
        .expect("map exists");
    assert_eq!(
        map.placements,
        vec![
            real_dest
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        ],
        "the placement records the canonical path"
    );
}

/// The staging window is not a licence to delete. A destination validated as EMPTY before the
/// staged tree is built, then filled by someone else while it is being built, must NOT be
/// recursively removed by the landing rename: the import refuses typed and the foreign bytes stay.
#[test]
fn add_remote_refuses_a_destination_that_gained_content_during_staging() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n", 0o644),
            ("skills/alpha/reference/notes.md", b"notes\n", 0o644),
        ],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("remote-race");
    let project = Scratch::new("remote-race-proj");
    let dest = project.0.join(".claude/skills/alpha");
    // The RACE: the first staged write is inside the window between the destination check and the
    // delete + rename that lands the import. A concurrent writer fills the destination there.
    let racer = crate::fs_seam::HookFs::new(|| {
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("SOMEONE-ELSES.md"), b"not topos's bytes\n").unwrap();
    });
    let ctx = Ctx {
        fs: &racer,
        ..h.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&ctx, &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "PLACEMENT_OCCUPIED", "{err:?}");
    // The witness: the racer's bytes are still there, untouched, and nothing of the import landed.
    assert_eq!(
        std::fs::read(dest.join("SOMEONE-ELSES.md")).unwrap(),
        b"not topos's bytes\n"
    );
    assert!(!dest.join("SKILL.md").exists(), "the import did not land");
    // And the staged sibling was cleaned up rather than left behind.
    assert!(
        !project
            .0
            .join(".claude/skills/.topos-import-alpha")
            .exists()
    );
}

/// PARK-THEN-VERIFY, at the sharpest instant there is. The previous test's racer arrives during
/// the staging window — early enough for any re-check to catch. This one arrives in the LAST
/// instant before the destructive move itself, where a verify-then-delete has nothing left to
/// check with. The park closes it: the rename takes whatever is there out of reach, the judgment
/// happens on the parked tree, and bytes this run cannot account for go straight back.
#[test]
fn add_remote_refuses_a_destination_filled_in_the_instant_before_the_park() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/alpha/SKILL.md", b"# alpha\n", 0o644)],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("park-race");
    let project = Scratch::new("park-race-proj");
    let dest = project.0.join(".claude/skills/alpha");
    // An EMPTY destination passes the up-front check — it is legitimately ours to fill.
    std::fs::create_dir_all(&dest).unwrap();
    // The RACE: the hook fires immediately before the rename that parks `dest`, i.e. after every
    // check this run will ever make.
    let racer = crate::fs_seam::HookFs::before_first_move_of(&dest, || {
        std::fs::write(dest.join("SOMEONE-ELSES.md"), b"not topos's bytes\n").unwrap();
    });
    let ctx = Ctx {
        fs: &racer,
        ..h.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&ctx, &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "PLACEMENT_OCCUPIED", "{err:?}");
    // The witness: the parked tree was READ, judged foreign, and put back whole.
    assert_eq!(
        std::fs::read(dest.join("SOMEONE-ELSES.md")).unwrap(),
        b"not topos's bytes\n"
    );
    assert!(!dest.join("SKILL.md").exists(), "the import did not land");
    let siblings: Vec<String> = std::fs::read_dir(project.0.join(".claude/skills"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        siblings,
        vec!["alpha".to_owned()],
        "no park and no staging sibling is left behind"
    );
}

/// P1: the import's staged tree must never be re-resolved through the stage's mutable PATHNAME.
/// The stage is swapped for an outward symlink AFTER its creation — immediately before the walk
/// that creates a nested file-parent, BEFORE that file's staged write — where a path-based base
/// re-open would follow the link and land archive bytes outside the proven root. The
/// fd-descended walk keeps every create inside the directory object this run built, and the
/// staged write's identity check refuses the import: typed refusal, zero bytes outside.
#[test]
fn add_remote_a_stage_swapped_for_a_symlink_mid_write_cannot_aim_bytes_outside() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n", 0o644),
            ("skills/alpha/reference/notes.md", b"notes\n", 0o644),
        ],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("stage-symlink");
    let project = Scratch::new("stage-symlink-proj");
    let victim = Scratch::new("stage-symlink-victim");
    let skills_root = project.0.join(".claude/skills");
    let stage = skills_root.join(".topos-import-alpha");
    let stolen = skills_root.join("stolen-stage");
    let (st, sto, vic) = (stage.clone(), stolen.clone(), victim.0.clone());
    let racer =
        crate::fs_seam::HookFs::before_nth_create_dir_all(&stage.join("reference"), 1, move || {
            std::fs::rename(&st, &sto).unwrap();
            std::os::unix::fs::symlink(&vic, &st).unwrap();
        });
    let ctx = Ctx {
        fs: &racer,
        ..h.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&ctx, &git, &spec, &roots, &opts);
    assert!(err.is_err(), "the swapped stage must refuse the import");
    assert_eq!(
        std::fs::read_dir(&victim.0).unwrap().count(),
        0,
        "zero bytes landed outside — the symlink was never followed"
    );
    assert!(
        !skills_root.join("alpha").exists(),
        "the import did not land"
    );
}

/// P1: the landing rename verifies the STAGE leaf still names the tree this run built. The
/// completed stage is moved aside and a substitute skill dir put at its predictable name
/// immediately BEFORE the landing rename (inside the op, after the before-move beat): typed
/// refusal, the prior occupant is restored, the substitute never lands, nothing is adopted.
#[test]
fn add_remote_refuses_a_stage_substituted_at_its_leaf_before_the_landing_rename() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/alpha/SKILL.md", b"# alpha\n", 0o644)],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("stage-substitute");
    let project = Scratch::new("stage-substitute-proj");
    let skills_root = project.0.join(".claude/skills");
    let dest = skills_root.join("alpha");
    // An EMPTY destination passes the up-front check and is PARKED before the landing — what a
    // refused landing must put back.
    std::fs::create_dir_all(&dest).unwrap();
    let stage = skills_root.join(".topos-import-alpha");
    let stolen = skills_root.join("stolen-stage");
    let (st, sto) = (stage.clone(), stolen.clone());
    let racer = crate::fs_seam::HookFs::before_first_move_of(&stage, move || {
        std::fs::rename(&st, &sto).unwrap();
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join("SKILL.md"), b"# hijacked\n").unwrap();
    });
    let ctx = Ctx {
        fs: &racer,
        ..h.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&ctx, &git, &spec, &roots, &opts);
    assert!(err.is_err(), "a substituted stage must refuse the landing");
    // The prior (empty) occupant is back where it was; the substitute's bytes never landed.
    assert!(dest.is_dir(), "the parked destination was restored");
    assert!(
        !dest.join("SKILL.md").exists(),
        "the substitute never lands"
    );
    // Nothing was adopted: no digest recorded, no tracked skill minted.
    let sidecar_skills = h.home.0.join("skills");
    let tracked = std::fs::read_dir(&sidecar_skills)
        .map(|rd| rd.count())
        .unwrap_or(0);
    assert_eq!(tracked, 0, "no skill was adopted from a substituted stage");
}

/// A FAILED adopt after the landing rename must not delete the destination blind: an edit landing
/// the instant the rename completes is a byte the cleanup would destroy. The error path now PARKS
/// the destination (journaled), drops it only when it still holds exactly what this run staged,
/// and recovery restores anything preserved back to the destination — visible, never lost.
#[test]
fn a_failed_adopt_preserves_an_edit_that_landed_after_the_rename() {
    // The reserved name `topos` makes `add_with_name` fail AFTER the staged tree was renamed
    // into the harness dir — the exact window the finding names.
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/topos/SKILL.md", b"# imported\n", 0o644)],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("failed-adopt");
    let project = Scratch::new("failed-adopt-proj");
    let dest = project.0.join(".claude/skills/topos");
    // The RACE: the edit lands between the rename and the adopt's failure (the adopt's own
    // create of the home dir is the first seam op after the rename).
    let racer = crate::fs_seam::HookFs::before_nth_create_dir_all(&h.home.0, 2, || {
        std::fs::write(dest.join("EXTRA.md"), b"# landed after the rename\n").unwrap();
    });
    let ctx = Ctx {
        fs: &racer,
        ..h.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("topos".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&ctx, &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err:?}");
    // The destination was PARKED, not deleted: the raced edit sits whole under the park name,
    // with its journal entry standing.
    let park = project.0.join(".claude/skills/.topos-import-failed-topos");
    assert_eq!(
        std::fs::read(park.join("EXTRA.md")).unwrap(),
        b"# landed after the rename\n"
    );
    assert!(!dest.exists(), "the destination name was left free");
    // Recovery reads the journal and RESTORES the park to the destination — the edit surfaces
    // where the person put it (an untracked dir), never invisibly.
    crate::sidecar::recover(&RealFs, &Layout::new(&h.home.0), 1, &mut Vec::new()).unwrap();
    assert_eq!(
        std::fs::read(dest.join("EXTRA.md")).unwrap(),
        b"# landed after the rename\n"
    );
    assert!(!park.exists(), "the restored park left its name behind");
}

/// P2: THE PRE-LANDING PARK SURVIVES UNTIL THE WHOLE ADOPT CONCLUDES — the concurrent identical
/// import, end to end. The hook lands at the exact boundary the finding names: after this run's
/// destination check, before its staging — where a COMPETING identical import runs to completion
/// (lands + adopts the same skill). This run then parks the winner's landed copy, judges it
/// byte-identical (DROPPABLE — but not dropped), lands its own stage, and fails the adopt with
/// `ALREADY_TRACKED`. Restore-on-failure puts the winner's copy back: the destination holds the
/// bytes, the winner's map points at a directory that EXISTS, and no park or journal entry is
/// left behind. (Before the fix, the park was deleted + settled at the judgment, and the
/// failed-adopt cleanup then removed the winner's replacement too — an absent dir under a live
/// map row.)
#[test]
fn a_losing_concurrent_identical_import_leaves_the_winner_whole_and_consistent() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/alpha/SKILL.md", b"# alpha\n", 0o644)],
    );
    let git = FakeGit(targz.clone());
    let spec = github_spec("o", "r", None);
    let h = Harness::new("concurrent-import");
    let project = Scratch::new("concurrent-import-proj");
    let dest = project.0.join(".claude/skills/alpha");
    let stage = project.0.join(".claude/skills/.topos-import-alpha");
    let winner_id = std::cell::RefCell::new(None::<String>);
    let racer_git = FakeGit(targz);
    // The first probe of the STAGE name is the first seam op after the destination check — the
    // competing import runs whole in that beat (over the real fs, so nothing re-fires).
    let racer = crate::fs_seam::HookFs::before_nth_exists(&stage, 1, || {
        let roots = ops::DiscoveryRoots {
            home: h.home.0.clone(),
            cwd: Some(project.0.clone()),
        };
        let opts = ops::AddRemoteOpts {
            skill: Some("alpha".into()),
            harness: None,
            dest_root: None,
            global: false,
        };
        let data = ops::add_remote(
            &h.ctx(),
            &racer_git,
            &github_spec("o", "r", None),
            &roots,
            &opts,
        )
        .expect("the concurrent import lands first and wins");
        *winner_id.borrow_mut() = data.skill_id.clone();
    });
    let ctx = Ctx {
        fs: &racer,
        ..h.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&ctx, &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED", "{err:?}");
    // The winner's landed copy is BACK at the destination, byte-exact…
    assert_eq!(std::fs::read(dest.join("SKILL.md")).unwrap(), b"# alpha\n");
    // …and the winner's map points only at directories that EXIST.
    let id = sid(&winner_id.borrow().clone().expect("the winner adopted"));
    let map = doc::read_map(&RealFs, &Layout::new(&h.home.0).published(&id).map)
        .unwrap()
        .expect("the winner's map survives");
    assert!(!map.placements.is_empty());
    for p in &map.placements {
        assert!(
            Path::new(p).is_dir(),
            "the winner's map points at an absent dir: {p}"
        );
    }
    // Both parks concluded (dropped or restored): no sibling left, no journal entry left.
    let siblings: Vec<String> = std::fs::read_dir(project.0.join(".claude/skills"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(siblings, vec!["alpha".to_owned()], "{siblings:?}");
    let journal: Option<crate::sidecar::ParkJournal> =
        doc::read_doc(&RealFs, &Layout::new(&h.home.0).park_journal_path()).unwrap();
    assert!(
        journal.as_ref().is_none_or(|j| j.parks.is_empty()),
        "{journal:?}"
    );
}

/// P2, the success half: a destination parked as DISPOSABLE (here: an empty dir) is not dropped
/// at the judgment — it is still on disk, journaled, while the adopt runs (witnessed at the
/// first seam op after the landing rename) — and only the SUCCESSFUL conclusion settles it.
#[test]
fn a_droppable_pre_landing_park_survives_the_adopt_and_settles_on_success() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/alpha/SKILL.md", b"# alpha\n", 0o644)],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("park-settle");
    let project = Scratch::new("park-settle-proj");
    let dest = project.0.join(".claude/skills/alpha");
    std::fs::create_dir_all(&dest).unwrap(); // empty: passes the check, gets parked droppable
    let park = project.0.join(".claude/skills/.topos-import-old-alpha");
    let journal_path = Layout::new(&h.home.0).park_journal_path();
    let park_alive_mid_adopt = std::cell::Cell::new(false);
    let journaled_mid_adopt = std::cell::Cell::new(false);
    // The adopt's own create of the home dir is the first seam op after the landing rename —
    // the same boundary the failed-adopt test pins.
    let fs = crate::fs_seam::HookFs::before_nth_create_dir_all(&h.home.0, 2, || {
        park_alive_mid_adopt.set(park.exists());
        let j: Option<crate::sidecar::ParkJournal> = doc::read_doc(&RealFs, &journal_path).unwrap();
        journaled_mid_adopt
            .set(j.is_some_and(|j| j.parks.iter().any(|e| Path::new(&e.park) == park.as_path())));
    });
    let ctx = Ctx { fs: &fs, ..h.ctx() };
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    ops::add_remote(&ctx, &git, &spec, &roots, &opts).expect("the import lands");
    assert!(
        park_alive_mid_adopt.get(),
        "the droppable park must still be on disk while the adopt runs"
    );
    assert!(
        journaled_mid_adopt.get(),
        "…and journaled, so a crash mid-adopt still restores it"
    );
    // The successful conclusion settled it: park gone, journal clean, the import landed.
    assert!(!park.exists());
    assert_eq!(std::fs::read(dest.join("SKILL.md")).unwrap(), b"# alpha\n");
    let journal: Option<crate::sidecar::ParkJournal> =
        doc::read_doc(&RealFs, &journal_path).unwrap();
    assert!(
        journal.as_ref().is_none_or(|j| j.parks.is_empty()),
        "{journal:?}"
    );
}

/// The benign half of the same cleanup: a destination still holding EXACTLY what this run staged
/// drops silently — no park, no journal residue, no litter.
#[test]
fn a_failed_adopt_with_an_untouched_destination_cleans_without_residue() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/topos/SKILL.md", b"# imported\n", 0o644)],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("failed-adopt-clean");
    let project = Scratch::new("failed-adopt-clean-proj");
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("topos".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&h.ctx(), &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err:?}");
    let skills = project.0.join(".claude/skills");
    let names: Vec<String> = std::fs::read_dir(&skills)
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(names.is_empty(), "no park, no litter: {names:?}");
    // Nothing left standing in the journal either (the entry settled with the drop).
    let journal: Option<crate::sidecar::ParkJournal> =
        crate::doc::read_doc(&RealFs, &Layout::new(&h.home.0).park_journal_path()).unwrap();
    assert!(
        journal.map(|j| j.parks.is_empty()).unwrap_or(true),
        "the journal settled"
    );
}

/// The import-destination containment rail (project scope): a pre-existing `.claude` symlink out
/// of the checkout aims the registry path exactly like a committed `path = "../.."` — the
/// import refuses at the write boundary, before any byte lands through the link.
#[test]
fn add_remote_refuses_a_project_destination_that_escapes_the_checkout() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[("skills/alpha/SKILL.md", b"# alpha\n", 0o644)],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("escape-import");
    let project = Scratch::new("escape-import-proj");
    let outside = Scratch::new("escape-import-outside");
    std::os::unix::fs::symlink(&outside.0, project.0.join(".claude")).unwrap();
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: Some("alpha".into()),
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&h.ctx(), &git, &spec, &roots, &opts).unwrap_err();
    assert!(
        err.to_string()
            .contains("does not resolve inside this checkout (the import destination)"),
        "{err}"
    );
    assert!(
        !outside.0.join("skills").exists(),
        "nothing was created through the symlink"
    );
}

#[test]
fn add_remote_ambiguous_multi_skill_repo_lists_choices() {
    let targz = build_repo_tarball(
        "o-r-abc1234",
        &[
            ("skills/alpha/SKILL.md", b"a", 0o644),
            ("skills/beta/SKILL.md", b"b", 0o644),
        ],
    );
    let git = FakeGit(targz);
    let spec = github_spec("o", "r", None);
    let h = Harness::new("remote-ambig");
    let project = Scratch::new("remote-ambig-proj");
    let roots = ops::DiscoveryRoots {
        home: h.home.0.clone(),
        cwd: Some(project.0.clone()),
    };
    let opts = ops::AddRemoteOpts {
        skill: None,
        harness: None,
        dest_root: None,
        global: false,
    };
    let err = ops::add_remote(&h.ctx(), &git, &spec, &roots, &opts).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_SKILL");
    // Nothing was written — a failed selection leaves the project tree clean.
    assert!(!project.0.join(".claude/skills/alpha").exists());
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
    assert_eq!(
        a1.skill_id.as_deref(),
        Some("topos_t00"),
        "deterministic injected id"
    );
    assert!(a1.tracked);
    assert_ne!(
        a1.version_id, a1.bundle_digest,
        "version_id and bundle_digest are distinct"
    );

    // The committed add golden equals the real output.
    assert_golden("add.ok", "add", serde_json::to_value(&a1).unwrap());
}

/// The whole chooser as a TTY prints it: the statement, then its lines — the two halves the
/// composition root writes to stderr back to back.
fn chooser_tty(err: &crate::error::ClientError, argv: &[&str]) -> String {
    let argv: Vec<String> = argv.iter().map(|a| (*a).to_owned()).collect();
    let mut out = crate::render::err_tty(err);
    if let Some(lines) = crate::render::err_hint_tty("add", &argv, err) {
        out.push('\n');
        out.push_str(&lines);
    }
    out
}

/// A chooser over the given candidate spellings, in the order a surface prints them.
fn chooser_err(name: &str, global: bool, candidates: &[&str]) -> crate::error::ClientError {
    crate::error::ClientError::AmbiguousSource {
        name: name.to_owned(),
        token: name.to_owned(),
        verb: "add".to_owned(),
        candidates: candidates
            .iter()
            .map(|c| crate::error::TargetCandidate::plain(*c))
            .collect(),
        global,
    }
}

#[test]
fn the_bare_name_chooser_matches_its_committed_envelope() {
    // ONE shape for every add ambiguity, whatever mix produced it: the spellings ride
    // `data.candidates` AND one runnable command each, so an agent picks a line instead of
    // re-reading a sentence.
    assert_golden_err(
        "add.ambiguous",
        "add",
        &chooser_err(
            "code-review",
            false,
            &[
                "topos.sh/acme/code-review",
                "~/.claude/skills/code-review",
                "~/.codex/skills/code-review",
            ],
        ),
    );
}

#[test]
fn the_chooser_prints_its_statement_then_one_runnable_line_per_candidate() {
    // FINAL copy: no `error:` prefix (this is an answer, not a failure), no trailing hint, and
    // every line is a command that can be pasted as it stands — `~/` unquoted, so the shell
    // expands it.
    let err = chooser_err(
        "frontend-design",
        false,
        &[
            "topos.sh/ideamotive/frontend-design",
            "~/.agents/skills/frontend-design",
            "~/.claude/skills/frontend-design",
            "~/.codex/skills/frontend-design",
        ],
    );
    assert_eq!(
        chooser_tty(&err, &["add", "frontend-design"]),
        "frontend-design is ambiguous, pick one:\n  \
         topos add topos.sh/ideamotive/frontend-design\n  \
         topos add ~/.agents/skills/frontend-design\n  \
         topos add ~/.claude/skills/frontend-design\n  \
         topos add ~/.codex/skills/frontend-design"
    );
}

#[test]
fn a_chooser_line_is_the_whole_invocation_with_only_the_target_swapped() {
    // AN OFFERED COMMAND IS THE WHOLE REQUEST. Keeping only the verb and the candidate turned
    // `-a cursor` into an install for every agent and `--propose` into a direct publish — an act
    // nobody asked for. Every other token the person typed rides through exactly as typed.
    let err = chooser_err(
        "code-review",
        false,
        &["topos.sh/acme/code-review", "topos.sh/beta/code-review"],
    );
    let argv = ["add", "code-review", "-a", "cursor", "--json"].map(str::to_owned);
    let envelope = crate::render::err_envelope("add", &argv, &err);
    assert_eq!(
        envelope
            .next_actions
            .iter()
            .map(|a| a.argv.join(" "))
            .collect::<Vec<_>>(),
        vec![
            "topos add topos.sh/acme/code-review -a cursor --json".to_owned(),
            "topos add topos.sh/beta/code-review -a cursor --json".to_owned(),
        ],
        "the selector survives: {:?}",
        envelope.next_actions
    );

    // `--yes` is the ONE token dropped: consent was given to a target now known to be ambiguous,
    // and a candidate is a source the caller has not seen described.
    let argv = ["publish", "code-review", "--propose", "--yes"].map(str::to_owned);
    let envelope = crate::render::err_envelope("publish", &argv, &err);
    assert_eq!(
        envelope.next_actions[0].argv.join(" "),
        "topos publish topos.sh/acme/code-review --propose --json"
    );

    // An invocation whose target token is nowhere in the argv falls back to the plain spelling
    // the error carries for exactly that case.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.next_actions[0].argv.join(" "),
        "topos add topos.sh/acme/code-review --json"
    );
}

#[test]
fn a_folder_candidate_is_absolute_in_argv_and_abbreviated_only_when_printed() {
    // An agent EXECS `argv`. A `~/…` token there is a shell's to expand, and this binary resolves
    // a source path itself — so the offered command would have refused as a missing source.
    let candidate =
        crate::error::TargetCandidate::folder("/home/ada/.claude/skills/x", "~/.claude/skills/x");
    let err = crate::error::ClientError::AmbiguousSource {
        name: "x".to_owned(),
        token: "x".to_owned(),
        verb: "add".to_owned(),
        candidates: vec![candidate],
        global: false,
    };
    let argv = ["add", "x"].map(str::to_owned);
    let envelope = crate::render::err_envelope("add", &argv, &err);
    assert_eq!(
        envelope.next_actions[0].argv,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "/home/ada/.claude/skills/x".to_owned(),
            "--json".to_owned(),
        ],
        "argv stays absolute"
    );
    assert_eq!(
        envelope.data["candidates"],
        serde_json::json!(["/home/ada/.claude/skills/x"]),
        "the machine-readable half is the argv form too"
    );
    // The PRINTED line reads shorter, and a shell expands it back to exactly that argv. ONE
    // folder is not a choice: it keeps the plain chooser line rather than becoming a listing.
    assert_eq!(
        chooser_tty(&err, &["add", "x"]),
        "x is ambiguous, pick one:\n  topos add ~/.claude/skills/x"
    );
}

#[test]
fn a_bare_add_whose_name_is_in_several_folders_lists_them() {
    // THE FOLDER LISTING for a name NOTHING here answers to: every folder, one per line, bare —
    // no bundle resolves under the name, so there is no relation to prove and no `--as` to spell.
    let folder = |dir: &str| {
        crate::error::TargetCandidate::folder(format!("/home/ada/{dir}"), format!("~/{dir}"))
    };
    let err = crate::error::ClientError::AmbiguousSource {
        name: "coolify-deploy".to_owned(),
        token: "coolify-deploy".to_owned(),
        verb: "add".to_owned(),
        candidates: vec![
            folder(".agents/skills/coolify-deploy"),
            folder(".claude/skills/coolify-deploy"),
        ],
        global: false,
    };
    assert_eq!(
        chooser_tty(&err, &["add", "coolify-deploy"]),
        "'coolify-deploy' is in 2 folders here:\n  ~/.agents/skills/coolify-deploy\n  \
         ~/.claude/skills/coolify-deploy\nname the one to adopt: topos add <folder>"
    );
    // The agent surface is unchanged: every folder still rides one runnable command, because
    // nothing here is a guess about bytes — the person named the folder either way.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.next_actions.len(),
        2,
        "{:?}",
        envelope.next_actions
    );
    // A WORKSPACE spelling among the candidates is a different question — two bundles, not two
    // copies of one — so that answer keeps the plain chooser.
    let mixed = crate::error::ClientError::AmbiguousSource {
        name: "coolify-deploy".to_owned(),
        token: "coolify-deploy".to_owned(),
        verb: "add".to_owned(),
        candidates: vec![
            crate::error::TargetCandidate::plain("topos.sh/acme/coolify-deploy"),
            folder(".claude/skills/coolify-deploy"),
        ],
        global: false,
    };
    assert!(
        crate::render::err_tty(&mixed).starts_with("coolify-deploy is ambiguous, pick one:"),
        "{}",
        crate::render::err_tty(&mixed)
    );
    // And so does another verb's chooser: `publish` is not asking anyone to adopt anything.
    let publishing = crate::error::ClientError::AmbiguousSource {
        name: "coolify-deploy".to_owned(),
        token: "coolify-deploy".to_owned(),
        verb: "publish".to_owned(),
        candidates: vec![
            folder(".agents/skills/coolify-deploy"),
            folder(".claude/skills/coolify-deploy"),
        ],
        global: false,
    };
    assert!(
        crate::render::err_tty(&publishing).starts_with("coolify-deploy is ambiguous, pick one:"),
        "{}",
        crate::render::err_tty(&publishing)
    );
}

#[test]
fn the_already_added_answer_reads_like_the_receipt_it_mirrors() {
    // The add receipt's own two lines, in the past tense — no `error:` prefix, no trailing hint.
    let project = crate::error::ClientError::AlreadyAdded {
        name: "coolify-deploy".to_owned(),
        scope: topos_types::results::ReceiptScope::Project,
        from: Some("topos.sh/ideamotive/coolify-deploy".to_owned()),
        candidates: Vec::new(),
    };
    assert_eq!(
        crate::render::err_tty(&project),
        "coolify-deploy is already added to this project (./topos.toml)\nsource: \
         topos.sh/ideamotive/coolify-deploy"
    );
    assert_eq!(project.code(), "ALREADY_TRACKED");
    assert!(crate::render::err_hint_tty("add", &["add".to_owned()], &project).is_none());
    // The machine file has its own one spelling, and a record nothing can name says less rather
    // than something vague.
    let machine = crate::error::ClientError::AlreadyAdded {
        name: "coolify-deploy".to_owned(),
        scope: topos_types::results::ReceiptScope::Machine,
        from: None,
        candidates: Vec::new(),
    };
    assert_eq!(
        crate::render::err_tty(&machine),
        "coolify-deploy is already added machine-wide (~/.topos/topos.toml)"
    );
}

/// The already-added answer lists the folders the name ALSO stands in, and says so: the bundle is
/// already managed in a folder of its own, so a bare "'x' is in 3 folders here" would undercount by
/// exactly the copy the reader already has — these are the unmanaged ones, on offer to adopt too.
#[test]
fn the_already_added_answer_lists_every_folder_the_name_is_in() {
    // THE FOLDER LISTING, byte-exact: every folder on one line with what its bytes are, then the
    // ONE command that adopts whichever the reader picks. `-g` rides that command, and the folders
    // read `~/…` while the argv the agent surface gets stays absolute.
    use crate::error::CopyRelation;
    let copy = |dir: &str, relation| {
        crate::error::TargetCandidate::claim(
            format!("/home/ada/{dir}"),
            format!("~/{dir}"),
            "topos.sh/ideamotive/coolify-deploy",
            "topos.sh/ideamotive/coolify-deploy",
        )
        .with_relation(relation)
    };
    let err = crate::error::ClientError::AlreadyAdded {
        name: "coolify-deploy".to_owned(),
        scope: topos_types::results::ReceiptScope::Machine,
        from: Some("topos.sh/ideamotive/coolify-deploy".to_owned()),
        candidates: vec![
            copy(".agents/skills/coolify-deploy", Some(CopyRelation::Edited)),
            copy(
                ".claude/skills/coolify-deploy",
                Some(CopyRelation::EditedDifferently),
            ),
            copy(".codex/skills/coolify-deploy", Some(CopyRelation::Current)),
        ],
    };
    assert_eq!(
        crate::render::err_tty(&err),
        "coolify-deploy is already added machine-wide (~/.topos/topos.toml)\nsource: \
         topos.sh/ideamotive/coolify-deploy\n'coolify-deploy' is also in 3 unmanaged folders \
         here:\n  ~/.agents/skills/coolify-deploy — edited (adopting it makes these your \
         draft)\n  ~/.claude/skills/coolify-deploy — edited differently\n  \
         ~/.codex/skills/coolify-deploy — matches the published current\nname the one to adopt: \
         topos add -g <folder> --as topos.sh/ideamotive/coolify-deploy"
    );
    // The listing IS the answer and it ends in its own command — nothing goes under it. A second
    // list of the same folders spelled as commands is exactly the shape this one replaced.
    assert!(crate::render::err_hint_tty("add", &["add".to_owned()], &err).is_none());
    // The RUNNABLE half is narrower than the listing: a folder whose bytes no version explains is
    // listed (with what adopting it would mean) and never handed to an agent as an argv.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope
            .next_actions
            .iter()
            .map(|a| a.argv.join(" "))
            .collect::<Vec<_>>(),
        vec![
            "topos add -g /home/ada/.codex/skills/coolify-deploy --as \
             topos.sh/ideamotive/coolify-deploy --json"
                .to_owned()
        ],
    );
    // The committed example IS what the agent surface emits: every folder in `data.candidates`,
    // the offerable ones as runnable claims, and the listing in the message.
    assert_golden_err("add.already-added", "add", &err);
    // ONE folder says so in the singular, and a relation nothing proved prints no clause at all.
    let one = crate::error::ClientError::AlreadyAdded {
        name: "coolify-deploy".to_owned(),
        scope: topos_types::results::ReceiptScope::Machine,
        from: None,
        candidates: vec![copy(".codex/skills/coolify-deploy", None)],
    };
    assert_eq!(
        crate::render::err_tty(&one),
        "coolify-deploy is already added machine-wide (~/.topos/topos.toml)\n'coolify-deploy' is \
         also in 1 unmanaged folder here:\n  ~/.codex/skills/coolify-deploy\nname the one to \
         adopt: topos add -g <folder> --as topos.sh/ideamotive/coolify-deploy"
    );
}

#[test]
fn the_server_floor_refusal_matches_its_committed_envelope() {
    // The client half of the version floor: the card named a server older than this build speaks
    // to. The message names both remedies; the one this machine can run rides as an argv, pinned
    // to the server's own release.
    assert_golden_err(
        "login.server-too-old",
        "login",
        &crate::error::ClientError::ServerTooOld {
            server_version: "0.1.9".to_owned(),
        },
    );
}

#[test]
fn a_global_add_refusal_keeps_g_in_every_spelled_follow_up() {
    // `add -g <name>` refused: following a spelled line must land the row machine-wide, not
    // silently in this folder's manifest — every offered command, on both surfaces, carries `-g`.
    let err = chooser_err(
        "code-review",
        true,
        &["topos.sh/acme/code-review", "topos.sh/beta/code-review"],
    );
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.next_actions.len(),
        2,
        "{:?}",
        envelope.next_actions
    );
    for action in &envelope.next_actions {
        assert_eq!(action.argv[2], "-g", "{:?}", action.argv);
    }
    assert_eq!(
        chooser_tty(&err, &["add", "-g", "code-review"]),
        "code-review is ambiguous, pick one:\n  topos add -g topos.sh/acme/code-review\n  topos \
         add -g topos.sh/beta/code-review"
    );
}

#[test]
fn add_pins_the_lock_shape() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("addlist");
    let add = ops::add(&h.ctx(), &root).unwrap();

    // The lock.json on-disk instance shape (sorted files: path, mode, sha256, size + base_commit).
    let lock: Lock = doc::read_doc(
        h.ctx().fs,
        &h.ctx()
            .layout
            .published(&sid(add.skill_id.as_deref().unwrap()))
            .lock,
    )
    .unwrap()
    .unwrap();
    assert_eq!(lock.schema_version, 1);
    assert_eq!(lock.name, "pr-describe");
    assert_eq!(Some(lock.base_commit), add.version_id);
    assert_eq!(Some(lock.bundle_digest), add.bundle_digest);
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
}

/// The committed `list.ok` golden equals the REAL op's output over a deterministic machine
/// scope: one workspace session, one assigned-and-applied skill (fixed version + digest), the
/// feed row login writes on the first connection. Path-free by construction (the manifest
/// renders `~`-relative), so the bytes are stable across machines.
#[test]
fn list_golden_matches_the_real_machine_scope_output() {
    use crate::ops::inventory::testkit::{TempHome, assigned, skill_id_of, with_ctx};
    let home = TempHome::new();
    let cwd = home.0.join("plain");
    std::fs::create_dir_all(&cwd).unwrap();
    home.session(
        "topos.sh",
        "w_acme",
        "acme",
        crate::sessions::SESSION_ACTIVE,
    );
    home.global("[bundles]\n\"topos.sh/acme\" = \"*\"\n");
    home.cache(
        "w_acme",
        "topos.sh",
        "acme",
        vec![assigned("pr-describe", Some("Dana"))],
        Vec::new(),
    );
    home.store_applied(
        &skill_id_of("pr-describe"),
        "pr-describe",
        &"d".repeat(64),
        &[],
    );
    let out = with_ctx(&home, Some(&cwd), |ctx| {
        ops::list_with(
            ctx,
            &ops::ListRequest::default(),
            None,
            None,
            ops::RowPage::unlimited(),
        )
    })
    .unwrap();
    assert_golden("list.ok", "list", serde_json::to_value(&out.data).unwrap());
}

#[test]
fn list_discovers_untracked_registry_skills_then_dedups_an_adopted_one() {
    let h = Harness::new("disc");
    // A temp user-home holding a Cursor skill. Cursor has no env home-override, so the registry resolves
    // its dir under the passed home — hermetic even though the ambient CLAUDE_CONFIG_DIR also resolves real
    // Claude Code skills (hence we assert CONTAINS, never an exact count).
    let user_home = Scratch::new("disc-userhome");
    let skill_dir = user_home.0.join(".cursor").join("skills").join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        b"---\nname: my-skill\ndescription: test\n---\n# My Skill\n",
    )
    .unwrap();

    let roots = || ops::DiscoveryRoots {
        home: user_home.0.clone(),
        cwd: None,
    };
    let untracked_req = || ops::ListRequest {
        untracked: true,
        ..ops::ListRequest::default()
    };
    let data = ops::list_with(
        &h.ctx(),
        &untracked_req(),
        Some(roots()),
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap()
    .data;
    let found = data
        .untracked
        .iter()
        .find(|u| u.readers.iter().any(|s| s == "cursor") && u.name == "my-skill")
        .expect("cursor skill discovered as untracked");
    assert_eq!(found.scope, "user");
    assert!(found.path.contains("my-skill"));
    // The folder is the entry's own, and its readers are the folder's — cursor alone reads
    // `~/.cursor/skills`, so this row's bracket is exactly one slug.
    assert_eq!(
        found.folder,
        skill_dir.parent().unwrap().to_string_lossy().into_owned()
    );
    assert_eq!(found.readers, vec!["cursor".to_owned()]);

    // The bare list itemizes no discoveries — they ride the ONE summary line instead.
    let bare = ops::list_with(
        &h.ctx(),
        &ops::ListRequest::default(),
        Some(roots()),
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap()
    .data;
    assert!(bare.untracked.is_empty());
    assert!(
        bare.untracked_summary
            .is_some_and(|s| s.skills >= 1 && s.command == "topos list --untracked"),
        "the summary points at the expanding command"
    );

    // Adopt it → it drops out of the untracked set (dedup by canonical placement path).
    ops::add(&h.ctx(), &skill_dir).unwrap();
    let after = ops::list_with(
        &h.ctx(),
        &untracked_req(),
        Some(roots()),
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap()
    .data;
    assert!(
        !after
            .untracked
            .iter()
            .any(|u| u.name == "my-skill" && u.path.contains("my-skill")),
        "an adopted skill is no longer reported as untracked"
    );
}

/// Discovery follows a link shell's `SKILL.md` to a regular file and lists it, so the listing has
/// to answer for the ORIGINAL too: once that folder is managed, the shell is a second window onto
/// a tracked skill, not an adoptable one. A shell whose original nobody tracks keeps listing —
/// naming it to `add` is exactly how it gets adopted.
#[test]
fn a_link_shell_stops_listing_once_its_original_is_tracked() {
    let h = Harness::new("shell-list");
    let user_home = Scratch::new("shell-list-userhome");
    let skills = user_home.0.join(".cursor").join("skills");
    let bundle = Scratch::new("shell-list-bundle");

    // Two shells in the agent's folder; their originals live outside any discovered root.
    let mut origins = Vec::new();
    for name in ["managed", "loose"] {
        let origin = bundle.0.join(name);
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("SKILL.md"), format!("# {name}\n")).unwrap();
        let shell = skills.join(name);
        std::fs::create_dir_all(&shell).unwrap();
        std::os::unix::fs::symlink(origin.join("SKILL.md"), shell.join("SKILL.md")).unwrap();
        origins.push(origin);
    }

    let untracked = |names: &Harness| {
        ops::list_with(
            &names.ctx(),
            &ops::ListRequest {
                untracked: true,
                ..ops::ListRequest::default()
            },
            Some(ops::DiscoveryRoots {
                home: user_home.0.clone(),
                cwd: None,
            }),
            None,
            ops::RowPage::unlimited(),
        )
        .unwrap()
        .data
        .untracked
        .iter()
        .map(|u| u.name.clone())
        .collect::<Vec<_>>()
    };

    let before = untracked(&h);
    assert!(before.contains(&"managed".to_owned()), "{before:?}");
    assert!(before.contains(&"loose".to_owned()), "{before:?}");

    ops::add(&h.ctx(), &origins[0]).unwrap();

    let after = untracked(&h);
    assert!(
        !after.contains(&"managed".to_owned()),
        "the shell of a tracked original is not adoptable: {after:?}"
    );
    assert!(
        after.contains(&"loose".to_owned()),
        "a shell nobody tracks is still adoptable: {after:?}"
    );
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
    let mut reported = ops::list_with(
        &h.ctx(),
        &ops::ListRequest {
            footprint: true,
            ..ops::ListRequest::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
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
    let reported = ops::list_with(
        &h.ctx(),
        &ops::ListRequest {
            footprint: true,
            ..ops::ListRequest::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
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
    // THE REASON REACHES THE TERMINAL. A scan reason is a bundle-relative name inside the folder
    // the person themselves named, so redacting it bought nothing and cost the reader the one
    // fact that makes the refusal actionable: WHICH file.
    let text = render::err_tty(&err);
    assert!(text.contains("symlink: link"), "{text}");
    // Nothing tracked.
    assert!(store_records(&h.ctx()).is_empty());
}

/// A nested reject names its file by the path the bundle knows it as, never by a host path.
#[test]
fn a_nested_scan_reject_names_the_bundle_relative_path() {
    let src = Scratch::new("sym-nested");
    let root = src.0.join("skill");
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(root.join("SKILL.md"), b"# s\n").unwrap();
    std::os::unix::fs::symlink("/etc/hosts", root.join("scripts/deploy.sh")).unwrap();

    let h = Harness::new("sym-nested-home");
    let err = ops::add(&h.ctx(), &root).unwrap_err();
    let text = render::err_tty(&err);
    assert!(text.contains("symlink: scripts/deploy.sh"), "{text}");
    assert!(
        !text.contains(src.0.to_str().unwrap()),
        "leaked a host path: {text}"
    );
}

/// The real folder plus a LINK SHELL beside it: `<root>/original/<name>/` holds the bytes, and
/// `<root>/agents/<name>/` is a real directory whose entries are links into it — the shape an
/// agent's skills dir grows when one bundle installs itself once and links each skill.
fn link_shell(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let origin = root.join("original").join(name);
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("SKILL.md"), b"# real\n").unwrap();
    let shell = root.join("agents").join(name);
    std::fs::create_dir_all(&shell).unwrap();
    std::os::unix::fs::symlink(origin.join("SKILL.md"), shell.join("SKILL.md")).unwrap();
    (origin, shell)
}

/// Discovery lists a link shell (its root `SKILL.md` resolves to a regular file) and the scanner
/// rejects every symlink — so the two halves used to disagree: `list` offered a folder `add`
/// refused. `add` now takes the folder the links point INTO, silently, and records THAT: the
/// row's key, the placement, and the receipt's `source:` all name the original.
#[test]
fn add_of_a_link_shell_adopts_the_folder_it_points_at() {
    let src = Scratch::new("shell");
    let (origin, shell) = link_shell(&src.0, "codex");

    let h = Harness::new("shell-home");
    let ctx = h.ctx();
    let mut data = ops::add(&ctx, &shell).unwrap();
    ops::note_added_path(&ctx, &mut data, &shell, true).unwrap();

    let origin_c = origin.canonicalize().unwrap();
    assert_eq!(data.name, "codex");
    assert_eq!(data.source.as_deref(), Some(origin_c.to_str().unwrap()));
    // The manifest row keys the ORIGINAL — a row naming the shell would demand a folder topos
    // does not track.
    let text = std::fs::read_to_string(h.ctx().layout.home().join("topos.toml")).unwrap();
    assert!(
        text.contains(origin_c.to_str().unwrap()),
        "the row names the original: {text}"
    );
    // And so does the placement the record holds.
    let sid = sid(data.skill_id.as_deref().unwrap());
    let map = doc::read_map(&h.fs, &h.ctx().layout.published(&sid).map)
        .unwrap()
        .expect("the adopt wrote a placement map");
    assert_eq!(
        map.placements,
        vec![origin_c.to_string_lossy().into_owned()]
    );
}

/// A shell that ALSO holds files of its own is two folders, and only the person knows which they
/// meant — refused by name, with both spellings on the line.
#[test]
fn add_of_a_mixed_link_shell_refuses_naming_both_folders() {
    let src = Scratch::new("shell-mixed");
    let (origin, shell) = link_shell(&src.0, "codex");
    std::fs::write(shell.join("notes.md"), b"mine\n").unwrap();

    let h = Harness::new("shell-mixed-home");
    let err = ops::add(&h.ctx(), &shell).unwrap_err();
    assert_eq!(err.code(), "SCAN_REJECTED");
    let text = render::err_tty(&err);
    assert!(text.contains("can't add codex"), "{text}");
    assert!(
        text.contains(origin.canonicalize().unwrap().to_str().unwrap()),
        "{text}"
    );
    assert!(text.contains("add one folder by path"), "{text}");
    assert!(store_records(&h.ctx()).is_empty());
}

/// A shell whose link points at nothing says exactly that — never the scanner's generic reject,
/// and never a half-adopted record.
#[test]
fn add_of_a_broken_link_shell_says_the_link_is_broken() {
    let src = Scratch::new("shell-broken");
    let shell = src.0.join("agents").join("codex");
    std::fs::create_dir_all(&shell).unwrap();
    std::os::unix::fs::symlink(src.0.join("gone/SKILL.md"), shell.join("SKILL.md")).unwrap();

    let h = Harness::new("shell-broken-home");
    let err = ops::add(&h.ctx(), &shell).unwrap_err();
    assert_eq!(err.code(), "SCAN_REJECTED");
    assert!(
        render::err_tty(&err).contains("can't add codex — its SKILL.md link is broken"),
        "{}",
        render::err_tty(&err)
    );
    assert!(store_records(&h.ctx()).is_empty());
}

/// The listing's promise is that `topos add <name>` manages one, so a shell it cannot resolve is
/// not listed: a MIXED shell (own files beside the link) is two folders, and `add` refuses it.
/// A healthy shell keeps listing — and says where its bytes really are.
#[test]
fn a_mixed_link_shell_is_not_listed_and_a_healthy_one_names_its_original() {
    let h = Harness::new("shell-listing");
    let user_home = Scratch::new("shell-listing-userhome");
    let skills = user_home.0.join(".cursor").join("skills");
    let bundle = Scratch::new("shell-listing-bundle");

    // Two shells in the agent's folder: one healthy, one holding a file of its own.
    for name in ["healthy", "mixed"] {
        let origin = bundle.0.join(name);
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("SKILL.md"), format!("# {name}\n")).unwrap();
        let shell = skills.join(name);
        std::fs::create_dir_all(&shell).unwrap();
        std::os::unix::fs::symlink(origin.join("SKILL.md"), shell.join("SKILL.md")).unwrap();
    }
    std::fs::write(skills.join("mixed").join("notes.md"), b"mine\n").unwrap();
    // …and a plain folder, which must be unaffected by any of this.
    let plain = skills.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("SKILL.md"), b"# plain\n").unwrap();

    let listed = ops::list_with(
        &h.ctx(),
        &ops::ListRequest {
            untracked: true,
            ..ops::ListRequest::default()
        },
        Some(ops::DiscoveryRoots {
            home: user_home.0.clone(),
            cwd: None,
        }),
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap()
    .data
    .untracked;
    let names: Vec<&str> = listed.iter().map(|u| u.name.as_str()).collect();
    assert!(names.contains(&"healthy"), "{names:?}");
    assert!(names.contains(&"plain"), "{names:?}");
    assert!(
        !names.contains(&"mixed"),
        "a shell `add` refuses is not offered as adoptable: {names:?}"
    );

    // THE SHELL SAYS WHERE ITS BYTES ARE — the folder an `add` of it would really take, absolute
    // on the wire. An ordinary folder is its own origin and carries nothing.
    let healthy = listed.iter().find(|u| u.name == "healthy").unwrap();
    assert_eq!(
        healthy.original.as_deref(),
        Some(
            bundle
                .0
                .join("healthy")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        ),
        "{healthy:?}"
    );
    // …and that is exactly what the WIRE carries: the absolute path on a shell row, the field
    // absent entirely on an ordinary one (a `~` is a shell's to expand, never a machine's).
    let wire = serde_json::to_value(healthy).unwrap();
    assert_eq!(
        wire["original"].as_str(),
        healthy.original.as_deref(),
        "{wire}"
    );
    assert!(
        wire["original"]
            .as_str()
            .is_some_and(|p| Path::new(p).is_absolute()),
        "{wire}"
    );
    let plain_wire =
        serde_json::to_value(listed.iter().find(|u| u.name == "plain").unwrap()).unwrap();
    assert!(plain_wire.get("original").is_none(), "{plain_wire}");
}

/// A shell whose link target is GONE is invisible to every probe (`is_file()` follows the link),
/// so a bare name that matches one used to answer "no untracked skill of that name" about a folder
/// the person is looking at. The folder's own refusal answers instead, naming the folder.
#[test]
fn a_bare_name_answers_for_a_broken_shell_discovery_cannot_see() {
    let h = Harness::new("shell-bare");
    let user_home = Scratch::new("shell-bare-userhome");
    let skills = user_home.0.join(".cursor").join("skills");
    let shell = skills.join("gonelink");
    std::fs::create_dir_all(&shell).unwrap();
    std::os::unix::fs::symlink(user_home.0.join("gone/SKILL.md"), shell.join("SKILL.md")).unwrap();

    fn no_sessions(_s: &crate::sessions::Session) -> ops::SessionTransports {
        unreachable!("no session is enrolled in this rig")
    }
    let roots = ops::DiscoveryRoots {
        home: user_home.0.clone(),
        cwd: None,
    };
    let err = ops::plan_bare_add(
        &h.ctx(),
        &no_sessions,
        &roots,
        "gonelink",
        ops::BareAdd {
            subscribe: true,
            dest_selected: false,
            global: true,
            workspace: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "SCAN_REJECTED");
    let text = render::err_tty(&err);
    assert!(
        text.contains("its SKILL.md link is broken (target missing)"),
        "{text}"
    );
    assert!(
        text.contains(shell.to_str().unwrap()),
        "the answer names the folder, not the bare word: {text}"
    );
}

/// A DIRECTORY-level symlink is a different shape and keeps its own (unchanged) answer: the
/// canonicalize resolves it, and the folder behind it is what gets adopted.
#[test]
fn add_through_a_directory_symlink_adopts_the_folder_behind_it() {
    let src = Scratch::new("shell-dir");
    let origin = src.0.join("original").join("codex");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::write(origin.join("SKILL.md"), b"# real\n").unwrap();
    let link = src.0.join("codex-link");
    std::os::unix::fs::symlink(&origin, &link).unwrap();

    let h = Harness::new("shell-dir-home");
    let data = ops::add(&h.ctx(), &link).unwrap();
    let sid = sid(data.skill_id.as_deref().unwrap());
    let map = doc::read_map(&h.fs, &h.ctx().layout.published(&sid).map)
        .unwrap()
        .expect("the adopt wrote a placement map");
    assert_eq!(
        map.placements,
        vec![
            origin
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        ]
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
        assert!(store_records(&h.ctx()).is_empty());
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
    let env = render::err_envelope("list", &["list".to_owned()], &amb);
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
    let env = render::err_envelope("list", &["list".to_owned()], &corrupt);
    let msg = env.error.unwrap().context["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!msg.contains("xyzzy"), "safe_message leaked: {msg}");
    assert!(!render::err_tty(&corrupt).contains("xyzzy"));

    // A store-side IO failure is retryable, like a client-side one.
    let io = ClientError::Gitstore(topos_gitstore::GitstoreError::Io("disk full".into()));
    assert!(
        render::err_envelope("add", &["add".to_owned()], &io)
            .error
            .unwrap()
            .retryable
    );
}

/// A SOURCE THAT IS NOT THERE IS PERMANENT, and says so in one sentence. It used to surface as
/// the adopt's generic canonicalize failure — `IO_ERROR`, `retryable: true`, "this is often
/// transient — running it again is safe" — which is advice to loop forever on a path that will be
/// missing every time. And it landed AFTER the store home had been created.
#[test]
fn adding_a_folder_that_is_not_there_is_a_permanent_refusal() {
    let h = Harness::new("no-such-source");
    let missing = h.home.0.join("nope/deploy");
    let err = ops::add(&h.ctx(), &missing).expect_err("nothing to adopt");
    assert_eq!(err.code(), "SOURCE_MISSING");
    assert_eq!(
        err.outcome(),
        topos_types::TerminalOutcome::PermanentFailure
    );
    let env = render::err_envelope("add", &["add".to_owned()], &err);
    let e = env.error.as_ref().unwrap();
    assert!(!e.retryable, "{e:?}");
    assert_eq!(
        e.context["message"].as_str().unwrap(),
        format!("{} does not exist — nothing was added", missing.display())
    );
    // The TTY says the same sentence, and never invites the retry.
    let tty = render::err_tty(&err);
    assert_eq!(
        tty,
        format!(
            "error: {} does not exist — nothing was added",
            missing.display()
        )
    );
    assert_eq!(
        render::err_hint_tty("add", &["add".to_owned()], &err),
        None,
        "nothing to try: the path is the person's to fix"
    );
}

#[test]
fn list_deep_dive_miss_answers_not_managed() {
    let h = Harness::new("ambig");
    // A token nothing delivers is a SUCCESS carrying the not-managed headline (no error, no
    // store mention) — "nothing manages it" is the whole answer.
    let src_a = editable_source();
    ops::add(&h.ctx(), &src_a.0.join("pr-describe")).unwrap();

    let deep = |name: &str| {
        ops::list_with(
            &h.ctx(),
            &ops::ListRequest {
                name: Some(name.to_owned()),
                ..ops::ListRequest::default()
            },
            None,
            None,
            ops::RowPage::unlimited(),
        )
    };
    let detail = deep("nope").unwrap().data.detail.expect("a detail");
    assert!(!detail.managed, "{detail:?}");
    assert!(detail.folders.is_empty(), "{detail:?}");
}

#[test]
fn diff_is_empty_when_clean_and_a_golden_when_edited() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("diff");
    ops::add(&h.ctx(), &root).unwrap();

    // Clean: an unmodified added skill has an empty diff.
    let clean = ops::diff(
        &h.ctx(),
        "pr-describe",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
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
    let edited = ops::diff(
        &h.ctx(),
        "pr-describe",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
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

    let edited = ops::diff(
        &h.ctx(),
        "pr-describe",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();

    // The reported digest equals an independent scan of the on-disk draft (adopt-in-place → `root`)…
    let draft_digest = topos_core::digest::to_hex(&crate::scan::scan(&root).unwrap().bundle_digest);
    assert_eq!(
        edited.bundle_digest, draft_digest,
        "diff reports the draft's byte-exact digest"
    );
    // …and differs from the base/current version's digest (the value publish would have rejected).
    assert_ne!(
        Some(edited.bundle_digest.clone()),
        add.bundle_digest,
        "the draft digest is not the base digest"
    );
    // The diffed endpoint (version_id) stays the base commit — only the consent digest moved.
    assert_eq!(Some(edited.version_id), add.version_id);
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
        progress: crate::progress::silent(),
        fs: &probe_fs,
        ids: &probe_ids,
        clock: &probe_clock,
        device_id: DEVICE_ID.to_owned(),
        layout: Layout::new(&probe_home.0),
        harness: &no_harness,
        triggers: crate::ops::Triggers::active_only(&no_harness),
        plane: &no_plane,
        follow: &no_follow,
        roots: None,
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
            progress: crate::progress::silent(),
            fs: &fs,
            ids: &ids,
            clock: &clock,
            device_id: DEVICE_ID.to_owned(),
            layout: layout.clone(),
            harness: &no_harness,
            triggers: crate::ops::Triggers::active_only(&no_harness),
            plane: &no_plane,
            follow: &no_follow,
            roots: None,
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
        crate::sidecar::recover(&real, &layout, 0, &mut Vec::new()).unwrap();
        let clean_ids = SeqIds::new("t");
        let clean_ctx = Ctx {
            progress: crate::progress::silent(),
            fs: &real,
            ids: &clean_ids,
            clock: &clock,
            device_id: DEVICE_ID.to_owned(),
            layout: layout.clone(),
            harness: &no_harness,
            triggers: crate::ops::Triggers::active_only(&no_harness),
            plane: &no_plane,
            follow: &no_follow,
            roots: None,
        };
        let tracked = store_records(&clean_ctx);

        // All-or-nothing: the staging-rename is the commit point, so the skill is either absent (fault
        // before the publish) or COMPLETE (fault at/after it) — never a half/corrupt state. A faulted
        // post-publish step (a final fsync or the log append) still leaves a fully usable skill.
        assert!(
            tracked.len() <= 1,
            "fail_at={fail_at}: at most one skill, found {tracked:?}"
        );
        if let Some((_, version)) = tracked.first() {
            assert_eq!(
                version,
                "d77b648d8149d63189864c6b6d06da4f7919935c4242cc197e708b1dafe941d5"
            );
            // Complete + usable: it renders + diffs without an integrity error.
            ops::diff(
                &clean_ctx,
                "pr-describe",
                None,
                ops::DiffBudget::unlimited(),
                &ops::Selection::default(),
                ops::StoreScope::Here,
            )
            .unwrap_or_else(|e| panic!("fail_at={fail_at}: tracked skill must be usable: {e:?}"));
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
        crate::sidecar::recover(&real, &layout, 0, &mut Vec::new()).unwrap();
        assert_eq!(
            before_fp,
            crate::sidecar::footprint(&real, &layout).unwrap()
        );
    }
}

/// Claude Code's config home under a scratch USER home — the root [`claude_trigger`] arms, stated
/// here rather than resolved: the registry's resolver reads `$CLAUDE_CONFIG_DIR`, so a rig that
/// asked it would arm the DEVELOPER's own settings whenever that variable happens to be set,
/// whatever scratch home it passed. Naming the root leaves the environment nothing to redirect.
fn claude_home(user_home: &Path) -> PathBuf {
    user_home.join(".claude")
}

/// Claude Code's TRIGGER port over that root, through the crate's injection-explicit constructor —
/// the same spec and machinery production arms, with the ONE env-sensitive step (resolving the
/// root) supplied by the caller instead.
fn claude_trigger<'a>(
    user_home: &Path,
    cfg: &'a dyn topos_harness::ConfigStore,
) -> Box<dyn TriggerAdapter + 'a> {
    topos_harness::triggers::claude_code_at(claude_home(user_home), cfg)
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
    let user = Scratch::new("cc-home");
    let claude = claude_home(&user.0);
    // Frontmatter `name` deliberately differs from the dir name — the DIRECTORY name (the command name)
    // must win for a recognized Claude Code skill.
    let skill = claude_skill(
        &claude,
        "pr-describe",
        "---\nname: not-the-command-name\n---\n\n# PR describe\n\nWrite a clear PR description.\n",
    );
    let before = fs_hashes(&skill);

    let cfg = RealFs;
    let cc = ClaudeCode::new(claude.clone());
    let trigger = claude_trigger(&user.0, &cfg);
    let ctx = h.ctx_with(&cc, trigger.as_ref());

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
    let settings = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert!(
        settings.contains("topos update --quiet --hook claude-code"),
        "hook command installed, carrying the dialect marker that opts Claude Code into the \
         reload extension"
    );
    assert!(settings.contains("# topos:currency"), "sentinel present");

    // The placement was recorded with the harness tag; the store carries the one record.
    let tracked = store_records(&ctx);
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0].0, "pr-describe");
}

#[test]
fn add_of_a_plain_dir_tags_no_harness_and_installs_no_hook() {
    let h = Harness::new("cc-plain");
    let user = Scratch::new("cc-empty"); // a real (empty) Claude home — the source is NOT under it
    let claude = claude_home(&user.0);
    let src = editable_source();
    let cfg = RealFs;
    let cc = ClaudeCode::new(claude.clone());
    let trigger = claude_trigger(&user.0, &cfg);
    let ctx = h.ctx_with(&cc, trigger.as_ref());

    let add = ops::add(&ctx, &src.0.join("pr-describe")).unwrap();
    assert!(
        add.harness.is_none(),
        "a plain dir is not a recognized harness skill"
    );
    assert!(add.currency.is_none(), "no currency armed for a plain dir");
    assert!(
        !claude.join("settings.json").exists(),
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
    let crate::error::ClientError::AlreadyTracked { name, dir, claim } = &err else {
        panic!("re-adding the same dir must be refused, got {err:?}");
    };
    // The refusal speaks in what a person can act on: the skill's own NAME and the folder.
    assert_eq!(name, "pr-describe");
    assert_eq!(*dir, root.canonicalize().unwrap().display().to_string());
    // No manifest row claims it here (this rig writes none), so the claim is honestly absent
    // and the sentence stops at the folder rather than naming a file that says nothing.
    assert!(claim.is_none(), "{claim:?}");
    let message = crate::render::safe_message(&err);
    assert!(
        !message.contains("topos_"),
        "the internal store id never reaches a user surface: {message}"
    );
    assert!(
        message.contains("'pr-describe' already tracks"),
        "{message}"
    );
    assert!(message.contains("topos remove pr-describe"), "{message}");
    assert_eq!(
        store_records(&h.ctx()).len(),
        1,
        "no second record was minted"
    );
}

/// The same refusal WITH the row in hand: a machine-file row already installs the folder, so the
/// refusal names that file and spells the drop for its scope (`-g`) — the two exits it offers are
/// both about the row, because the row is what made the folder tracked. Both ride the `--json`
/// envelope as executable next actions.
#[test]
fn the_already_tracked_refusal_names_the_claiming_manifest_row() {
    let src = editable_source();
    let root = src.0.join("pr-describe");
    let h = Harness::new("dup-claimed");
    ops::add(&h.ctx(), &root).unwrap();
    // The machine file the add's row would live in, spelled by absolute path (this rig has no
    // `$HOME` roots to abbreviate against).
    let manifest = h.home.0.join(crate::manifest::MANIFEST_FILE);
    std::fs::write(
        &manifest,
        format!(
            "[bundles]\n\"{}\" = \"*\"\n",
            root.canonicalize().unwrap().display()
        ),
    )
    .unwrap();

    let err = ops::add(&h.ctx(), &root).unwrap_err();
    let crate::error::ClientError::AlreadyTracked { claim, .. } = &err else {
        panic!("expected AlreadyTracked, got {err:?}");
    };
    let claim = claim.as_ref().expect("the machine row claims the folder");
    assert_eq!(claim.manifest, manifest.display().to_string());
    assert!(claim.global, "the machine file is the global scope");

    let message = crate::render::safe_message(&err);
    assert!(
        message.contains(&manifest.display().to_string()),
        "{message}"
    );
    assert!(message.contains("topos remove -g pr-describe"), "{message}");
    assert!(!message.contains("topos_"), "{message}");

    // The agent surface gets the same two exits, each a runnable argv.
    let argv: Vec<String> = ["topos", "add", "./pr-describe"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let actions = crate::render::next_actions("add", &argv, &err);
    let argvs: Vec<Vec<String>> = actions.into_iter().map(|a| a.argv).collect();
    assert_eq!(
        argvs,
        vec![
            vec!["topos", "diff", "pr-describe", "--json"],
            vec!["topos", "remove", "-g", "pr-describe", "--json"],
        ]
    );
}

#[test]
fn installing_the_trigger_is_crash_safe_across_the_fault_table() {
    // A realistic pre-existing settings.json: a foreign top-level key + a non-SessionStart hook.
    let user = Scratch::new("cc-fault");
    let claude = claude_home(&user.0);
    std::fs::create_dir_all(&claude).unwrap();
    let settings_path = claude.join("settings.json");
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"matcher\": \"Bash\"}]\n  }\n}\n";

    // Count the durable ops a clean install performs, so we fault each.
    std::fs::write(&settings_path, original).unwrap();
    let probe = FaultFs::new(0);
    claude_trigger(&user.0, &probe).install();
    let max_ops = probe.ops_attempted();
    assert!(
        max_ops >= 4,
        "the atomic config write performs at least temp/fsync/rename/fsync-dir"
    );

    for fail_at in 1..=max_ops {
        std::fs::write(&settings_path, original).unwrap(); // reset to the pre-state
        let fs = FaultFs::new(fail_at);
        let _ = claude_trigger(&user.0, &fs).install();

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

// ── unfollow: flag-flip only — bytes kept, token retained, idempotent, sweep skips ──────────────────
