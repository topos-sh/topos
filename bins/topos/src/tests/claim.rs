//! THE IDENTITY CLAIM (`add <path> --as <bundle>`) and its inverse — the copy that already exists
//! is brought under management without recreating it, and detaching it puts the world back exactly
//! as the claim found it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::{Lock, PlacementMap};
use topos_types::results::{ClaimState, RemoveKind};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::{AgentRoots, Ctx};
use crate::doc;
use crate::fs_seam::RealFs;
use crate::id::SkillId;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::ops;
use crate::sidecar::Layout;

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("topos-claim-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, for the same reason the reconcile rig canonicalizes: `$TMPDIR` sits behind
        // the macOS `/var` symlink, and every recorded-vs-named path compare would miss.
        let dir = dir.canonicalize().unwrap_or(dir);
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
        _naming: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: std::env::temp_dir().join(skill_id),
        }
    }
}
impl TriggerAdapter for NoHarness {
    fn slug(&self) -> &'static str {
        HarnessId::ClaudeCode.slug()
    }
    fn install(&self) -> TriggerReport {
        TriggerReport {
            agent: "claude-code".to_owned(),
            currency_kind: CurrencyKind::ExplicitPullOnly,
            touched_path: None,
            marker_id: "test".into(),
            state: TriggerState::Inactive,
            note: None,
        }
    }
    fn remove(&self) -> TriggerReport {
        self.install()
    }
    fn artifacts(&self) -> Vec<TriggerArtifact> {
        Vec::new()
    }
    fn present(&self) -> bool {
        false
    }
}

/// A machine-scope rig: a `~/.topos` home, a `work/` tree the folders live in, and a cwd that is
/// NOT inside any project (so `-g` is the honest scope for everything here).
struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NoHarness,
    plane: crate::plane::InertPlane,
    follow: crate::plane::InertFollow,
}

impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("c"),
            clock: FixedClock(1_700_000_000_000),
            harness: NoHarness,
            plane: crate::plane::InertPlane,
            follow: crate::plane::InertFollow,
        }
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            progress: crate::progress::silent(),
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_claim".to_owned(),
            layout: Layout::new(&self.home.0),
            harness: &self.harness,
            triggers: crate::ops::Triggers::active_only(&self.harness),
            plane: &self.plane,
            follow: &self.follow,
            // The cwd is deliberately a SEPARATE folder from the home tree: with the two equal,
            // every registry root under it would classify as a project dir and the user-scope
            // half of discovery would never be exercised.
            roots: Some(AgentRoots {
                home: self.work.0.clone(),
                cwd: Some(self.cwd()),
            }),
        }
    }

    /// The working directory every invocation stands in — inside no project.
    fn cwd(&self) -> PathBuf {
        let dir = self.work.0.join("cwd");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A bundle folder under `work/`, with the given SKILL.md body.
    fn folder(&self, name: &str, body: &str) -> PathBuf {
        let dir = self.work.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
        dir
    }

    /// Adopt `dir` machine-wide — the standing record every claim below is about.
    fn adopt(&self, dir: &Path) -> topos_types::results::AddData {
        let ctx = self.ctx();
        let scope = ops::add_scope(&ctx, true).unwrap();
        let mut data = ops::adopt_path(&ctx, &scope, dir, ops::KindDeclared::Yes).unwrap();
        ops::note_added_path_in(&ctx, &mut data, &scope.target, dir).unwrap();
        data
    }

    fn scope(&self, ctx: &Ctx<'_>) -> crate::ops::AddScope {
        ops::add_scope(ctx, true).unwrap()
    }

    fn map(&self, id: &str) -> PlacementMap {
        let ctx = self.ctx();
        doc::read_map(ctx.fs, &ctx.layout.published(&sid(id)).map)
            .unwrap()
            .unwrap()
    }

    fn lock(&self, id: &str) -> Lock {
        let ctx = self.ctx();
        doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&sid(id)).lock)
            .unwrap()
            .unwrap()
    }

    fn manifest(&self) -> String {
        std::fs::read_to_string(self.home.0.join("topos.toml")).unwrap()
    }
}

fn sid(id: &str) -> SkillId {
    SkillId::parse(id).expect("a rig id")
}

/// The state of the placement recorded LAST — the row a claim appends.
fn last_state(map: &PlacementMap) -> &topos_types::persisted::PlacementState {
    map.placement_state.last().expect("a placement")
}

// ---------------------------------------------------------------------------------------------
// The three claim arms
// ---------------------------------------------------------------------------------------------

#[test]
fn a_copy_at_the_current_version_is_recorded_clean_and_changes_nothing() {
    let rig = Rig::new("current");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();

    // A byte-identical second copy in another agent's folder.
    let copy = rig.folder("codex/pr-describe", "# pr\n");
    let before = std::fs::read(copy.join("SKILL.md")).unwrap();

    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    let claim = data.claim.as_ref().expect("a claim receipt");
    assert_eq!(claim.state, ClaimState::Current);
    assert!(claim.twin.is_none());
    // NOTHING in the folder changed.
    assert_eq!(std::fs::read(copy.join("SKILL.md")).unwrap(), before);
    assert_eq!(std::fs::read_dir(&copy).unwrap().count(), 1);

    // THE BASELINE IS WHAT MAKES THE CLAIM LIVE: a row with no `materialized_sha` scans Foreign
    // and is frozen out of every plan forever.
    let map = rig.map(&id);
    assert_eq!(map.placements.len(), 2);
    let state = last_state(&map);
    assert_eq!(
        state.materialized_sha.as_deref(),
        Some(rig.lock(&id).bundle_digest.as_str())
    );
    assert!(state.adopted_source, "the folder is the person's own");
    let scans = crate::placement::scan_placements(&ctx, &map).unwrap();
    assert!(
        scans
            .iter()
            .all(|s| matches!(s.status, crate::placement::ScanStatus::Clean { .. })),
        "a claimed copy scans clean, never foreign"
    );
    // The unpinned row was not touched: the claim is a record act.
    assert!(!rig.manifest().contains("dest"));
}

#[test]
fn a_copy_at_an_older_version_records_that_version_and_waits_for_the_next_update() {
    let rig = Rig::new("older");
    let source = rig.folder("pr-describe", "# v1\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();
    let v1_digest = added.bundle_digest.clone().unwrap();
    advance_to_v2(&rig, &id, &source);

    // A copy still holding v1 — a version the history DOES explain.
    let copy = rig.folder("codex/pr-describe", "# v1\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(data.claim.as_ref().unwrap().state, ClaimState::Older);
    let tty = crate::render::add_tty(&data);
    assert!(
        tty.contains("it holds an older version; the next update brings it current"),
        "{tty}"
    );

    // The baseline is THAT version's digest — so the copy scans CLEAN-at-stale, which is exactly
    // what the ordinary converge refreshes on the next sweep (a lock-digest baseline here would
    // have made it read as an edit and never catch up).
    let map = rig.map(&id);
    assert_eq!(
        last_state(&map).materialized_sha.as_deref(),
        Some(v1_digest.as_str())
    );
    let scans = crate::placement::scan_placements(&ctx, &map).unwrap();
    let claimed = scans
        .iter()
        .find(|s| s.dir == copy)
        .expect("the claimed row");
    assert!(matches!(
        claimed.status,
        crate::placement::ScanStatus::Clean { .. }
    ));
    assert_ne!(v1_digest, rig.lock(&id).bundle_digest, "the lock moved on");
}

/// Move the record forward one version, the way a landed publish does: the v2 bytes become a
/// stored commit and the lock (plus the source placement's baseline) advances to it, leaving v1 in
/// history where a claimed copy can still match it.
fn advance_to_v2(rig: &Rig, id: &str, source: &Path) {
    std::fs::write(source.join("SKILL.md"), "# v2\n").unwrap();
    let ctx = rig.ctx();
    let sp = ctx.layout.published(&sid(id));
    let mut lock = rig.lock(id);
    let scanned = crate::scan::scan(source).unwrap();
    let v2 = crate::ops::sync_engine::snapshot_draft(&ctx, &sp, &lock, &scanned).unwrap();
    let v2_digest = to_hex(&scanned.bundle_digest);
    lock.base_commit = v2.clone();
    lock.bundle_digest = v2_digest.clone();
    doc::write_doc(ctx.fs, &sp.lock, &lock).unwrap();
    let mut map = rig.map(id);
    map.applied_commit = v2;
    map.materialized_sha = v2_digest.clone();
    for st in &mut map.placement_state {
        st.materialized_sha = Some(v2_digest.clone());
    }
    doc::write_map(ctx.fs, &sp.map, &map).unwrap();
}

#[test]
fn a_copy_no_version_explains_becomes_the_draft_and_its_bytes_are_snapshotted_first() {
    let rig = Rig::new("edited");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();

    let copy = rig.folder("codex/pr-describe", "# pr\nlocal note\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(data.claim.as_ref().unwrap().state, ClaimState::Edited);

    // THE LOSS-PROOFING: the bytes are in the store before anything else could swap them out.
    let sp = ctx.layout.published(&sid(&id));
    let store = Store::open(&sp.store).unwrap();
    let scanned = crate::scan::scan(&copy).unwrap();
    assert!(
        ops::digest_in_history(&store, &to_hex(&scanned.bundle_digest)).unwrap(),
        "the claimed bytes must be recoverable from the store"
    );
    // Recorded against the LOCK, so the copy scans Modified and rides the draft machinery — never
    // against its own digest, which would make it refresh-bait.
    let map = rig.map(&id);
    assert_eq!(
        last_state(&map).materialized_sha.as_deref(),
        Some(rig.lock(&id).bundle_digest.as_str())
    );
    let scans = crate::placement::scan_placements(&ctx, &map).unwrap();
    let claimed = scans
        .iter()
        .find(|s| s.dir == copy)
        .expect("the claimed row");
    assert!(matches!(
        claimed.status,
        crate::placement::ScanStatus::Modified { .. }
    ));
    // THE DRAFT the receipt promises will spread and that `publish` will offer: the classifier
    // resolves the claimed copy as THE one advanced content, with the source stale behind it.
    let idx = claimed.idx;
    assert!(
        matches!(
            crate::placement::classify_draft(&scans, &map),
            crate::placement::DraftVerdict::One { idx: winner, .. } if winner == idx
        ),
        "the claimed copy is the bundle's draft"
    );
    let tty = crate::render::add_tty(&data);
    assert!(
        tty.contains("its edits become your draft of this skill"),
        "{tty}"
    );
    assert!(tty.contains("`topos publish` will offer them"), "{tty}");
}

#[test]
fn a_claim_beside_a_competing_draft_lands_and_says_the_copies_are_frozen() {
    let rig = Rig::new("frozen");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    // The source itself is edited one way…
    std::fs::write(source.join("SKILL.md"), "# pr\nmine\n").unwrap();
    // …and the claimed folder another. Neither explains the other.
    let copy = rig.folder("codex/pr-describe", "# pr\ntheirs\n");

    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(data.claim.as_ref().unwrap().state, ClaimState::Frozen);
    let tty = crate::render::add_tty(&data);
    assert!(tty.contains("nothing syncs until one is chosen"), "{tty}");
    assert!(tty.contains("--reset"), "{tty}");
}

// ---------------------------------------------------------------------------------------------
// The guards
// ---------------------------------------------------------------------------------------------

#[test]
fn claiming_a_folder_the_same_record_already_holds_changes_nothing() {
    let rig = Rig::new("idem");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &source, "pr-describe").unwrap();
    assert!(
        data.note.as_deref().unwrap().contains("nothing changed"),
        "{:?}",
        data.note
    );
    assert_eq!(rig.map(&id).placements.len(), 1, "no second row");
}

#[test]
fn claiming_a_folder_another_record_holds_refuses_by_name() {
    let rig = Rig::new("taken");
    let a = rig.folder("pr-describe", "# a\n");
    let b = rig.folder("code-review", "# b\n");
    rig.adopt(&a);
    rig.adopt(&b);
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let err = ops::claim(&ctx, &scope, &b, "pr-describe").unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    assert!(
        err.to_string()
            .ends_with("as pr-describe — this folder belongs to code-review"),
        "{err}"
    );
}

#[test]
fn a_folder_the_project_store_records_refuses_naming_that_file() {
    // CROSS-SCOPE: the other store's engine would converge the same directory.
    let rig = Rig::new("cross");
    let project = rig.work.0.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("topos.toml"), "[bundles]\n").unwrap();
    let inside = project.join("skills/deploy");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("SKILL.md"), "# deploy\n").unwrap();

    // Adopt it into the PROJECT store, standing in the checkout.
    let pctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let pscope = ops::add_scope(&pctx, false).unwrap();
    let mut data = ops::adopt_path(&pctx, &pscope, &inside, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&pctx, &mut data, &pscope.target, &inside).unwrap();

    // A machine-wide record of another name, and a `-g` claim over the project's folder.
    let mine = rig.folder("mine", "# mine\n");
    rig.adopt(&mine);
    let gctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let gscope = ops::add_scope(&gctx, true).unwrap();
    let err = ops::claim(&gctx, &gscope, &inside, "mine").unwrap_err();
    // The scope rail answers first — the folder is the project's, whoever records it.
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(err.to_string().contains("drop `-g`"), "{err}");
}

#[test]
fn a_folder_the_other_scope_records_refuses_naming_its_file() {
    // The CROSS-SCOPE probe proper: a project row may name a folder OUTSIDE the checkout (a
    // machine-local fact spelled absolutely), which puts it out of the scope rail's reach — and
    // two engines would then converge one directory from two files.
    let rig = Rig::new("cross-out");
    let project = rig.work.0.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("topos.toml"), "[bundles]\n").unwrap();
    let outside = rig.folder("shared/deploy", "# deploy\n");

    let pctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let pscope = ops::add_scope(&pctx, false).unwrap();
    let mut data = ops::adopt_path(&pctx, &pscope, &outside, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&pctx, &mut data, &pscope.target, &outside).unwrap();

    // Machine-wide, a different bundle — and a `-g` claim over the folder the project holds.
    let mine = rig.folder("mine", "# mine\n");
    rig.adopt(&mine);
    let err = ops::claim(
        &pctx,
        &ops::add_scope(&pctx, true).unwrap(),
        &outside,
        "mine",
    )
    .unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    assert!(said.contains("already belongs to deploy in "), "{said}");
    assert!(said.ends_with("repo/topos.toml"), "{said}");
}

#[test]
fn a_folder_outside_the_project_refuses_toward_the_machine_file() {
    let rig = Rig::new("outside");
    let project = rig.work.0.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("topos.toml"), "[bundles]\n").unwrap();
    let inside = project.join("skills/deploy");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("SKILL.md"), "# deploy\n").unwrap();

    let pctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let pscope = ops::add_scope(&pctx, false).unwrap();
    let mut data = ops::adopt_path(&pctx, &pscope, &inside, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&pctx, &mut data, &pscope.target, &inside).unwrap();

    let outside = rig.folder("elsewhere/deploy", "# deploy\n");
    let err = ops::claim(&pctx, &pscope, &outside, "deploy").unwrap_err();
    assert!(err.to_string().contains("-g"), "{err}");
}

#[test]
fn an_unreadable_folder_refuses_before_a_row_is_written() {
    let rig = Rig::new("unscannable");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();
    // A symlink inside the folder is exactly what the scanner refuses.
    let copy = rig.folder("codex/pr-describe", "# pr\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", copy.join("hosts")).unwrap();
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let err = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap_err();
    assert_eq!(err.code(), "SCAN_REJECTED");
    assert!(
        err.to_string().contains("the folder can't be read"),
        "{err}"
    );
    assert_eq!(rig.map(&id).placements.len(), 1, "nothing was recorded");
}

#[test]
fn an_mcp_bundle_has_no_folder_copy_to_claim() {
    let rig = Rig::new("mcp");
    let dir = rig.work.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("the workspace root")
                .join("tests/fixtures/mcp/valid/remote-no-auth.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let ctx = rig.ctx();
    let scope = ops::add_scope(&ctx, true).unwrap();
    let mut data =
        ops::adopt_path_any_kind(&ctx, &scope, &dir, crate::bundle_kind::BundleKind::Mcp).unwrap();
    ops::note_added_path_in(&ctx, &mut data, &scope.target, &dir).unwrap();
    let name = data.name.clone();

    let copy = rig.folder("codex/weather", "# not a server\n");
    let err = ops::claim(&ctx, &scope, &copy, &name).unwrap_err();
    assert!(err.to_string().contains("is an MCP server"), "{err}");
    assert!(
        err.to_string().contains("no folder copy to manage"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------------------------
// The twin
// ---------------------------------------------------------------------------------------------

#[test]
fn a_clean_duplicate_beside_the_claimed_folder_retires_with_it() {
    let rig = Rig::new("twin");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();

    // The engine's own suffixed copy, recorded beside the folder about to be claimed.
    let agents = rig.work.0.join("agents");
    let copy = agents.join("pr-describe");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "# pr\n").unwrap();
    let twin = agents.join("pr-describe-eng");
    std::fs::create_dir_all(&twin).unwrap();
    std::fs::write(twin.join("SKILL.md"), "# pr\n").unwrap();
    record_engine_copy(&rig, &id, &twin);

    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    let receipt = data.claim.as_ref().unwrap();
    let reported = receipt.twin.as_ref().expect("the duplicate");
    assert!(reported.removed);
    assert!(!twin.exists(), "the duplicate folder is gone");
    let tty = crate::render::add_tty(&data);
    assert!(
        tty.contains("removed the duplicate") && tty.contains("(unedited copy of the same skill)"),
        "{tty}"
    );
    let map = rig.map(&id);
    assert!(
        !map.placements.iter().any(|p| Path::new(p) == twin),
        "the duplicate's row is dropped"
    );
}

#[test]
fn an_edited_duplicate_is_kept_and_said() {
    let rig = Rig::new("twin-edited");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();

    let agents = rig.work.0.join("agents");
    let copy = agents.join("pr-describe");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "# pr\n").unwrap();
    let twin = agents.join("pr-describe-eng");
    std::fs::create_dir_all(&twin).unwrap();
    std::fs::write(twin.join("SKILL.md"), "# pr\n").unwrap();
    record_engine_copy(&rig, &id, &twin);
    // Edited AFTER the record — a draft of the person's own.
    std::fs::write(twin.join("SKILL.md"), "# pr\nedited\n").unwrap();

    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let data = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    let reported = data
        .claim
        .as_ref()
        .unwrap()
        .twin
        .as_ref()
        .expect("the duplicate");
    assert!(!reported.removed);
    assert!(twin.exists(), "an edited copy is never swept up");
    let tty = crate::render::add_tty(&data);
    assert!(tty.contains("kept the duplicate"), "{tty}");
}

/// Record `dir` as a placement TOPOS wrote (`adopted_source: false`, baseline = the current
/// digest) — the engine-placed copy the twin rule is about.
fn record_engine_copy(rig: &Rig, id: &str, dir: &Path) {
    let ctx = rig.ctx();
    let sp = ctx.layout.published(&sid(id));
    let mut map = doc::read_map(ctx.fs, &sp.map).unwrap().unwrap();
    let digest = to_hex(&crate::scan::scan(dir).unwrap().bundle_digest);
    map.placements.push(dir.to_string_lossy().into_owned());
    map.placement_state
        .push(topos_types::persisted::PlacementState {
            kind: topos_types::persisted::PlacementKind::Native,
            agent: None,
            materialized_sha: Some(digest),
            pre_existing_sha: None,
            swap_capability: topos_types::persisted::SwapCapability::Unsupported,
            adopted_source: false,
            claim: None,
        });
    doc::write_map(ctx.fs, &sp.map, &map).unwrap();
}

// ---------------------------------------------------------------------------------------------
// The detach — the claim's exact inverse
// ---------------------------------------------------------------------------------------------

#[test]
fn claim_then_detach_restores_the_record_and_the_file_byte_for_byte() {
    for pinned in [false, true] {
        let rig = Rig::new(if pinned {
            "roundtrip-pinned"
        } else {
            "roundtrip"
        });
        let source = rig.folder("pr-describe", "# pr\n");
        let ctx = rig.ctx();
        let scope = rig.scope(&ctx);
        let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
        if pinned {
            let dest = vec![rig.work.0.join("agents").to_string_lossy().into_owned()];
            ops::note_added_path_dest_in(&ctx, &mut data, &scope.target, &source, &dest).unwrap();
        } else {
            ops::note_added_path_in(&ctx, &mut data, &scope.target, &source).unwrap();
        }
        let id = data.skill_id.clone().unwrap();

        let agents = rig.work.0.join("agents");
        let copy = agents.join("pr-describe");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("SKILL.md"), "# pr\n").unwrap();

        let map_before = serde_json::to_string(&rig.map(&id)).unwrap();
        let file_before = rig.manifest();

        ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
        if pinned {
            // The dest-pinned row gained the claimed folder's root — the file IS the recipe.
            assert_ne!(
                rig.manifest(),
                file_before,
                "a pinned row records the folder"
            );
        } else {
            assert_eq!(rig.manifest(), file_before, "an unpinned row is untouched");
        }

        let out = ops::remove_global(
            &ctx,
            &no_sessions,
            &["pr-describe".to_owned()],
            None,
            true,
            &ops::Selection::new(&[], &["~/agents".to_owned()]),
        )
        .unwrap();
        let ops::RemoveOutcome::Applied(applied) = out else {
            panic!("a detach loses nothing and applies immediately");
        };
        assert_eq!(applied.items[0].kind, RemoveKind::ClaimDetached);
        assert!(copy.join("SKILL.md").exists(), "the bytes stay");

        assert_eq!(
            serde_json::to_string(&rig.map(&id)).unwrap(),
            map_before,
            "the record is exactly what the claim found"
        );
        assert_eq!(rig.manifest(), file_before, "and so is the file");
    }
}

#[test]
fn the_detach_receipt_says_the_folder_stays() {
    let rig = Rig::new("detach-copy");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    let agents = rig.work.0.join("agents");
    let copy = agents.join("pr-describe");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "# pr\n").unwrap();
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();

    let out = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/agents".to_owned()]),
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(applied) = out else {
        panic!("applies immediately");
    };
    let line = crate::render::remove_applied_tty(&applied);
    assert!(
        line.contains(
            "removed ~/agents/pr-describe from pr-describe — the folder and its files stay; it \
             just stops updating"
        ),
        "{line}"
    );
    // The exact inverse is offered, because it verifiably restores the whole prior state.
    assert!(
        line.contains(&format!("Undo: topos add -g {} --as ", copy.display())),
        "{line}"
    );
}

#[test]
fn a_detach_answers_to_every_spelling_the_copy_has() {
    // NAMING AN EXISTING COPY is not writing a manifest line: a pasted absolute path, the
    // `~/`-spelled root, and the claimed FOLDER's own path all name the folder they obviously
    // name. Matching the manifest dialect verbatim refused three of the four — and then, on a row
    // with no destinations, refused with "nothing has synced" while `list` showed two placements.
    let agents_abs = |rig: &Rig| rig.work.0.join("agents").to_string_lossy().into_owned();
    let copy_abs = |rig: &Rig| {
        rig.work
            .0
            .join("agents/pr-describe")
            .to_string_lossy()
            .into_owned()
    };
    for spelling in [
        Box::new(|_: &Rig| "~/agents".to_owned()) as Box<dyn Fn(&Rig) -> String>,
        Box::new(agents_abs),
        Box::new(|_: &Rig| "~/agents/pr-describe".to_owned()),
        Box::new(copy_abs),
    ] {
        let rig = Rig::new("spellings");
        let source = rig.folder("pr-describe", "# pr\n");
        rig.adopt(&source);
        let copy = rig.folder("agents/pr-describe", "# pr\n");
        let ctx = rig.ctx();
        ops::claim(&ctx, &rig.scope(&ctx), &copy, "pr-describe").unwrap();

        let token = spelling(&rig);
        let out = ops::remove_global(
            &ctx,
            &no_sessions,
            &["pr-describe".to_owned()],
            None,
            true,
            &ops::Selection::new(&[], std::slice::from_ref(&token)),
        )
        .unwrap_or_else(|e| panic!("`--dest {token}` must name the copy: {e}"));
        let ops::RemoveOutcome::Applied(applied) = out else {
            panic!("a detach applies immediately");
        };
        assert_eq!(applied.items[0].kind, RemoveKind::ClaimDetached, "{token}");
        assert!(copy.join("SKILL.md").exists(), "the bytes stay ({token})");
    }
}

#[test]
fn a_dest_that_names_no_copy_says_which_copies_there_are() {
    // The honest not-found. A row with no destinations resolves an EMPTY set (every placement is
    // a folder the person owns, which no destination set speaks for), and the zero-state refusal
    // used to state that nothing had synced — a plain falsehood beside `list`.
    let rig = Rig::new("miss");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    let copy = rig.folder("agents/pr-describe", "# pr\n");
    let ctx = rig.ctx();
    ops::claim(&ctx, &rig.scope(&ctx), &copy, "pr-describe").unwrap();

    let err = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/nowhere".to_owned()]),
    )
    .unwrap_err();
    let said = err.to_string();
    assert!(
        said.starts_with("no copy of 'pr-describe' is in --dest ~/nowhere"),
        "{said}"
    );
    assert!(said.contains("~/agents/pr-describe"), "{said}");
    assert!(!said.contains("nothing has synced"), "{said}");
}

#[test]
#[cfg(unix)]
fn a_folder_the_kernel_refuses_to_open_answers_the_cant_read_shape() {
    use std::os::unix::fs::PermissionsExt;
    // Running as root ignores the mode bits, so the folder would read fine and prove nothing.
    if std::env::var("USER").as_deref() == Ok("root") {
        return;
    }
    let rig = Rig::new("eacces");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    let copy = rig.folder("agents/pr-describe", "# pr\n");
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o000)).unwrap();

    let ctx = rig.ctx();
    let err = ops::claim(&ctx, &rig.scope(&ctx), &copy, "pr-describe").unwrap_err();
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The scanner-hazard answer, for the same news: the folder, and why it cannot be read.
    assert_eq!(err.code(), "SCAN_REJECTED");
    let said = err.to_string();
    assert!(
        said.starts_with("can't add ~/agents/pr-describe — the folder can't be read ("),
        "{said}"
    );
    assert!(said.contains("permission denied"), "{said}");
    // And PERMANENT: `chmod 000` answers identically next run, so no agent is sent round again.
    assert_eq!(
        err.outcome(),
        topos_types::TerminalOutcome::PermanentFailure
    );
}

#[test]
fn an_add_that_changed_nothing_leads_with_that_and_nothing_else() {
    // AN ACT THAT DID NOT HAPPEN MAY NOT HEAD THE ANSWER. A repeat claim printed the full
    // `added … as a copy … updates land here from now on` lead and retracted it one line down.
    let rig = Rig::new("unchanged");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    let copy = rig.folder("agents/pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();

    let again = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert!(again.unchanged);
    let tty = crate::render::add_tty(&again);
    let mut lines = tty.lines();
    assert_eq!(
        lines.next(),
        Some("~/agents/pr-describe is already one of pr-describe's folders — nothing changed")
    );
    assert!(lines.next().unwrap().starts_with("source: "), "{tty}");
    assert_eq!(lines.next(), None, "{tty}");
    assert!(!tty.contains("added "), "{tty}");

    // The same rule on the destination receipt: a `-a` naming what the row already spells.
    let mut data = ops::add_scope(&ctx, true)
        .and_then(|scope| {
            let mut d = again.clone();
            d.unchanged = false;
            d.note = None;
            ops::note_added_path_dest_in(
                &ctx,
                &mut d,
                &scope.target,
                &source,
                &["~/claude".to_owned()],
            )?;
            // The SECOND time, naming exactly what the row now spells.
            let mut again = d.clone();
            again.note = None;
            ops::note_added_path_dest_in(
                &ctx,
                &mut again,
                &scope.target,
                &source,
                &["~/claude".to_owned()],
            )?;
            Ok(again)
        })
        .unwrap();
    data.claim = None;
    assert!(data.unchanged, "the row already spelled it");
    let tty = crate::render::add_tty(&data);
    assert!(
        tty.starts_with("`") && tty.contains("nothing changed"),
        "{tty}"
    );
    assert!(!tty.contains("installed ("), "{tty}");
}

#[test]
fn a_dest_a_row_already_reaches_freezes_nothing_and_says_so() {
    // A row that names NO destinations reaches EVERY agent. Naming one it already reaches asks for
    // nothing — and the extend arm, having materialized the current set to append to, wrote that
    // set back as the row's frozen `dest`. The file silently changed shape, and an agent set up
    // tomorrow would stop receiving. The documented promise is the opposite: naming a destination
    // the line already has changes nothing, and says so.
    let rig = Rig::new("already-reached");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();
    // A copy TOPOS placed, so the row's current resolved set is `~/agents`.
    let agents = rig.work.0.join("agents");
    let placed = agents.join("pr-describe");
    std::fs::create_dir_all(&placed).unwrap();
    std::fs::write(placed.join("SKILL.md"), "# pr\n").unwrap();
    record_engine_copy(&rig, &id, &placed);

    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let before = rig.manifest();
    assert!(
        !before.contains("dest"),
        "the row reaches every agent: {before}"
    );
    let mut data = added.clone();
    data.note = None;
    data.undo = Vec::new();
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/agents".to_owned()],
    )
    .unwrap();

    assert_eq!(rig.manifest(), before, "the row is untouched, and unfrozen");
    assert!(data.unchanged, "and the answer says so");
    let tty = crate::render::add_tty(&data);
    assert!(tty.contains("nothing changed"), "{tty}");
    assert!(!tty.contains("installed ("), "{tty}");
}

#[test]
fn a_name_the_other_scope_adopted_from_a_folder_is_never_adopted_twice() {
    // ONE FOLDER, ONE ENGINE. The machine store adopted `~/skills/deploy` in place; a bare
    // `topos add deploy` inside a project used to resolve that record and adopt THE SAME
    // DIRECTORY into the project store — the two-engines-one-directory state the claim door
    // refuses outright. A reference is re-recordable (each scope lands its own copies); a folder
    // is not, and the honest answer names the file that already asks for it.
    let rig = Rig::new("cross-folder");
    let source = rig.folder("skills/deploy", "# deploy\n");
    rig.adopt(&source);

    let project = rig.work.0.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("topos.toml"), "[bundles]\n").unwrap();
    let pctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let roots = ops::DiscoveryRoots {
        home: rig.work.0.clone(),
        cwd: Some(project.clone()),
    };
    let err = ops::plan_bare_add(
        &pctx,
        &no_sessions,
        &roots,
        "deploy",
        ops::BareAdd {
            subscribe: true,
            dest_selected: false,
            global: false,
            workspace: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    assert!(
        said.starts_with("deploy is already added machine-wide"),
        "{said}"
    );
    assert!(said.contains("source: "), "{said}");
    // And the project store recorded nothing.
    assert!(
        crate::sidecar::existing_project_store(pctx.fs, &project).is_none(),
        "no second record was minted"
    );
}

#[test]
fn a_fresh_claim_over_a_row_that_already_names_the_root_states_the_claim() {
    // The row already names the folder's root, so the row write is redundant — but a PLACEMENT was
    // recorded, which is a real change. The receipt must lead with the claim, carry no
    // nothing-changed note (the row's note names the row's SOURCE folder, which beside a claim
    // headline reads as a statement about the folder just claimed), and record adding nothing.
    let rig = Rig::new("fresh-redundant-row");
    let source = rig.folder("pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/agents".to_owned()],
    )
    .unwrap();
    let id = data.skill_id.clone().unwrap();

    let copy = rig.folder("agents/pr-describe", "# pr\n");
    let claimed = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert!(!claimed.unchanged, "a placement was recorded");
    assert!(claimed.note.is_none(), "{:?}", claimed.note);
    let tty = crate::render::add_tty(&claimed);
    assert!(
        tty.starts_with("added ~/agents/pr-describe as a copy of pr-describe"),
        "{tty}"
    );
    assert!(!tty.contains("nothing changed"), "{tty}");
    // And the row gained nothing, so its detach takes nothing back.
    assert!(
        rig.map(&id)
            .placement_state
            .last()
            .unwrap()
            .claim
            .as_ref()
            .expect("a claim marker")
            .added_dest
            .is_none()
    );
}

#[test]
fn a_crash_before_the_row_write_still_leaves_the_detach_exact() {
    // THE WINDOW: the record is written, the row is not. The stamp rides the SAME map write as the
    // placement, so it is already there — a detach can never under-subtract — and the re-run
    // repairs the row half. A twin the crashed run never reached retires on that same re-run.
    let rig = Rig::new("crash-window");
    let source = rig.folder("pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/claude".to_owned()],
    )
    .unwrap();
    let id = data.skill_id.clone().unwrap();

    let agents = rig.work.0.join("agents");
    let copy = agents.join("pr-describe");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "# pr\n").unwrap();
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    let complete = rig.manifest();

    // The crash, staged: the record (stamp included) stands, the row does not.
    std::fs::write(
        rig.home.0.join("topos.toml"),
        complete.replace(", \"~/agents\"", ""),
    )
    .unwrap();
    // The stamp survived the crash — which is what keeps a detach from under-subtracting.
    assert_eq!(
        rig.map(&id)
            .placement_state
            .last()
            .unwrap()
            .claim
            .as_ref()
            .unwrap()
            .added_dest
            .as_deref(),
        Some("~/agents")
    );
    // A twin the crashed run never got to.
    let twin = agents.join("pr-describe-eng");
    std::fs::create_dir_all(&twin).unwrap();
    std::fs::write(twin.join("SKILL.md"), "# pr\n").unwrap();
    record_engine_copy(&rig, &id, &twin);

    let again = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(rig.manifest(), complete, "the row half is repaired");
    assert!(!twin.exists(), "and the duplicate retires on the re-run");
    assert!(!again.unchanged, "a repair is not nothing");

    // The detach is exact either way: the row loses ~/agents and keeps ~/claude.
    let out = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/agents".to_owned()]),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let file = rig.manifest();
    assert!(!file.contains("~/agents"), "{file}");
    assert!(file.contains("~/claude"), "{file}");
}

#[test]
fn the_row_entry_a_claim_will_add_is_recorded_with_the_placement_not_after_it() {
    // ONE FACT, ONE WRITE. Recording "the claim put this entry there" AFTER the row write made it
    // a third write that could be lost — and the loss is permanent, because the re-run's row write
    // then finds the entry already present and reports adding nothing, so the stamp never lands.
    // A detach would under-subtract forever: the row keeps the root, and the next sweep
    // re-materializes an engine copy right after the receipt said the folder stops updating.
    let rig = Rig::new("stamp-with-placement");
    let source = rig.folder("pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/claude".to_owned()],
    )
    .unwrap();
    let id = data.skill_id.clone().unwrap();
    let copy = rig.folder("agents/pr-describe", "# pr\n");

    // The row write FAILS — an unwritable manifest DIRECTORY (the editor stages a sibling and
    // renames it into place) is the deterministic stand-in for a crash between the two writes.
    let home = rig.home.0.clone();
    let mut perms = std::fs::metadata(&home).unwrap().permissions();
    let restore = perms.clone();
    perms.set_readonly(true);
    std::fs::set_permissions(&home, perms).unwrap();
    let failed = ops::claim(&ctx, &scope, &copy, "pr-describe");
    std::fs::set_permissions(&home, restore).unwrap();
    assert!(failed.is_err(), "the row write could not land");

    // THE RECORD ALREADY CARRIES IT: the placement and its stamp were one write.
    let map = rig.map(&id);
    let stamped = map
        .placement_state
        .iter()
        .zip(&map.placements)
        .find(|(_, p)| Path::new(p) == copy)
        .map(|(st, _)| st.claim.as_ref().and_then(|c| c.added_dest.clone()));
    assert_eq!(
        stamped,
        Some(Some("~/agents".to_owned())),
        "the stamp rides the placement write, not a later one"
    );

    // The re-run repairs the row half, and the detach is then exact.
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert!(rig.manifest().contains("~/agents"));
    let out = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/agents".to_owned()]),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let file = rig.manifest();
    assert!(
        !file.contains("~/agents"),
        "the root the claim added leaves: {file}"
    );
    assert!(file.contains("~/claude"), "{file}");
}

/// The rig has no sessions, so no transport is ever built.
fn no_sessions(_s: &crate::sessions::Session) -> ops::SessionTransports {
    unreachable!("the claim suite runs with no sessions")
}

// ---------------------------------------------------------------------------------------------
// The already-added answer's OPTIONS — the seam the claim completes
// ---------------------------------------------------------------------------------------------

#[test]
fn the_already_added_answer_offers_only_the_copies_whose_bytes_it_can_prove() {
    let rig = Rig::new("options");
    // The standing record, adopted from one agent's folder (creating the dir installs the agent,
    // as far as detection is concerned — its detect dir is the same tree).
    let source = rig.folder(".claude/skills/pr-describe", "# pr\n");
    rig.adopt(&source);
    // A byte-identical stray in another agent's folder — provably this bundle.
    let same = rig.folder(".cursor/skills/pr-describe", "# pr\n");
    // A same-NAME stray holding something else entirely — never offered, because the name is not
    // evidence and merging two histories that never met is not a suggestion to make.
    rig.folder(".codeium/windsurf/skills/pr-describe", "# something else\n");

    let ctx = rig.ctx();
    let roots = ops::DiscoveryRoots {
        home: rig.work.0.clone(),
        cwd: Some(rig.cwd()),
    };
    let err = ops::plan_bare_add(
        &ctx,
        &no_sessions,
        &roots,
        "pr-describe",
        ops::BareAdd {
            subscribe: true,
            dest_selected: false,
            global: true,
            workspace: None,
        },
    )
    .unwrap_err();
    let crate::error::ClientError::AlreadyAdded { candidates, .. } = &err else {
        panic!("the standing answer, with its options: {err}");
    };
    let spellings: Vec<String> = candidates.iter().map(|c| c.spelling()).collect();
    assert_eq!(
        spellings,
        vec![format!("{} --as {}", same.display(), source.display())],
        "byte proof only, and every printed line spells the whole reference"
    );
    // The count line rides the answer itself; the runnable lines ride the hint surface, `-g` and
    // all, and the printed folder is the `~/…` a shell expands back to the agent's own argv.
    assert!(
        err.to_string()
            .ends_with("1 unmanaged copy looks like it — manage it as the same skill:"),
        "{err}"
    );
    let hint = crate::render::err_hint_tty("add", &["add".to_owned()], &err).unwrap();
    assert!(hint.starts_with("  topos add -g "), "{hint}");
    assert!(hint.contains(" --as "), "{hint}");
}

#[test]
fn a_project_invoked_answer_never_offers_a_home_folder() {
    // SCOPE, as everywhere: the copies a project answer offers are the checkout's own.
    let rig = Rig::new("options-scope");
    // Two agents are installed here: Claude Code (whose project dir holds the record) and Cursor
    // (whose project dir is the shared `.agents/skills`).
    std::fs::create_dir_all(rig.work.0.join(".claude")).unwrap();
    std::fs::create_dir_all(rig.work.0.join(".cursor")).unwrap();
    let project = rig.work.0.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("topos.toml"), "[bundles]\n").unwrap();
    let source = project.join(".claude/skills/pr-describe");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), "# pr\n").unwrap();

    let ctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let scope = ops::add_scope(&ctx, false).unwrap();
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&ctx, &mut data, &scope.target, &source).unwrap();

    // A byte-identical copy in the HOME tree, and one inside the checkout.
    rig.folder(".cursor/skills/pr-describe", "# pr\n");
    let in_project = project.join(".agents/skills/pr-describe");
    std::fs::create_dir_all(&in_project).unwrap();
    std::fs::write(in_project.join("SKILL.md"), "# pr\n").unwrap();

    let roots = ops::DiscoveryRoots {
        home: rig.work.0.clone(),
        cwd: Some(project.clone()),
    };
    let err = ops::plan_bare_add(
        &ctx,
        &no_sessions,
        &roots,
        "pr-describe",
        ops::BareAdd {
            subscribe: true,
            dest_selected: false,
            global: false,
            workspace: None,
        },
    )
    .unwrap_err();
    let crate::error::ClientError::AlreadyAdded { candidates, .. } = &err else {
        panic!("the standing answer: {err}");
    };
    let refs: Vec<&str> = candidates.iter().map(|c| c.reference.as_str()).collect();
    assert_eq!(refs, vec![in_project.to_string_lossy().as_ref()]);
}

// ---------------------------------------------------------------------------------------------
// The dest-pinned row
// ---------------------------------------------------------------------------------------------

#[test]
fn a_claim_on_a_dest_pinned_row_extends_the_row_with_the_folder() {
    // A dest-frozen row plans from its entries ALONE, so a record-only claim would be starved by
    // the next sweep: the file is the recipe, and the claim writes the folder into it.
    let rig = Rig::new("pinned");
    let source = rig.folder("pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/claude".to_owned()],
    )
    .unwrap();
    let id = data.skill_id.clone().unwrap();

    let copy = rig.folder("agents/pr-describe", "# pr\n");
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    let file = rig.manifest();
    assert!(file.contains("~/claude"), "{file}");
    assert!(file.contains("~/agents"), "{file}");

    // And the row's own planner now names the claimed folder, so the sweep keeps it.
    let map = rig.map(&id);
    let lock = rig.lock(&id);
    let plan = crate::placement::plan_for_skill(&ctx, &id, &lock, &map);
    assert!(
        plan.holds_planned_dir(&copy),
        "the claimed folder is planned: {plan:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The planning invariant: a claimed folder is a target of EVERY plan
// ---------------------------------------------------------------------------------------------

/// A follow state that claims ONE record for a workspace — what makes `plan_for_skill` take the
/// delivered-bundle branches instead of the purely-local one.
struct DeliveredFollow(String);
impl crate::plane::FollowSource for DeliveredFollow {
    fn followed(&self) -> Vec<(String, crate::plane::FollowContext)> {
        vec![(
            self.0.clone(),
            crate::plane::FollowContext {
                workspace_id: "w_eng".to_owned(),
                mode: crate::plane::FollowMode::Auto,
                review_required: false,
                following: true,
            },
        )]
    }
}

#[test]
fn a_claimed_folder_is_planned_whatever_the_row_shape_and_detection_say() {
    // THE CLAIM'S PROMISE IS CURRENCY. Each dir planner reaches its targets a different way —
    // detection keys, project keys, a row's frozen roots — and a folder the PERSON named sits
    // under none of them by right. An unplanned placement is excluded from `managed_indices`, so
    // the claim would succeed and updates would never land.
    let rig = Rig::new("planned");
    let source = rig.folder(".cursor/skills/pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();
    let copy = rig.folder(".cursor/skills/pr-describe-mine", "# pr\n");
    let ctx = rig.ctx();
    ops::claim(&ctx, &rig.scope(&ctx), &copy, "pr-describe").unwrap();
    // BOTH placements attributed to the SAME installed agent — the attribution a machine where
    // Cursor is set up really makes. The detection planner reuses ONE prior dir per `(kind, agent)`
    // key, and the agent-less always-managed arm covers neither, so without the claim's own arm
    // the second folder simply falls out of the plan.
    attribute_all(&rig, &id, "cursor");

    let map = rig.map(&id);
    let lock = rig.lock(&id);
    let plan = crate::placement::plan_for_skill(&ctx, &id, &lock, &map);
    assert!(plan.holds_planned_dir(&copy), "machine plan: {plan:?}");
    assert_eq!(
        crate::placement::managed_indices(&map, &plan).len(),
        map.placements.len(),
        "every recorded placement is managed"
    );
    // The same record DELIVERED by a workspace takes the detection planner instead.
    let follow = DeliveredFollow(id.clone());
    let delivered = Ctx {
        follow: &follow,
        ..rig.ctx()
    };
    let plan = crate::placement::plan_for_skill(&delivered, &id, &lock, &map);
    assert!(plan.holds_planned_dir(&copy), "delivered plan: {plan:?}");
}

/// Attribute every recorded placement to one agent — the attribution `folder_readers` makes on a
/// machine where that agent is installed (it resolves against the process's own `$HOME`, which a
/// temp-dir rig cannot stand in for).
fn attribute_all(rig: &Rig, id: &str, slug: &str) {
    let ctx = rig.ctx();
    let sp = ctx.layout.published(&sid(id));
    let mut map = doc::read_map(ctx.fs, &sp.map).unwrap().unwrap();
    for st in &mut map.placement_state {
        st.agent = Some(slug.to_owned());
    }
    doc::write_map(ctx.fs, &sp.map, &map).unwrap();
}

#[test]
fn a_claimed_folder_in_a_project_is_planned_for_a_delivered_bundle() {
    // The PROJECT planner keys on the checkout's own agent dirs and preserves no arbitrary prior
    // target — the combination the machine-scope arms never exercise.
    let rig = Rig::new("planned-project");
    let project = rig.work.0.join("repo");
    std::fs::create_dir_all(project.join(".claude/skills")).unwrap();
    std::fs::write(project.join("topos.toml"), "[bundles]\n").unwrap();
    let source = project.join("skills/pr-describe");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), "# pr\n").unwrap();

    let pctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(project.clone()),
        }),
        ..rig.ctx()
    };
    let scope = ops::add_scope(&pctx, false).unwrap();
    let sctx = ops::ctx_with_layout(&pctx, &scope.layout);
    let mut data = ops::adopt_path(&pctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&pctx, &mut data, &scope.target, &source).unwrap();
    let id = data.skill_id.clone().unwrap();

    let copy = project.join(".agents/skills/pr-describe");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), "# pr\n").unwrap();
    ops::claim(&pctx, &scope, &copy, "pr-describe").unwrap();

    let map = doc::read_map(sctx.fs, &sctx.layout.published(&sid(&id)).map)
        .unwrap()
        .unwrap();
    let lock = doc::read_doc::<Lock>(sctx.fs, &sctx.layout.published(&sid(&id)).lock)
        .unwrap()
        .unwrap();
    let follow = DeliveredFollow(id.clone());
    let delivered = Ctx {
        follow: &follow,
        ..ops::ctx_with_layout(&pctx, &scope.layout)
    };
    let plan = crate::placement::plan_for_skill(&delivered, &id, &lock, &map);
    assert!(plan.holds_planned_dir(&copy), "project plan: {plan:?}");
}

// ---------------------------------------------------------------------------------------------
// The detach, exactly
// ---------------------------------------------------------------------------------------------

#[test]
fn a_detach_takes_back_only_what_the_claim_put_on_the_row() {
    // The row ALREADY named the claimed folder's root, so the claim changed no file — and its
    // inverse must change none either. Inferring "the claim added this" from "the row names it"
    // subtracted a pre-existing destination and, being the only one, dropped the WHOLE row,
    // uninstalling every copy of a bundle the person asked to stop managing ONE folder of.
    let rig = Rig::new("only-what-it-added");
    let source = rig.folder("pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    // TWO destinations, so the subtraction has something to get wrong: with the row's own entry
    // taken as evidence, `~/agents` leaves and the copy under it is uninstalled; with only one
    // destination the same mistake drops the row itself.
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/agents".to_owned(), "~/claude".to_owned()],
    )
    .unwrap();
    let id = data.skill_id.clone().unwrap();

    let copy = rig.folder("agents/pr-describe", "# pr\n");
    let file_before = rig.manifest();
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(
        rig.manifest(),
        file_before,
        "the row already named ~/agents"
    );
    assert!(
        rig.map(&id)
            .placement_state
            .last()
            .unwrap()
            .claim
            .as_ref()
            .expect("a claim marker")
            .added_dest
            .is_none(),
        "the claim recorded adding nothing"
    );

    let out = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/agents".to_owned()]),
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(applied) = out else {
        panic!("a detach applies immediately");
    };
    assert_eq!(applied.items[0].kind, RemoveKind::ClaimDetached);
    assert_eq!(rig.manifest(), file_before, "the row survives, whole");
    assert!(copy.join("SKILL.md").exists(), "the bytes stay");
    assert!(
        !rig.map(&id).placements.iter().any(|p| Path::new(p) == copy),
        "the record forgets the folder"
    );
}

#[test]
fn a_record_only_detach_never_writes_the_manifest() {
    // Nothing to edit means nothing to write: a record-only detach must not fail on a file it has
    // no business opening, nor rewrite one its receipt says it left alone. An unwritable manifest
    // DIRECTORY is the sharp proof — the editor stages a sibling and renames it in, so a
    // read-only file alone would not stop it.
    let rig = Rig::new("no-write");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    let copy = rig.folder("agents/pr-describe", "# pr\n");
    let ctx = rig.ctx();
    ops::claim(&ctx, &rig.scope(&ctx), &copy, "pr-describe").unwrap();

    let manifest = rig.home.0.join("topos.toml");
    let before = std::fs::read_to_string(&manifest).unwrap();
    let mut perms = std::fs::metadata(&rig.home.0).unwrap().permissions();
    let restore = perms.clone();
    perms.set_readonly(true);
    std::fs::set_permissions(&rig.home.0, perms).unwrap();
    let out = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/agents".to_owned()]),
    );
    std::fs::set_permissions(&rig.home.0, restore).unwrap();

    assert!(
        matches!(out, Ok(ops::RemoveOutcome::Applied(_))),
        "{:?}",
        out.err().map(|e| e.to_string())
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), before);
}

#[test]
fn a_claim_on_a_set_delivered_bundle_detaches_record_side() {
    // A SET line (here a whole repo) delivers the bundle and no row of its own spells it, so the
    // removal resolves to a SPLIT — whose per-agent narrowing refusal would leave the documented
    // `remove <bundle> --dest <folder>` inverse with no command behind it. The claim was
    // record-only, so its inverse is too: the set line is not touched at all.
    let rig = Rig::new("set-delivered");
    let source = rig.folder("pr-describe", "# pr\n");
    let added = rig.adopt(&source);
    let id = added.skill_id.clone().unwrap();
    // The record is a forge IMPORT, and the file spells only the repo line that delivers it.
    let ctx = rig.ctx();
    doc::write_doc(
        ctx.fs,
        &ctx.layout.published(&sid(&id)).origin,
        &ops::OriginDoc {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            origin: topos_types::results::SkillOrigin {
                source: "github.com/acme/tools".to_owned(),
                git_ref: None,
                commit: None,
                subdir: None,
                license: None,
            },
            imported_at: 0,
            members: vec!["pr-describe".to_owned()],
        },
    )
    .unwrap();
    let manifest = rig.home.0.join("topos.toml");
    std::fs::write(&manifest, "[bundles]\n\"github.com/acme/tools\" = \"*\"\n").unwrap();

    let copy = rig.folder("agents/pr-describe", "# pr\n");
    ops::claim(&ctx, &rig.scope(&ctx), &copy, "pr-describe").unwrap();
    let before = std::fs::read_to_string(&manifest).unwrap();

    let out = ops::remove_global(
        &ctx,
        &no_sessions,
        &["pr-describe".to_owned()],
        None,
        true,
        &ops::Selection::new(&[], &["~/agents".to_owned()]),
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(applied) = out else {
        panic!("a detach loses nothing and applies immediately");
    };
    assert_eq!(applied.items[0].kind, RemoveKind::ClaimDetached);
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        before,
        "the set line is untouched"
    );
    assert!(copy.join("SKILL.md").exists(), "the bytes stay");
    assert!(
        !rig.map(&id).placements.iter().any(|p| Path::new(p) == copy),
        "the record forgets the folder"
    );
}

// ---------------------------------------------------------------------------------------------
// The re-run: verify-and-repair, and an answer read off the record
// ---------------------------------------------------------------------------------------------

#[test]
fn a_re_run_repairs_a_row_the_first_claim_did_not_finish_writing() {
    // The claim is two writes, and a crash can land between them. The already-a-place path is
    // therefore a VERIFY-AND-REPAIR, not an early return: re-running converges the row.
    let rig = Rig::new("repair");
    let source = rig.folder("pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let mut data = ops::adopt_path(&ctx, &scope, &source, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut data,
        &scope.target,
        &source,
        &["~/claude".to_owned()],
    )
    .unwrap();
    let id = data.skill_id.clone().unwrap();
    let copy = rig.folder("agents/pr-describe", "# pr\n");
    ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    let complete = rig.manifest();
    assert!(complete.contains("~/agents"));

    // The crash, staged: the record stands, the row does not.
    let crashed = complete.replace(", \"~/agents\"", "");
    assert_ne!(crashed, complete, "the fixture must really drop the entry");
    std::fs::write(rig.home.0.join("topos.toml"), &crashed).unwrap();

    let again = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(rig.manifest(), complete, "the row is repaired");
    assert_eq!(rig.map(&id).placements.len(), 2, "and no second placement");
    // A repair CHANGED something, so it does not claim otherwise: the ordinary claim receipt.
    assert!(!again.unchanged);
    assert!(
        crate::render::add_tty(&again).starts_with("added ~/agents/pr-describe as a copy of"),
        "{}",
        crate::render::add_tty(&again)
    );
}

#[test]
fn a_re_claim_reads_the_state_off_the_record_rather_than_repeating_the_first_answer() {
    // A folder claimed clean and edited since is EDITED — the receipt must not keep saying
    // "nothing in it changed" about bytes that have moved.
    let rig = Rig::new("restate");
    let source = rig.folder("pr-describe", "# pr\n");
    rig.adopt(&source);
    let copy = rig.folder("agents/pr-describe", "# pr\n");
    let ctx = rig.ctx();
    let scope = rig.scope(&ctx);
    let first = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(first.claim.as_ref().unwrap().state, ClaimState::Current);

    std::fs::write(copy.join("SKILL.md"), "# pr\nedited here\n").unwrap();
    let again = ops::claim(&ctx, &scope, &copy, "pr-describe").unwrap();
    assert_eq!(again.claim.as_ref().unwrap().state, ClaimState::Edited);
}

#[test]
fn an_ancestor_project_that_records_the_folder_refuses_a_nested_claim() {
    // A nested checkout is TWO scopes, and the outer one's sweep converges the folders its rows
    // name. Probing only the machine store let an inner claim double-claim an outer folder.
    let rig = Rig::new("nested");
    let outer = rig.work.0.join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(outer.join("topos.toml"), "[bundles]\n").unwrap();
    std::fs::write(inner.join("topos.toml"), "[bundles]\n").unwrap();
    // The contested folder sits INSIDE the nested checkout — so the scope rail passes it (it IS
    // the inner project's business by location) and only the ancestor probe can catch that the
    // outer project's own row already names it.
    let shared = inner.join("shared/deploy");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("SKILL.md"), "# deploy\n").unwrap();

    // The OUTER project records the folder.
    let octx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(outer.clone()),
        }),
        ..rig.ctx()
    };
    let oscope = ops::add_scope(&octx, false).unwrap();
    let mut data = ops::adopt_path(&octx, &oscope, &shared, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&octx, &mut data, &oscope.target, &shared).unwrap();

    // The INNER project has a bundle of its own, and claims the outer's folder for it.
    let ictx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(inner.clone()),
        }),
        ..rig.ctx()
    };
    let iscope = ops::add_scope(&ictx, false).unwrap();
    let mine = inner.join("skills/mine");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::write(mine.join("SKILL.md"), "# mine\n").unwrap();
    let mut data = ops::adopt_path(&ictx, &iscope, &mine, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&ictx, &mut data, &iscope.target, &mine).unwrap();

    let err = ops::claim(&ictx, &iscope, &shared, "mine").unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    assert!(said.contains("already belongs to deploy in "), "{said}");
    assert!(said.ends_with("outer/topos.toml"), "{said}");
}

// ---------------------------------------------------------------------------------------------
// The PATH door asks the same ownership question — a plain `add <path>` over a folder the other
// scope already manages is the same two-engines state, met without a `--as`.
// ---------------------------------------------------------------------------------------------

/// A project checkout with its own manifest, and a ctx standing in it.
fn project_at<'a>(rig: &'a Rig, dir: &Path) -> (Ctx<'a>, ops::AddScope) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("topos.toml"), "[bundles]\n").unwrap();
    let ctx = Ctx {
        roots: Some(AgentRoots {
            home: rig.work.0.clone(),
            cwd: Some(dir.to_path_buf()),
        }),
        ..rig.ctx()
    };
    let scope = ops::add_scope(&ctx, false).unwrap();
    (ctx, scope)
}

#[test]
fn a_path_add_refuses_a_folder_the_machine_scope_already_manages() {
    // The plain path door, from a project, over a folder `~/.topos/topos.toml` already installs:
    // adopting it here would put a second store's engine on one directory — the state the claim
    // door has always refused, reached without a `--as`.
    let rig = Rig::new("path-cross");
    let shared = rig.folder("deploy", "# deploy\n");
    rig.adopt(&shared); // machine-wide

    let project = rig.work.0.join("proj");
    let (pctx, pscope) = project_at(&rig, &project);
    let err = ops::adopt_path(&pctx, &pscope, &shared, ops::KindDeclared::Yes).unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    // The claim door's own sentence, minus the `as <bundle>` a path add never typed — and naming
    // the file the owner answers to, which is the only place the person can act on it.
    assert!(
        said.contains("already belongs to deploy in ~/.topos/topos.toml"),
        "{said}"
    );
    assert!(!said.contains(" as "), "no bundle was named: {said}");
    // NOTHING landed: the project store has no record for the folder.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &project);
    assert!(
        playout.is_none_or(|l| ops::tracked_skill_at(
            &ops::ctx_with_layout(&pctx, &l),
            &shared.canonicalize().unwrap()
        )
        .unwrap()
        .is_none()),
        "the refusal is before any write"
    );
}

#[test]
fn a_global_path_add_refuses_a_folder_a_project_already_manages() {
    // The `-g` mirror: the machine file may not adopt a folder a checkout's own store installs.
    let rig = Rig::new("path-cross-g");
    let project = rig.work.0.join("proj");
    let (pctx, pscope) = project_at(&rig, &project);
    let inside = project.join("skills/deploy");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("SKILL.md"), "# deploy\n").unwrap();
    let mut data = ops::adopt_path(&pctx, &pscope, &inside, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&pctx, &mut data, &pscope.target, &inside).unwrap();

    // `-g` from inside the same checkout.
    let gscope = ops::add_scope(&pctx, true).unwrap();
    let err = ops::adopt_path(&pctx, &gscope, &inside, ops::KindDeclared::Yes).unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    assert!(said.contains("already belongs to deploy in "), "{said}");
    assert!(said.ends_with("proj/topos.toml"), "{said}");
}

#[test]
fn a_path_add_refuses_a_folder_an_ancestor_project_already_manages() {
    // A nested checkout is TWO scopes: the ancestor's sweep converges the folders its rows name,
    // so the inner project may not adopt one of them either.
    let rig = Rig::new("path-nested");
    let outer = rig.work.0.join("outer");
    let inner = outer.join("inner");
    let (octx, oscope) = project_at(&rig, &outer);
    let (ictx, iscope) = project_at(&rig, &inner);
    let shared = inner.join("shared/deploy");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("SKILL.md"), "# deploy\n").unwrap();
    let mut data = ops::adopt_path(&octx, &oscope, &shared, ops::KindDeclared::Yes).unwrap();
    ops::note_added_path_in(&octx, &mut data, &oscope.target, &shared).unwrap();

    let err = ops::adopt_path(&ictx, &iscope, &shared, ops::KindDeclared::Yes).unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    assert!(said.contains("already belongs to deploy in "), "{said}");
    assert!(said.ends_with("outer/topos.toml"), "{said}");
}

#[test]
fn a_path_add_in_the_scope_that_manages_the_folder_keeps_its_own_answer() {
    // The SAME-SCOPE case is this scope's own business, and its own answer stands: the row is
    // still there, so the re-add is the already-tracked refusal that names the row — never the
    // cross-scope sentence about another file.
    let rig = Rig::new("path-same");
    let shared = rig.folder("deploy", "# deploy\n");
    rig.adopt(&shared);
    let ctx = rig.ctx();
    let scope = ops::add_scope(&ctx, true).unwrap();
    let err = ops::adopt_path(&ctx, &scope, &shared, ops::KindDeclared::Yes).unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    let said = err.to_string();
    assert!(said.contains("'deploy' already tracks "), "{said}");
    assert!(!said.contains("belongs to"), "{said}");
}
