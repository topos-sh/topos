//! End-to-end tests of the pull/apply engine against a real git store + fixture plane responses (no
//! HTTP). These exercise the release-blocker invariants: never-clobber-draft, the served record IS the
//! sync target (a server-restore backward move applies too), the crash-after-swap heal, mis-scoped
//! rejection, go-back/resume, and the first receive landing on the bare sweep (a manifest row IS the
//! consent), all through the public `ops::pull` entry point. Integrity is the content-addressed version
//! id re-verified on apply — there is no signature and no anti-rollback floor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::identity::{self, Commit};
use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::SyncState;
use topos_types::results::{PullAction, PullData};
use topos_types::{
    CurrencyKind, CurrentRecord, HarnessId, PointerScope, TriggerReport, TriggerState,
    WireCurrentRecord,
};

use crate::ctx::Ctx;
use crate::fs_seam::{FaultFs, FsOps, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    FollowContext, FollowSource, InertFollow, InertPlane, KnownCurrent, PlaneError, PlaneSource,
    PointerFetch,
};
use crate::sidecar::Layout;
use crate::{doc, ops};

const WS: &str = "w_acme";
const DEVICE: &str = "d_test";

// ---------------------------------------------------------------------------------------------
// Scratch + fixtures.
// ---------------------------------------------------------------------------------------------

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-sync-{tag}-{}-{n}", std::process::id()));
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

/// A minimal harness stub — the engine reads the placement from `map.json`, never the adapter, so these
/// methods are never reached during a pull (and `add` of a plain dir does not recognize it).
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
        _d: Option<&DiscoveredPlacement>,
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
        report()
    }

    fn remove(&self) -> TriggerReport {
        report()
    }

    fn artifacts(&self) -> Vec<TriggerArtifact> {
        Vec::new()
    }

    fn present(&self) -> bool {
        !self.artifacts().is_empty()
    }
}
fn report() -> TriggerReport {
    TriggerReport {
        agent: "claude-code".to_owned(),
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test".into(),
        state: TriggerState::Inactive,

        note: None,
    }
}

#[derive(Default)]
struct FixturePlane {
    records: HashMap<String, WireCurrentRecord>,
    versions: HashMap<(String, String), crate::plane::FetchedVersion>,
}
impl FixturePlane {
    fn set_current(&mut self, skill: &str, rec: WireCurrentRecord) {
        self.records.insert(skill.to_owned(), rec);
    }
    fn add_version(&mut self, skill: &str, v: &Version) {
        self.versions
            .insert((skill.to_owned(), to_hex(&v.id)), v.fetched.clone());
    }
}
impl PlaneSource for FixturePlane {
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        let Some(rec) = self.records.get(skill_id) else {
            return Err(PlaneError::NotFound);
        };
        // The conditional GET: 304 only when the client already holds this EXACT (generation, version_id),
        // so a same-generation record naming a different commit is always returned (the tuple-reuse path).
        if let Some(k) = known
            && k.generation == rec.record.generation
            && to_hex(&k.version_id) == rec.record.version_id
        {
            return Ok(PointerFetch::NotModified);
        }
        Ok(PointerFetch::Record(rec.clone()))
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, PlaneError> {
        self.versions
            .get(&(skill_id.to_owned(), to_hex(&version_id)))
            .cloned()
            .ok_or(PlaneError::NotFound)
    }
}

struct FixtureFollow {
    entries: Vec<(String, FollowContext)>,
}
impl FollowSource for FixtureFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        self.entries.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Version construction + signing.
// ---------------------------------------------------------------------------------------------

struct Version {
    id: [u8; 32],
    fetched: crate::plane::FetchedVersion,
}

fn mk_version(
    parents: &[[u8; 32]],
    files: &[(&str, FileMode, &[u8])],
    author: &str,
    message: &str,
) -> Version {
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|(p, m, b)| ManifestEntry {
            path: (*p).to_owned(),
            mode: *m,
            content_sha256: digest::sha256(b),
        })
        .collect();
    let digest = digest::bundle_digest(&entries).unwrap();
    let id = identity::commit_id(&Commit {
        parents,
        tree: digest,
        author,
        message,
    })
    .unwrap();
    let fetched = crate::plane::FetchedVersion {
        parents: parents.to_vec(),
        author: author.to_owned(),
        message: message.to_owned(),
        files: files
            .iter()
            .map(|(p, m, b)| crate::plane::FetchedFile {
                path: (*p).to_owned(),
                mode: *m,
                bytes: b.to_vec(),
            })
            .collect(),
    };
    Version { id, fetched }
}

/// An unsigned `current` record for the given scope + version + generation (the plane serves these; the
/// engine scope-checks them and re-verifies the fetched bytes against the version id).
fn served(ws: &str, skill: &str, version_id: [u8; 32], generation: u64) -> WireCurrentRecord {
    WireCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: ws.to_owned(),
            skill_id: skill.to_owned(),
        },
        record: CurrentRecord {
            version_id: to_hex(&version_id),
            generation,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The rig: a topos home + a workspace dir holding the adopted skill.
// ---------------------------------------------------------------------------------------------

struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: NoHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1),
            harness: NoHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    fn ctx<'a>(&'a self, plane: &'a dyn PlaneSource, follow: &'a dyn FollowSource) -> Ctx<'a> {
        self.ctx_fs(&self.fs, plane, follow)
    }
    /// A [`Ctx`] over an arbitrary LAYOUT — the hostile-checkout tests drive a PROJECT store,
    /// whose `.topos/` (and therefore every document a resolution reads) travels with the clone.
    fn ctx_at<'a>(
        &'a self,
        layout: Layout,
        plane: &'a dyn PlaneSource,
        follow: &'a dyn FollowSource,
    ) -> Ctx<'a> {
        Ctx {
            layout,
            ..self.ctx_fs(&self.fs, plane, follow)
        }
    }
    /// A [`Ctx`] over an arbitrary [`FsOps`] (the crash gate injects a [`FaultFs`]).
    fn ctx_fs<'a>(
        &'a self,
        fs: &'a dyn FsOps,
        plane: &'a dyn PlaneSource,
        follow: &'a dyn FollowSource,
    ) -> Ctx<'a> {
        Ctx {
            progress: crate::progress::silent(),
            fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE.to_owned(),
            layout: self.layout(),
            harness: &self.harness,
            triggers: crate::ops::Triggers::active_only(&self.harness),
            plane,
            follow,
            roots: None,
        }
    }
    /// Adopt a skill from the work dir (returns its id, name, and genesis version id).
    fn adopt(&self, base: &[(&str, FileMode, &[u8])]) -> (String, String, [u8; 32]) {
        let dir = self.work.0.join("pr-describe");
        write_tree(&dir, base);
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = self.ctx(&inert_p, &inert_f);
        let data = ops::add(&ctx, &dir).unwrap();
        let genesis = ops::parse_hex32(data.version_id.as_deref().unwrap()).unwrap();
        (data.skill_id.unwrap(), data.name, genesis)
    }
    fn placement(&self) -> PathBuf {
        self.work.0.join("pr-describe")
    }
    fn read_sync(&self, id: &str) -> SyncState {
        doc::read_doc(&self.fs, &self.layout().published(&sid(id)).sync)
            .unwrap()
            .unwrap()
    }
    fn patch_sync(&self, id: &str, f: impl FnOnce(&mut SyncState)) {
        let mut s = self.read_sync(id);
        f(&mut s);
        doc::write_doc(&self.fs, &self.layout().published(&sid(id)).sync, &s).unwrap();
    }
    fn patch_map(&self, id: &str, f: impl FnOnce(&mut topos_types::persisted::PlacementMap)) {
        let p = self.layout().published(&sid(id)).map;
        let mut m = doc::read_map(&self.fs, &p).unwrap().unwrap();
        f(&mut m);
        doc::write_map(&self.fs, &p, &m).unwrap();
    }
    fn patch_lock(&self, id: &str, f: impl FnOnce(&mut topos_types::persisted::Lock)) {
        let p = self.layout().published(&sid(id)).lock;
        let mut l: topos_types::persisted::Lock = doc::read_doc(&self.fs, &p).unwrap().unwrap();
        f(&mut l);
        doc::write_doc(&self.fs, &p, &l).unwrap();
    }
    fn open_store(&self, id: &str) -> topos_gitstore::Store {
        topos_gitstore::Store::open(&self.layout().published(&sid(id)).store).unwrap()
    }
    fn conflict_exists(&self, id: &str) -> bool {
        self.layout().published(&sid(id)).conflict.exists()
    }
    fn conflict_state(&self, id: &str) -> topos_types::persisted::ConflictState {
        doc::read_doc(&self.fs, &self.layout().published(&sid(id)).conflict)
            .unwrap()
            .expect("a recorded conflict")
    }
    /// Where the marked-up copy of a recorded conflict lives — read from the record itself, so the
    /// test asserts against the path the code actually documented, never a guess.
    fn conflict_copy(&self, id: &str) -> PathBuf {
        self.layout().conflict_copy_dir(
            &crate::sidecar::ConflictDir::parse(
                self.conflict_state(id).copy_dir.as_deref().unwrap(),
            )
            .expect("a recorded workbench component parses"),
        )
    }
}

/// Parse a rig-minted skill id through the validated newtype (always charset-clean here).
fn sid(id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(id).expect("rig skill id is charset-clean")
}

/// The test shim over [`ops::pull`]: project the schema payload (the envelope warnings have their own
/// dedicated tests below).
fn pull_data(ctx: &Ctx<'_>, scope: ops::PullScope) -> Result<PullData, crate::error::ClientError> {
    ops::pull(ctx, scope).map(|o| o.data)
}

fn follow(skill_id: &str) -> FixtureFollow {
    FixtureFollow {
        entries: vec![(
            skill_id.to_owned(),
            FollowContext {
                workspace_id: WS.to_owned(),
                review_required: false,
                following: true,
            },
        )],
    }
}

fn write_tree(dir: &Path, files: &[(&str, FileMode, &[u8])]) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    for (p, m, b) in files {
        let dest = dir.join(p);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b).unwrap();
        let mode = if *m == FileMode::Executable {
            0o755
        } else {
            0o644
        };
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn snapshot(dir: &Path) -> Option<Vec<(String, Vec<u8>)>> {
    if !dir.exists() {
        return None;
    }
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push((
                    p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    Some(out)
}

/// A static file set as a [`topos_gitstore::RenderedBundle`] — the shape the in-memory merge
/// preview takes for the two published sides it compares.
fn rendered(files: &[(&str, FileMode, &[u8])]) -> topos_gitstore::RenderedBundle {
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|(p, m, b)| ManifestEntry {
            path: (*p).to_owned(),
            mode: *m,
            content_sha256: digest::sha256(b),
        })
        .collect();
    topos_gitstore::RenderedBundle {
        files: files
            .iter()
            .map(|(p, m, b)| topos_gitstore::RenderedFile {
                path: (*p).to_owned(),
                mode: *m,
                bytes: b.to_vec(),
                content_sha256: digest::sha256(b),
            })
            .collect(),
        bundle_digest: digest::bundle_digest(&entries).unwrap(),
    }
}

/// The all-zero sentinel a never-received baseline carries for its commit ids.
fn zero_hex() -> String {
    "0".repeat(64)
}

fn expect(files: &[(&str, FileMode, &[u8])]) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(p, _, b)| ((*p).to_owned(), b.to_vec()))
        .collect();
    v.sort();
    v
}

fn only(data: &PullData) -> &topos_types::results::PullSkill {
    assert_eq!(data.skills.len(), 1, "expected exactly one skill row");
    &data.skills[0]
}

const BASE: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# v0\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
];
const V1: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# v1\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
    ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
];
/// A draft over [`BASE`] that rewrites `SKILL.md` and nothing else — the shape every
/// conflict-with-[`V1`] fixture below uses.
const MINE_OVER_BASE: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# mine\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
];
/// What `--keep-mine` commits for [`MINE_OVER_BASE`] against [`V1`]: this person's `SKILL.md` (the
/// one file both sides rewrote), plus everything V1 changed that they did not touch — `run.sh`
/// caught up, and the file V1 added. `git merge -X ours`, not `-s ours`.
const KEPT_OVER_V1: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# mine\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
    ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
];

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn clean_follower_auto_fast_forwards() {
    let rig = Rig::new("ff");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    let row = only(&data);
    assert_eq!(row.action, PullAction::FastForwarded);
    assert_eq!(row.applied, 1);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "new bytes placed"
    );
    // The executable bit is part of the consent-bound digest and must survive.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(rig.placement().join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, s.observed);
    assert_eq!(s.base_commit, to_hex(&v1.id));
}

/// **A manifest row IS the consent.** A bundle this machine's recipe demands but has never
/// received places its first bytes on the BARE sweep — no offer, no second command — and the row
/// reads `installed`, naming the folder it landed in. The next bare sweep then has nothing to do.
#[test]
fn a_never_received_bundle_installs_on_the_bare_sweep() {
    let rig = Rig::new("first-receive");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    // Roll the sidecar back to the never-received baseline a brand-new arrival gets: nothing
    // applied, the all-zero base, and no bytes on disk yet.
    std::fs::remove_dir_all(rig.placement()).unwrap();
    rig.patch_sync(&id, |s| {
        s.observed = 0;
        s.observed_version_id = zero_hex();
        s.applied = 0;
        s.base_commit = zero_hex();
        s.work_hash = zero_hex();
    });
    rig.patch_lock(&id, |l| {
        l.base_commit = zero_hex();
        l.bundle_digest = zero_hex();
    });

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // ONE bare sweep. The bytes are on disk when it returns.
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Installed,
        "a first receive installs on the bare sweep"
    );
    assert_eq!(row.applied, 1);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the first bytes landed without a second command"
    );
    assert_eq!(
        row.destinations.len(),
        1,
        "an installed row names where the bytes went: {:?}",
        row.destinations
    );
    assert!(
        row.destinations[0].ends_with("pr-describe"),
        "{:?}",
        row.destinations
    );
    assert_eq!(rig.read_sync(&id).applied, 1);

    // Nothing is left waiting on a person: the next bare sweep has nothing to do.
    let again = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&again).action, PullAction::UpToDate);
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
}

/// The in-memory merge PREVIEW over an OVERLAPPING edit predicts the conflict and names the
/// conflicting path, while running nothing: no markers on disk, no conflict record. (The preview
/// is what `publish`'s describe shows an author whose copy is behind `current`.)
#[test]
fn the_merge_preview_names_a_conflicting_path_without_running_the_merge() {
    let rig = Rig::new("preview");
    let (id, _name, _genesis) = rig.adopt(BASE);
    // The same overlap the auto-sweep conflict test uses: SKILL.md edited on both sides.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let mine = crate::scan::scan(&rig.placement()).unwrap();

    let preview = ops::merge_resolve::preview_merge(&rendered(BASE), &mine, &rendered(V1));
    assert_eq!(
        preview.verdict,
        topos_types::results::MergePreviewVerdict::Conflicted
    );
    assert_eq!(preview.conflicts, vec!["SKILL.md".to_owned()]);
    // Predicted, never run: the placement still holds the author's bytes (no markers) and no
    // conflict record exists.
    let skill = std::fs::read_to_string(rig.placement().join("SKILL.md")).unwrap();
    assert!(!skill.contains("<<<<<<<"), "{skill}");
    assert!(!rig.conflict_exists(&id));
}

/// **A folder an agent reads always holds one coherent, complete bundle.** An AUTO follower's bare
/// sweep RESOLVES a diverged draft; here the local edit overlaps theirs' edit to `SKILL.md`, so the
/// merge conflicts — and the conflict writes NOTHING into the agent-readable placement. The folder
/// keeps the author's own version, byte for byte, exactly as it stood before the update. The
/// complete marked-up tree (markers carrying BOTH sides, the other files merged clean) goes to the
/// scope's own workbench, `~/.topos/conflicts/<name>/`, where a person resolves it by hand.
#[test]
fn a_conflict_marks_up_a_sidecar_copy_and_leaves_every_agent_folder_alone() {
    let rig = Rig::new("diverge");
    let (id, _name, genesis) = rig.adopt(BASE);
    // Edit SKILL.md (overlaps theirs' SKILL.md edit → a conflict) and leave run.sh at base.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let before = snapshot(&rig.placement());

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);

    // Resolved (not merely surfaced): a conflict, with a merge report listing the conflicting path.
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(!mr.clean);
    assert_eq!(mr.theirs_version_id, to_hex(&v1.id));
    assert!(mr.conflicts.iter().any(|c| c.path == "SKILL.md"));

    // THE POINT: the placement is byte-identical to what stood there before the update — the
    // author's own version, no markers, no half-state, nothing an agent could read and act on.
    assert_eq!(
        snapshot(&rig.placement()),
        before,
        "a conflict must not write into a folder an agent reads"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(edited)));

    // The COMPLETE conflict tree is in the sidecar, at the path the record names: SKILL.md has
    // diff3 markers carrying BOTH sides; the non-overlapping files are merged clean (run.sh →
    // theirs, the new ref/notes.md → theirs).
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        copy,
        rig.layout().home().join("conflicts").join("pr-describe"),
        "the copy is keyed by the bundle NAME, under the scope's own store"
    );
    let skill = std::fs::read_to_string(copy.join("SKILL.md")).unwrap();
    assert!(
        skill.contains("<<<<<<<") && skill.contains(">>>>>>>"),
        "{skill}"
    );
    assert!(
        skill.contains("my local edit") && skill.contains("# v1"),
        "the edit must survive inside the markers: {skill}"
    );
    assert_eq!(
        std::fs::read(copy.join("run.sh")).unwrap(),
        b"#!/bin/sh\necho v1\n"
    );
    assert!(copy.join("ref/notes.md").exists());
    // The row carries both halves of that truth, so the receipt can name them without re-deriving
    // either: the folder that still holds the author's version, and the folder the record names.
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    assert_eq!(
        mr.placements
            .iter()
            .map(|p| (real(std::path::Path::new(&p.dir)), p.holds))
            .collect::<Vec<_>>(),
        vec![(
            real(&rig.placement()),
            topos_types::results::ConflictHolds::Yours
        )],
        "the untouched placement is named on the row, and says what it holds"
    );
    assert_eq!(
        mr.copy_dir
            .as_deref()
            .map(|d| real(std::path::Path::new(d))),
        Some(real(&copy))
    );
    assert_eq!(
        row.scope.as_deref(),
        Some("person"),
        "the machine's own store — its exits are spelled with `-g`"
    );
    // The copy IS the recorded conflict tree — the digest the escape reads as "untouched".
    let scanned = crate::scan::scan(&copy).unwrap();
    assert_eq!(
        to_hex(&scanned.bundle_digest),
        rig.conflict_state(&id).conflicted_digest
    );

    // Never clobbered: the pre-merge draft is snapshotted into the sidecar store (recoverable).
    let draft = mk_version(&[genesis], edited, DEVICE, "topos: draft snapshot");
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid(&id)).store).unwrap();
    assert!(
        store.list_versions().unwrap().contains(&draft.id),
        "the diverged draft must be snapshotted by the merge"
    );

    // A durable conflict record blocks publish; the pending update is consumed into the (blocked)
    // draft, and the lock now names the team's version as the base a `--reset` restores.
    assert!(rig.conflict_exists(&id));
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, 1);
    assert_eq!(s.base_commit, to_hex(&v1.id));
}

/// The blocked row promises the reader that a named folder still holds their version, so it may
/// only name a folder it LOOKED AT. The placement map is what was recorded when the block was
/// raised; a folder deleted since is still in it, and reading the promise off the map alone keeps
/// asserting a directory that is gone. The row already owns the words for holding nothing, and
/// both of its exits put the bytes back.
#[test]
fn a_re_disclosed_block_never_names_a_folder_that_is_no_longer_there() {
    let rig = Rig::new("gone");
    let (id, _name, genesis) = rig.adopt(BASE);
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The block is raised, and the folder that holds this person's version is named on it.
    let raised = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&raised);
    assert_eq!(row.action, PullAction::Conflicted);
    assert_eq!(
        row.merge.as_ref().expect("a merge report").placements.len(),
        1
    );

    // The person deletes it while the merge stands. The record is untouched — nothing about the
    // merge changed — but the promise is no longer true.
    std::fs::remove_dir_all(rig.placement()).unwrap();

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "the block itself is unaffected"
    );
    assert!(
        row.merge
            .as_ref()
            .expect("a merge report")
            .placements
            .is_empty(),
        "{:?}",
        row.merge
    );
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0);
    assert!(
        tty.contains(
            "    no agent folder holds this skill right now — either way out below puts it back\n"
        ),
        "{tty}"
    );
    assert!(!tty.contains("your agents are unaffected"), "{tty}");
}

#[test]
fn go_back_then_resume() {
    let rig = Rig::new("goback");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Fast-forward to v1.
    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    // Go back to genesis: old bytes installed, `held` set, the floor (`observed`) untouched.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(genesis)),
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "old bytes restored"
    );
    let s = rig.read_sync(&id);
    assert!(s.held, "held set");
    assert_eq!(s.observed, 1, "floor NOT lowered");
    assert_eq!(s.applied, 0, "applied dropped to the old gen");

    // A held skill is NOT auto-fast-forwarded by the sweep.
    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "hold suppresses auto-FF"
    );

    // A bare explicit `pull <skill>` resumes (clears the hold) and fast-forwards back to v1.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "resumed to v1"
    );
    assert!(!rig.read_sync(&id).held);
}

#[test]
fn pull_name_fallback_reaches_a_skill_literally_named_with_a_hex_at_suffix() {
    let rig = Rig::new("atname");
    // Adopt a skill whose NAME looks exactly like a go-back target (a name is a directory basename —
    // only the skill ID charset forbids `@`).
    let dir = rig.work.0.join("docs@abcdef12");
    write_tree(&dir, BASE);
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let added = ops::add(&rig.ctx(&inert_p, &inert_f), &dir).unwrap();
    assert_eq!(added.name, "docs@abcdef12");

    // The go-back parse tries the pre-@ name (`docs`) first, finds no tracked skill, and retries the
    // WHOLE argument as the name — the skill is reachable, never shadowed by the suffix parse.
    let out = crate::app::pull_with_name_fallback(
        &rig.ctx(&inert_p, &inert_f),
        Some("docs@abcdef12".to_owned()),
        false,
        ops::StoreScope::Here,
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1, "the @-named skill resolved");

    // Neither interpretation tracked → the typed NoSuchSkill names the FULL argument the user typed.
    let err = match crate::app::pull_with_name_fallback(
        &rig.ctx(&inert_p, &inert_f),
        Some("nope@abcdef12".to_owned()),
        false,
        ops::StoreScope::Here,
    ) {
        Ok(_) => panic!("an untracked name must not resolve"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, crate::error::ClientError::NoSuchSkill { name } if name == "nope@abcdef12"),
        "got {err:?}"
    );
}

#[test]
fn pull_name_fallback_keeps_the_go_back_primary() {
    // The go-back interpretation still wins when the pre-@ name IS tracked — same shape as
    // `go_back_then_resume`, but driven through the app-level fallback entry point.
    let rig = Rig::new("atgoback");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    let out = crate::app::pull_with_name_fallback(
        &rig.ctx(&plane, &foll),
        Some(format!("{name}@{}", to_hex(&genesis))),
        false,
        ops::StoreScope::Here,
    )
    .unwrap();
    assert_eq!(out.data.skills[0].action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "the go-back landed the old bytes"
    );
}

#[test]
fn go_back_resolves_a_unique_short_prefix_and_refuses_a_no_match() {
    // Same shape as `go_back_then_resume`, but the target rides as a pasted 12-char short form — the
    // exact string every TTY surface renders — resolved against the skill's recorded history.
    let rig = Rig::new("gobackprefix");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Prefix(to_hex(&genesis)[..12].into())),
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "the short prefix installed the same bytes the full id would"
    );

    // A prefix matching nothing in the recorded history is the SAME typed error an unknown full id
    // reports — never a fabricated floor, never a silent name fallback.
    let ctx = rig.ctx(&plane, &foll);
    let err = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Prefix("ffffffffffff".into())),
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "UNKNOWN_GOBACK_VERSION");
}

#[test]
fn server_restore_backward_move_applies() {
    // The served record IS the sync target: after a DB restore the plane re-serves an EARLIER
    // (generation, version_id) than the client last observed. That is a legitimate team rollback now — the
    // client silently applies TOWARD it (a clean follower always applies), never refusing it as a downgrade.
    let rig = Rig::new("restore");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(
        &id,
        &Version {
            id: genesis,
            fetched: crate::plane::FetchedVersion {
                parents: Vec::new(),
                author: DEVICE.to_owned(),
                message: "topos: publish".to_owned(),
                files: BASE
                    .iter()
                    .map(|(p, m, b)| crate::plane::FetchedFile {
                        path: (*p).to_owned(),
                        mode: *m,
                        bytes: b.to_vec(),
                    })
                    .collect(),
            },
        },
    );
    // The client had applied v1 @ (1,2); the plane is then restored and re-serves genesis @ (1,1).
    plane.set_current(&id, served(WS, &id, v1.id, 2));
    let foll = follow(&id);
    {
        let ctx = rig.ctx(&plane, &foll);
        pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    }
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    // The restore: the served target moves BACKWARD to genesis @ (1,1). The client applies toward it.
    plane.set_current(&id, served(WS, &id, genesis, 1));
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "applied the restored (earlier) target — a legitimate team rollback"
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.observed, 1);
    assert_eq!(s.observed_version_id, to_hex(&genesis));
    assert_eq!(s.applied, s.observed);
}

#[test]
fn mis_scoped_pointer_is_a_wire_error() {
    // A served record scoped to ANOTHER workspace (even for the same skill id) is a malformed response, not
    // the sync target — a targeted pull surfaces it as a wire-validation error, and nothing is applied.
    let rig = Rig::new("xws");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served("w_other", &id, v1.id, 1)); // wrong workspace scope

    let foll = follow(&id);
    let ctx = rig.ctx(&plane, &foll);
    let err = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "CORRUPT_STATE",
        "a mis-scoped record is a wire error"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(BASE)), "untouched");
    assert_eq!(rig.read_sync(&id).observed, 0, "target not advanced");
}

#[test]
fn crash_after_swap_heals_without_false_divergence() {
    // The bytes were swapped to v1 but `applied` never advanced (a crash between the swap and the sync
    // write). The next pull must HEAL forward (advance `applied`), never show a false DIVERGED panel.
    let rig = Rig::new("heal");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    // Simulate the post-swap, pre-commit state: placement holds v1 bytes; sync says observed=(1,1) naming
    // v1 but applied still (0,0).
    write_tree(&rig.placement(), V1);
    rig.patch_sync(&id, |s| {
        s.observed = 1;
        s.observed_version_id = to_hex(&v1.id);
        s.applied = 0;
        // base/work still describe genesis (the docs never advanced).
    });

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::FastForwarded,
        "healed, not merged as a false divergence"
    );
    assert!(row.merge.is_none(), "a heal runs no merge");
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    assert_eq!(rig.read_sync(&id).applied, 1);
}

#[test]
fn an_accept_applies_a_version_that_moved_during_the_pull() {
    // The plane advances to v2 between the sweep that landed v1 and the targeted accept. The accept
    // applies v2 in that same call: the row demanding the bundle already consented, so a version
    // this call discovered is not held back for a second command.
    let rig = Rig::new("moved");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2files: &[(&str, FileMode, &[u8])] = &[("SKILL.md", FileMode::Regular, b"# v2\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    // The sweep lands v1.
    {
        let ctx = rig.ctx(&plane, &foll);
        let d = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
        assert_eq!(only(&d).action, PullAction::FastForwarded);
    }
    // The plane moves to v2 before the targeted accept runs.
    plane.set_current(&id, served(WS, &id, v2.id, 2));
    let ctx = rig.ctx(&plane, &foll);
    let d = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    let row = only(&d);
    assert_eq!(
        row.action,
        PullAction::FastForwarded,
        "a version discovered during the accept is applied, not deferred"
    );
    assert_eq!(row.applied, 2);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(v2files)),
        "v2's bytes are on disk"
    );
}

#[test]
fn go_back_snapshots_an_unsaved_draft_before_overwriting() {
    // The never-clobber rail applies to go-back too: an explicit `pull <skill>@<old>` over an EDITED
    // placement must snapshot the draft into the sidecar store FIRST, so the unsaved edits stay recoverable.
    let rig = Rig::new("goback-draft");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    // Fast-forward to v1 (so v1 is in the store + recorded; the placement is clean at v1).
    {
        let ctx = rig.ctx(&plane, &foll);
        pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    }
    // Edit the placement → an unsaved local draft on top of v1.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my unsaved edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&rig.placement(), edited);
    // The draft snapshot the engine must make: a commit on the current base (v1) carrying the edited bytes.
    let draft = mk_version(&[v1.id], edited, DEVICE, "topos: draft snapshot");

    // Go back to genesis.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(genesis)),
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "old bytes installed"
    );
    // CRITICAL: the unsaved draft was snapshotted into the store BEFORE the overwrite — it is recoverable.
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid(&id)).store).unwrap();
    assert!(
        store.list_versions().unwrap().contains(&draft.id),
        "the unsaved draft must be snapshotted before a go-back overwrites it"
    );
}

/// A plane that returns a structurally-malformed response (a corrupt/forged record or bytes).
struct MalformedPlane;
impl PlaneSource for MalformedPlane {
    fn get_current(&self, _: &str, _: Option<KnownCurrent>) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::Malformed("corrupt current record".into()))
    }
    fn fetch_version(
        &self,
        _: &str,
        _: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, PlaneError> {
        Err(PlaneError::Malformed("corrupt version bytes".into()))
    }
}

#[test]
fn malformed_plane_response_is_a_wire_error() {
    // A structurally-malformed served response cannot be the sync target — a targeted pull surfaces it as a
    // wire-validation error (content addressing is the integrity story; a garbled body is simply refused).
    let rig = Rig::new("malformed");
    let (id, name, _genesis) = rig.adopt(BASE);
    let plane = MalformedPlane;
    let foll = follow(&id);
    let ctx = rig.ctx(&plane, &foll);
    let err = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "CORRUPT_STATE",
        "a malformed response is a wire error"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "nothing applied"
    );
    assert_eq!(rig.read_sync(&id).observed, 0);
}

// =================================================================================================
// Author-side merge resolution (the diff3 increment): clean merge, the fixpoint, the targeted accept,
// the escape, conflict-blocks-publish, no-base, structural author-only, binary sidecars, and the crash
// gate. These drive the full resolve through the public `ops::pull` entry point against a real store.
// =================================================================================================

/// A static bundle fixture (path, mode, bytes).
type FileSet = &'static [(&'static str, FileMode, &'static [u8])];

/// Three single-file versions whose edits are on disjoint lines → a clean three-way merge.
fn clean_trio() -> (FileSet, FileSet, FileSet) {
    (
        &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")], // base
        &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")],  // mine (edited line 1)
        &[("SKILL.md", FileMode::Regular, b"line1\nline2\nTHEIRS\n")], // theirs (edited line 3)
    )
}

/// A clean three-way merge: an AUTO follower's bare sweep combines both edits into a draft-on-current —
/// `applied == observed`, `base == theirs`, no conflict record, publishable.
#[test]
fn auto_sweep_clean_merge_lands_draft_on_current() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("clean");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Merged);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(mr.clean);
    assert_eq!(mr.theirs_version_id, to_hex(&v1.id));

    // Both edits are combined on disk; nothing is a conflict marker.
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"MINE\nline2\nTHEIRS\n"
    );
    assert!(
        !rig.conflict_exists(&id),
        "a clean merge writes no conflict record"
    );

    // draft-on-current: the pending update is consumed; the working tree reads as a draft on `current`.
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, 1);
    assert_eq!(s.base_commit, to_hex(&v1.id));
}

/// The clean merge is a stable fixpoint: a re-pull with `current` unchanged is a no-op (never re-merged,
/// never clobbered); and when `current` moves again the merged draft is re-resolved, NEVER fast-forwarded
/// over — so the author's edit is never lost across rounds.
#[test]
fn clean_merge_is_a_stable_fixpoint_with_no_lost_update() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("fixpoint");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Merged
    );

    // (1) Re-pull, `current` unchanged → UpToDate (the draft is not nagged, not re-merged, not clobbered).
    let again = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&again).action, PullAction::UpToDate);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"MINE\nline2\nTHEIRS\n"
    );

    // (2) `current` moves to v2 (an edit on line 3, disjoint from MINE's line-1 edit with an unchanged
    // line 2 between them so diff3 merges cleanly) → the merged draft re-resolves, NOT a fast-forward.
    let v2files: &[(&str, FileMode, &[u8])] =
        &[("SKILL.md", FileMode::Regular, b"line1\nline2\nV2\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane2 = FixturePlane::default();
    plane2.add_version(&id, &v1);
    plane2.add_version(&id, &v2);
    plane2.set_current(&id, served(WS, &id, v2.id, 2));
    let row =
        only(&pull_data(&rig.ctx(&plane2, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_ne!(
        row.action,
        PullAction::FastForwarded,
        "a fast-forward would clobber the merged draft (lost update)"
    );
    assert_eq!(row.action, PullAction::Merged);
    // MINE's original line-1 edit survived two merge rounds.
    let final_skill = std::fs::read(rig.placement().join("SKILL.md")).unwrap();
    assert!(
        final_skill.starts_with(b"MINE\n"),
        "lost update: {final_skill:?}"
    );
}

/// The TARGETED accept resolves a diverged draft exactly as the bare sweep does — the other arm of
/// the resolve-strategy table — and the in-memory preview called this trio clean before anything
/// ran (the prediction `publish`'s describe shows an author whose copy is behind `current`).
#[test]
fn a_targeted_accept_merges_a_diverged_draft_the_preview_called_clean() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("accept-merge");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The prediction, over already-local bytes and writing none of them.
    let scanned = crate::scan::scan(&rig.placement()).unwrap();
    let preview = ops::merge_resolve::preview_merge(&rendered(base), &scanned, &rendered(theirs));
    assert_eq!(
        preview.verdict,
        topos_types::results::MergePreviewVerdict::Clean
    );
    assert!(preview.conflicts.is_empty());
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(mine)),
        "the preview wrote no byte"
    );

    // The targeted accept runs it.
    let accepted = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(only(&accepted).action, PullAction::Merged);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"MINE\nline2\nTHEIRS\n"
    );
    assert!(!rig.conflict_exists(&id));
}

/// **The escape is `git merge -X ours`, and this is the test that says so.**
///
/// The team's version does THREE things at once: it rewrites the line this person also rewrote
/// (contested), it rewrites a line far away that this person left alone (uncontested), and it adds a
/// file that did not exist before (uncontested). `--keep-mine` must keep this person's wording on the
/// first and take BOTH of the others — which is exactly what a person gets by pulling, editing the
/// contested line back to their wording, and committing.
///
/// The shipped code committed the person's WHOLE folder instead (`-s ours`), so the far-away line
/// reverted and the new file was deleted — silently, and nowhere disclosed. That is the defect.
#[test]
fn keep_mine_keeps_my_side_of_the_collision_and_takes_the_rest() {
    let rig = Rig::new("escape");
    let base: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"top\nmid1\nmid2\nmid3\nmid4\nmid5\nbottom\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    let (id, _name, genesis) = rig.adopt(base);
    // MINE: the top line, and nothing else.
    let mine: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"TOP-mine\nmid1\nmid2\nmid3\nmid4\nmid5\nbottom\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    // THEIRS: the same top line (contested), the bottom line (uncontested), a new file, and a
    // rewrite of a file this person never touched.
    let theirs: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"TOP-theirs\nmid1\nmid2\nmid3\nmid4\nmid5\nBOTTOM-theirs\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The merge stops FIRST — `--keep-mine` finishes a stopped merge, and there is nothing to
    // finish before one has stopped (see `keep_mine_refuses_wherever_no_merge_has_stopped`).
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    let data = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Merged);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(mr.clean);
    assert!(mr.drop_diff.is_some(), "the escape discloses what it drops");
    // WHICH exit finished it — the receipt cannot speak for both at once, and a `drop_diff` cannot
    // tell them apart because both carry one.
    assert_eq!(
        mr.resolved,
        Some(topos_types::results::MergeResolution::KeepMine)
    );

    // THE ASSERTION THIS WHOLE CHANGE EXISTS FOR.
    let resolved: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"TOP-mine\nmid1\nmid2\nmid3\nmid4\nmid5\nBOTTOM-theirs\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "my wording on the contested line; everything else of theirs came with it"
    );
    // And the receipt NAMES what came along, so the fact is readable rather than inferred.
    assert_eq!(
        mr.took,
        vec![
            "SKILL.md".to_owned(),
            "ref/notes.md".to_owned(),
            "run.sh".to_owned()
        ],
        "{mr:?}"
    );

    assert!(!rig.conflict_exists(&id));
    let s = rig.read_sync(&id);
    // The base IS theirs — the commit's one parent — so the plane's lineage fence is satisfied and
    // publishing from here is an ordinary publish.
    assert_eq!(
        s.base_commit,
        to_hex(&v1.id),
        "the escape commits ON the team's version"
    );
    assert_eq!(s.applied, s.observed, "and takes it as applied");
    assert_eq!(s.observed, 1);
    assert!(!s.held);

    // The escape's own commit is a real 1-parent commit on `current` — the history stays a list.
    let m = mk_version(&[v1.id], resolved, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// The structural collisions, one table, each against the git command that produces the same
/// answer. Every one resolves — `--keep-mine` never deadlocks on a file — and every one resolves
/// the way git resolves it, so nothing here is a topos invention a person has to learn.
///
/// `git merge -X ours` is the citation for exactly TWO of these: it settles CONTENT, so a
/// both-added-different-content collision and a binary one come out ours. It settles nothing about
/// the file SET — a modify/delete, a delete/modify, and a disagreeing mode all stop that merge
/// dead, and what takes your side there is the per-file take-ours command. The rows are cited
/// accordingly.
///
/// And each row asserts the RECEIPT, not only the resulting files. A resolution that silently
/// dropped something the team changed produces exactly the file set the person asked for, so the
/// files alone can never catch it: what the row must also carry is the disclosure of what went —
/// `took` for what came over from them, `drop_diff` for what did not.
#[test]
fn keep_mine_settles_every_structural_collision_the_way_git_does() {
    // (label, base, mine, theirs, expected, what the drop disclosure must name)
    let cases: &[(&str, FileSet, FileSet, FileSet, FileSet, &str)] = &[
        (
            // Mine modified, theirs deleted. `-X ours` does not resolve this at all; taking our
            // side is `git checkout --ours doomed.md`, which keeps the file.
            "modify/delete",
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("doomed.md", FileMode::Regular, b"base\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("doomed.md", FileMode::Regular, b"MINE\n"),
            ],
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("doomed.md", FileMode::Regular, b"MINE\n"),
            ],
            "doomed.md",
        ),
        (
            // Mine deleted, theirs modified. `-X ours` does not resolve this either, and
            // `git checkout --ours` cannot: our side has no such file. Taking our side is
            // `git rm gone.md` — it stays gone.
            "delete/modify",
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("gone.md", FileMode::Regular, b"base\n"),
            ],
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("gone.md", FileMode::Regular, b"THEIRS\n"),
            ],
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            "gone.md",
        ),
        (
            // Both added the same path with different content — a CONTENT collision, so
            // `git merge -X ours` settles it on ours.
            "add/add",
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("new.md", FileMode::Regular, b"MINE\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("new.md", FileMode::Regular, b"THEIRS\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("new.md", FileMode::Regular, b"MINE\n"),
            ],
            "new.md",
        ),
        (
            // Identical content, disagreeing modes. Only the executable bit is in dispute, and
            // `-X ours` has no say over it; `git checkout --ours run.sh` restores our bit.
            "add/add, mode only",
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("run.sh", FileMode::Regular, b"#!/bin/sh\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("run.sh", FileMode::Executable, b"#!/bin/sh\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("run.sh", FileMode::Regular, b"#!/bin/sh\n"),
            ],
            "run.sh",
        ),
        (
            // A binary file both sides changed — a CONTENT collision with no hunks to reconcile,
            // which `git merge -X ours` takes whole from ours.
            "binary",
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x01]),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x02]),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x03]),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x02]),
            ],
            "logo.bin",
        ),
    ];

    for (label, base, mine, theirs, want, dropped) in cases {
        let rig = Rig::new(&format!("escape-{}", label.replace(['/', ' ', ','], "-")));
        let (id, _name, genesis) = rig.adopt(base);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        assert_eq!(
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::Conflicted,
            "{label}"
        );
        let escaped = pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .unwrap();
        let row = only(&escaped).clone();
        assert_eq!(row.action, PullAction::Merged, "{label}");
        assert_eq!(snapshot(&rig.placement()), Some(expect(want)), "{label}");
        assert!(!rig.conflict_exists(&id), "{label}");

        // THE RECEIPT. Which exit finished it, what came over from the team (nothing in any of
        // these — every one of them settles the whole collision on this person's side), and the
        // disclosure of what the team wrote that did NOT survive it, naming the file.
        let mr = row
            .merge
            .as_ref()
            .unwrap_or_else(|| panic!("{label}: a merge report"));
        assert_eq!(
            mr.resolved,
            Some(topos_types::results::MergeResolution::KeepMine),
            "{label}"
        );
        assert_eq!(
            mr.took,
            Vec::<String>::new(),
            "{label}: nothing here comes over from the team"
        );
        let dd = mr
            .drop_diff
            .as_deref()
            .unwrap_or_else(|| panic!("{label}: the exit discloses what it drops"));
        assert!(
            dd.contains(dropped),
            "{label}: the receipt never names the team's change it dropped ({dropped}):\n{dd}"
        );
        // And the rendered row says it in the one voice a person reads.
        let rendered = crate::render::pull_tty(&escaped, &[], &[], &[], &[], 0);
        assert!(
            rendered.contains("kept your wording where you both changed the same lines"),
            "{label}: {rendered}"
        );
        assert!(
            !rendered.contains("took everything else the team changed"),
            "{label}: nothing was taken, so nothing may be claimed: {rendered}"
        );
    }
}

/// A hand-edited workbench is committed EXACTLY as the person wrote it — the merge is not re-run
/// over it and nothing of theirs is re-applied. They resolved it; second-guessing their tree would
/// be worse than not asking.
///
/// And the ROW says so. This person took the team's contested line and kept a change of their own
/// somewhere else — the exact tree a "kept your wording where you both changed the same lines"
/// headline would misdescribe, which is what the row said while both exits were told apart by a
/// `drop_diff` they both carry. It also claims no file as taken from the team: every byte here is
/// this person's, and topos never looked at any of it.
#[test]
fn a_hand_resolution_is_committed_exactly_as_written() {
    let rig = Rig::new("escape-reconciled");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // The team's contested line, taken as it stands, plus a change of this person's own elsewhere.
    let reconciled: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nMINE3\n")];
    write_tree(&rig.conflict_copy(&id), reconciled);
    let row = only(
        &pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .unwrap(),
    )
    .clone();
    assert_eq!(row.action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(reconciled)));
    let mr = row.merge.as_ref().expect("a merge report");
    assert_eq!(
        mr.resolved,
        Some(topos_types::results::MergeResolution::ByHand)
    );
    assert!(
        mr.took.is_empty(),
        "nothing in a tree topos never examined is claimable as taken from the team: {:?}",
        mr.took
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.base_commit, to_hex(&v1.id));
    assert_eq!(s.applied, s.observed);
}

/// The no-deadlock guarantee, at its narrowest: a hand resolution needs NEITHER the stored draft
/// nor the fork point, so an exit that always works must not begin by re-rendering them.
///
/// The recorded `draft_commit` here names an object that is not in the store. Reading it up front —
/// only ever to compute what the OTHER exit took from the team — turned a corrupt or pruned draft
/// into a failed escape on the one path that has a complete answer sitting on disk.
#[test]
fn a_hand_resolution_needs_neither_the_draft_nor_the_fork_point() {
    let rig = Rig::new("escape-lazy-draft");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    let reconciled: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nMINE3\n")];
    write_tree(&rig.conflict_copy(&id), reconciled);
    // Point the record's draft at an object nothing ever wrote.
    let path = rig.layout().published(&sid(&id)).conflict;
    let mut cs = rig.conflict_state(&id);
    cs.draft_commit = "ab".repeat(32);
    cs.draft_digest = "cd".repeat(32);
    doc::write_doc(&rig.fs, &path, &cs).unwrap();

    let row = only(
        &pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .expect("the exit that always works, works"),
    )
    .clone();
    assert_eq!(row.action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(reconciled)));
    assert!(!rig.conflict_exists(&id));
}

/// A stopped merge is finished by `--keep-mine` whether or not anything still FOLLOWS the bundle.
///
/// The block lives in this machine's own record and the resolution in its own store; nothing about
/// either is the follow-state's to answer. Unfollowing a bundle that is mid-conflict used to leave
/// it wedged: the exit was routed through the followed-only sync, so it fell through to a read-only
/// "up to date" while `publish` stayed refused by a block nothing could clear.
#[test]
fn keep_mine_finishes_a_stopped_merge_on_a_row_nobody_follows() {
    let rig = Rig::new("escape-unfollowed");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // The follow goes away with the block still standing.
    let unfollowed = InertFollow;
    let row = only(
        &pull_data(
            &rig.ctx(&plane, &unfollowed),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .expect("the escape is plane- and follow-independent"),
    )
    .clone();
    assert_eq!(row.action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(!rig.conflict_exists(&id), "the block is cleared");
    assert_eq!(rig.read_sync(&id).base_commit, to_hex(&v1.id));
}

/// Where `--keep-mine` leaves the bundle, seen from the NEXT update: an ordinary draft on the team's
/// version. Nothing new from the team is a no-op; a newer team version runs a real three-way merge
/// FROM THEIRS — the version the escape's commit is parented on.
///
/// The old shape of this test asserted the opposite (a merge re-run from the PRE-conflict base, with
/// the same conflict raised again forever). That was the loop: the decision never became durable, so
/// every sweep re-asked the question the person had already answered.
#[test]
fn after_keep_mine_the_draft_sits_on_theirs_and_merges_forward_from_it() {
    let rig = Rig::new("keepmine-forward");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    // The same line, differently — a genuine conflict.
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The ordinary sweep conflicts.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    // Keep mine, leaving the workbench alone.
    let keep = ops::PullScope::One {
        store: ops::StoreScope::Here,
        name: "pr-describe".into(),
        workspace: None,
        mode: ops::TargetMode::KeepMine,
    };
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), keep).unwrap()).action,
        PullAction::Merged
    );
    assert!(!rig.conflict_exists(&id));
    assert_eq!(rig.read_sync(&id).base_commit, to_hex(&v1.id));

    // NOTHING NEW FROM THE TEAM: the decision stands and the sweep says so. The old behavior
    // re-raised the very conflict this exit resolved, on every single sweep.
    let quiet =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action;
    assert_eq!(quiet, PullAction::UpToDate, "the decision is durable");
    assert!(!rig.conflict_exists(&id));
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // A NEWER TEAM VERSION: a real three-way merge, from THEIRS (v1) — the version this draft is
    // parented on. v2 touches a line nobody else did, so it merges clean and lands silently.
    let v2files: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nTEAM3\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane2 = FixturePlane::default();
    plane2.add_version(&id, &v1);
    plane2.add_version(&id, &v2);
    plane2.set_current(&id, served(WS, &id, v2.id, 2));
    let merged =
        only(&pull_data(&rig.ctx(&plane2, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(merged.action, PullAction::Merged);
    assert!(merged.merge.as_ref().is_some_and(|m| m.clean));
    assert_eq!(
        merged.merge.as_ref().map(|m| m.base_version_id.clone()),
        Some(to_hex(&v1.id)),
        "the merge's base is the version the escape committed on: {merged:?}"
    );
    // This person's line-1 choice survived; the team's new line-3 landed beside it.
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[(
            "SKILL.md",
            FileMode::Regular,
            b"MINE\nline2\nTEAM3\n"
        )]))
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.base_commit, to_hex(&v2.id));
    assert_eq!(s.applied, s.observed);
}

/// `--keep-mine` FINISHES a stopped merge, and refuses EVERY way there is nothing to finish —
/// including on a row nobody follows.
///
/// Each shape had its own silent wrong answer: on an up-to-date copy and on a plain draft it
/// reported success and did nothing; on a clean copy that was merely behind it APPLIED the team's
/// version — the exact opposite of what the flag says; on a live divergence it committed the draft
/// over changes the merge was about to land. One check, before any of them.
///
/// The last two shapes are the ROUTING half, and they are the ones a followed-only fixture could
/// never catch: the refusal used to live inside the followed-sync path, so a tracked-but-unfollowed
/// row — a local path, a forge import, an unfollowed workspace bundle — never reached it and was
/// answered with a read-only "up to date" instead, `ok: true` and exit 0.
#[test]
fn keep_mine_refuses_wherever_no_merge_has_stopped() {
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    // The team changed line 3 only — nothing collides, so a merge would land both sides.
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nTEAM3\n")];
    let both: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nTEAM3\n")];

    // (label, does the team have a newer version?, is there a local draft?, is the row followed?)
    let shapes: &[(&str, bool, bool, bool)] = &[
        ("up to date", false, false, true),
        ("a plain draft, nothing pending", false, true, true),
        ("behind, clean", true, false, true),
        ("diverged, merge never run", true, true, true),
        ("not followed, clean", false, false, false),
        ("not followed, drafted", false, true, false),
    ];
    for (label, pending, drafted, followed) in shapes {
        let rig = Rig::new(&format!("keepmine-{}", label.replace([' ', ','], "-")));
        let (id, name, genesis) = rig.adopt(base);
        let on_disk: FileSet = if *drafted { mine } else { base };
        if *drafted {
            write_tree(&rig.placement(), mine);
        }
        let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        if *pending {
            plane.add_version(&id, &v1);
            plane.set_current(&id, served(WS, &id, v1.id, 1));
        } else {
            plane.set_current(&id, served(WS, &id, genesis, 0));
        }
        let foll = follow(&id);
        let unfollowed = InertFollow;
        let fsrc: &dyn FollowSource = if *followed { &foll } else { &unfollowed };

        let err = pull_data(
            &rig.ctx(&plane, fsrc),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .expect_err(label);
        assert!(
            matches!(&err, crate::error::ClientError::NoStoppedMerge { skill, .. } if skill == &name),
            "{label}: {err:?}"
        );
        // Nothing moved: no commit, no placement write, no record.
        assert_eq!(snapshot(&rig.placement()), Some(expect(on_disk)), "{label}");
        assert!(!rig.conflict_exists(&id), "{label}");
        assert_eq!(rig.read_sync(&id).base_commit, to_hex(&genesis), "{label}");

        // And the merge the refusal names does exactly what it promised.
        if *pending {
            assert_eq!(
                only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap())
                    .action,
                if *drafted {
                    PullAction::Merged
                } else {
                    PullAction::FastForwarded
                },
                "{label}"
            );
            assert_eq!(
                snapshot(&rig.placement()),
                Some(expect(if *drafted { both } else { theirs })),
                "{label}"
            );
        }
    }
}

/// A conflict blocks publish and the block PERSISTS — a bare re-sweep keeps reporting it (healing a
/// crashed materialize), and editing the working tree does NOT clear it (the guard is presence-based, not
/// a digest/marker scan). Only the escape (or a clean re-merge) clears it.
#[test]
fn conflict_blocks_and_persists_until_escaped() {
    let rig = Rig::new("persist");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Auto sweep → conflict (overlapping SKILL.md) → blocked.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));

    // A bare re-sweep keeps reporting the block (does not silently clear or advance).
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));

    // The author hand-resolves the marked-up copy — the block STILL stands (presence-based, not a
    // marker scan), and the re-sweep leaves the hand resolution exactly as written.
    let copy = rig.conflict_copy(&id);
    write_tree(
        &copy,
        &[
            ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
        ],
    );
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(
        rig.conflict_exists(&id),
        "an edit must not clear the conflict"
    );
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# hand-resolved\n",
        "a re-sweep never clobbers the author's hand resolution"
    );

    // The escape resolves it: the block clears, the copy goes with it, and a publishable
    // draft-on-current results.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id), "the escape clears the block");
    assert!(!copy.exists(), "the marked-up copy goes with the block");
}

/// A marked-up copy that goes missing while the block still stands is RE-RENDERED from the recorded
/// result on the next sweep (the copy is derived state; the store holds it). And a copy nobody has
/// touched still means "keep my version": the escape then commits the merge resolved to this
/// person's side, exactly as if the folder had been sitting there untouched all along.
#[test]
fn a_deleted_conflict_copy_is_re_rendered_and_still_reads_as_untouched() {
    let rig = Rig::new("copy-gone");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    let copy = rig.conflict_copy(&id);
    let tree = snapshot(&copy);
    std::fs::remove_dir_all(&copy).unwrap();

    // The next bare sweep still reports the block AND puts the workbench back, byte for byte.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert_eq!(
        snapshot(&copy),
        tree,
        "the copy is re-rendered from the store"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // Untouched ⇒ the escape keeps the author's version.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!copy.exists());
}

/// **A folder holding topos's OWN marked-up tree is never read as the person's work.** The
/// workbench is where markers go; a placement can still end up holding that exact tree — an
/// install that upgraded mid-merge, a person who copied the workbench over their agent folder —
/// and the escape reads the working tree from the placements. Committing what it found there would
/// publish the conflict markers as this person's version, under a row that says `merged`.
///
/// So the marked-up tree is not a draft: the escape falls back to the recorded draft snapshot (the
/// same answer [`super::super::ops::merge_resolve`] gives when the WORKBENCH is untouched), and the
/// re-disclosure gives that folder its own line instead of calling it newer edits of theirs.
#[test]
fn a_placement_holding_the_marker_tree_is_never_committed_as_the_persons_work() {
    let rig = Rig::new("marker-tree-placement");
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), MINE_OVER_BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // Plant it: the workbench tree, byte for byte (modes included), in the agent folder.
    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (src, dest) = (e.path(), to.join(e.file_name()));
            if src.is_dir() {
                copy_tree(&src, &dest);
            } else {
                std::fs::copy(&src, &dest).unwrap();
            }
        }
    }
    let copy = rig.conflict_copy(&id);
    std::fs::remove_dir_all(rig.placement()).unwrap();
    copy_tree(&copy, &rig.placement());
    assert_eq!(
        to_hex(&crate::scan::scan(&rig.placement()).unwrap().bundle_digest),
        rig.conflict_state(&id).conflicted_digest,
        "the placement really holds the marked-up tree"
    );

    // THE DISCLOSURE: that folder gets its own line — never "your newer edits".
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    assert_eq!(
        row.merge
            .as_ref()
            .expect("a merge report")
            .placements
            .iter()
            .map(|p| p.holds)
            .collect::<Vec<_>>(),
        vec![topos_types::results::ConflictHolds::MarkedUp]
    );
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0);
    let leaf = rig
        .placement()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        tty.lines().any(|l| l.starts_with("      ")
            && l.contains(&leaf)
            && l.ends_with("holds both versions marked up — run one of the ways out below")),
        "{tty}"
    );
    assert!(!tty.contains("newer edits"), "{tty}");

    // THE EXIT: `--keep-mine` commits the recorded DRAFT against the team's version — the ordinary
    // keep-mine result — and every folder converges on it. No markers reach disk, and no marked-up
    // tree reaches the store as a version.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!copy.exists(), "the workbench goes with the block");
}

/// The escape writes its committed bytes over EVERY managed placement, so copies that held
/// DIFFERENT edits are collapsed into one. The recorded-conflict entry runs before the work-tree
/// classification, so the typed competitor freeze never fires here — deliberately, since freezing
/// would deadlock the one exit that always works — and that makes the DISCLOSURE the whole
/// protection: the row names how many folders disagreed and hands back one runnable line per copy
/// that puts those exact bytes back.
#[test]
fn the_escape_discloses_the_divergent_copies_it_collapsed() {
    let rig = Rig::new("escape-collapse");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Sweep → conflict: the placement keeps MINE, the workbench holds the marked-up tree.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // A SECOND managed folder carrying its OWN, different edit: neither copy's bytes are the
    // other's recorded baseline, so they are true competitors.
    let replica = rig.work.0.join("replica");
    let other: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# other\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    add_replica(&rig, &id, &replica, other);

    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    let row = only(&escaped);
    assert_eq!(row.action, PullAction::Merged);
    // The collapse really happened: the untouched workbench means "keep my version", and BOTH
    // folders now hold the resolution — the replica's own edit is gone from disk.
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert_eq!(snapshot(&replica), Some(expect(KEPT_OVER_V1)));

    // …and the row says so, in the receipt's own destination convention.
    let note = row.note.as_deref().expect("the collapse is disclosed");
    let lines: Vec<&str> = note.lines().collect();
    assert_eq!(
        lines[0], "overwrote different edits in 2 folders — restore a copy:",
        "{note}"
    );
    assert_eq!(
        lines.len(),
        3,
        "one recovery line per collapsed copy: {note}"
    );
    // Each recovery line is runnable AS PRINTED and names the folder it came from — and the
    // version it offers is one the store really holds, so the go-back it spells resolves.
    let versions = rig.open_store(&id).list_versions().unwrap();
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    for (line, dir) in lines[1..].iter().zip([rig.placement(), replica.clone()]) {
        let named = line
            .split_once("   (was in ")
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("a named folder: {line}"));
        assert_eq!(real(std::path::Path::new(named)), real(&dir), "{line}");
        let hash = line
            .split_once("pr-describe@")
            .and_then(|(_, rest)| rest.split_once("   "))
            .map(|(h, _)| h)
            .unwrap_or_else(|| panic!("a go-back token: {line}"));
        assert!(line.starts_with("  topos update -g pr-describe@"), "{line}");
        assert_eq!(hash.len(), 12, "{line}");
        assert!(
            versions.iter().any(|v| to_hex(v).starts_with(hash)),
            "the offered version is in the store: {line}"
        );
    }
}

/// **A `--dest` reset of ONE copy does not conclude a stopped merge — and the `--keep-mine` the
/// receipt names must then FINISH it, never refuse.** The narrowed reset advances the durable
/// documents to exactly the values a LANDED exit leaves behind — `sync.work_hash` to the reset
/// copy's digest (= base = the conflict's theirs), `map.applied_commit` to `lock.base_commit`
/// (= the conflict's `current_commit`), the map-level `materialized_sha` mirror to the same digest
/// — while another copy still holds the un-merged draft. Any recovery that INFERS "an exit already
/// landed" from those documents deletes the live record and its workbench, refuses
/// `NoStoppedMerge`, and leaves the surviving draft unblocked for `publish` (the record's absence
/// IS the publish guard). Liveness is a recorded fact (the `concluded` mark), never a document
/// comparison.
#[test]
fn a_narrowed_reset_leaves_the_merge_stopped_and_the_escape_still_finishes_it() {
    let rig = Rig::new("narrow-reset-live");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Sweep → a recorded stopped merge; then a SECOND managed copy holding the same un-merged
    // draft (one draft in two folders — never competitors), which is what the narrowed reset
    // leaves standing.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let replica = rig.work.0.join("replica");
    add_replica(&rig, &id, &replica, mine);
    let copy = rig.conflict_copy(&id);

    // The REAL narrowed reset, apply arm: the FIRST copy back to the team's version (the first,
    // because the map-level `materialized_sha` mirrors placement 0 — the exact document shape the
    // deleted inference misread as a landed exit). Named by its recorded spelling.
    let recorded_first = doc::read_map(&rig.fs, &rig.layout().published(&sid(&id)).map)
        .unwrap()
        .unwrap()
        .placements[0]
        .clone();
    let sel = ops::Selection::one(None, Some(&recorded_first));
    ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &sel,
    )
    .unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the named copy is reset to theirs"
    );
    assert_eq!(
        snapshot(&replica),
        Some(expect(mine)),
        "the copy nobody named keeps the draft"
    );
    assert!(
        rig.conflict_exists(&id),
        "a copy still holds the merge, so the record — the publish guard — must stand"
    );
    assert!(copy.exists(), "and the workbench with it");

    // The `--keep-mine` the receipt names FINISHES the stopped merge: the surviving copy's side
    // wins the collided lines, every managed folder converges on the resolution, and only that
    // real conclusion takes the record and the workbench with it.
    let escaped = pull_data(&rig.ctx(&plane, &foll), keep_mine_scope(name)).unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert_eq!(snapshot(&replica), Some(expect(KEPT_OVER_V1)));
    assert!(
        !rig.conflict_exists(&id),
        "publish unblocks only with the real conclusion"
    );
    assert!(!copy.exists());
}

/// **After a narrowed reset, the block's re-disclosure tells PER-FOLDER truth — and the reset's own
/// receipt says the merge is still stopped.** Both surfaces used to speak for the whole set from one
/// fact they never checked: the row promised "your agents are unaffected — N folders still hold your
/// version" over a set that no longer did, and the reset receipt said what it took and stopped,
/// never saying whether the decision was over. This drives the REAL state — a `--dest` reset of one
/// copy of three, with a third edited on afterwards — so every claim is measured against disk.
#[test]
fn a_narrowed_reset_leaves_per_folder_truth_on_both_surfaces() {
    let rig = Rig::new("narrow-reset-truth");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The stop, then two more managed copies of the same un-merged draft.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let untouched = rig.work.0.join("untouched");
    let worked_on = rig.work.0.join("worked-on");
    add_replica(&rig, &id, &untouched, mine);
    add_replica(&rig, &id, &worked_on, mine);

    // Reset the FIRST copy only, and keep working in the third while the merge stands.
    let recorded_first = doc::read_map(&rig.fs, &rig.layout().published(&sid(&id)).map)
        .unwrap()
        .unwrap()
        .placements[0]
        .clone();
    let outcome = ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::one(None, Some(&recorded_first)),
    )
    .unwrap();
    let ops::ResetOutcome::Applied(items) = outcome else {
        panic!("the `--yes` arm applies");
    };
    // THE RESET RECEIPT (D2): the merge it met is still standing, and the receipt says so and
    // points at the surface that answers about it.
    assert_eq!(
        items[0].merge,
        Some(topos_types::results::ResetMergeOutcome::StillStopped),
        "two copies still hold the merge"
    );
    let receipt = crate::render::reset_applied_tty(&items);
    assert!(
        receipt.ends_with(&format!(
            "  the merge on '{name}' is still stopped (see: topos list {name})"
        )),
        "{receipt}"
    );
    write_tree(
        &worked_on,
        &[
            ("SKILL.md", FileMode::Regular, b"# mine, again\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ],
    );

    // THE RE-DISCLOSURE (D1): three folders, three different answers, each read off the folder.
    use topos_types::results::ConflictHolds;
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    let mut held: Vec<(String, ConflictHolds)> = row
        .merge
        .as_ref()
        .expect("a merge report")
        .placements
        .iter()
        .map(|p| (real(std::path::Path::new(&p.dir)), p.holds))
        .collect();
    held.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want = vec![
        (real(&rig.placement()), ConflictHolds::Theirs),
        (real(&untouched), ConflictHolds::Yours),
        (real(&worked_on), ConflictHolds::NewerEdits),
    ];
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(held, want, "{:?}", row.merge);

    // The `--json` half of the same row offers the same two exits, spelled for the row's own
    // scope (this is the machine's store, so `-g`). The TTY named them and the envelope did not.
    let argv = |exit: &str| {
        ["topos", "update", "-g", name.as_str(), exit, "--json"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        crate::render::conflict_next_actions(&data)
            .into_iter()
            .map(|a| a.argv)
            .collect::<Vec<_>>(),
        vec![argv("--keep-mine"), argv("--reset")]
    );

    // And the receipt says the three of them one at a time — never the aggregate sentence, which
    // is false of two of these folders.
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0);
    assert!(
        !tty.contains("your agents are unaffected"),
        "the aggregate promise is false here: {tty}"
    );
    for (dir, said) in [
        (rig.placement(), "holds the team's version"),
        (untouched.clone(), "still holds your version"),
        (worked_on.clone(), "holds your newer edits"),
    ] {
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            tty.lines()
                .any(|l| l.starts_with("      ") && l.contains(&leaf) && l.ends_with(said)),
            "{leaf} → {said}: {tty}"
        );
    }

    // The reset that DOES end it — every remaining copy settled — says so instead, and names no
    // pointer: there is nothing left to decide.
    let ops::ResetOutcome::Applied(last) = ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap() else {
        panic!("the `--yes` arm applies");
    };
    assert_eq!(
        last[0].merge,
        Some(topos_types::results::ResetMergeOutcome::Concluded)
    );
    assert!(!rig.conflict_exists(&id), "the record is gone with it");
    assert!(
        crate::render::reset_applied_tty(&last).ends_with(
            "  that was the last copy holding the merge — the team's version stands everywhere"
        ),
        "{}",
        crate::render::reset_applied_tty(&last)
    );
}

/// `update --reset` in the RECORDED-conflict state resolves it the team's way: the author's draft is
/// snapshotted and discarded, theirs lands on the placement, and the conflict record is CLEARED —
/// with the marked-up copy it named — so publish is not left refused by a divergence that no longer
/// exists, and the next sweep reads the skill current instead of re-disclosing a stale block.
#[test]
fn reset_clears_the_recorded_conflict_block() {
    let rig = Rig::new("reset-clears");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Auto sweep → conflict (overlapping SKILL.md) → blocked.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));
    let copy = rig.conflict_copy(&id);
    assert!(copy.exists(), "the marked-up copy is written");

    // The loss-led discard (`--yes`): theirs restored, the block gone, the copy gone with it.
    ops::reset(
        &rig.ctx(&plane, &foll),
        &[name],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert!(!rig.conflict_exists(&id), "the reset clears the block");
    assert!(!copy.exists(), "and the marked-up copy with it");
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the placement holds the team's bytes after the reset"
    );

    // The next sweep reads the skill current — never a re-disclosed stale conflict.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::UpToDate
    );
}

/// Unrelated histories (no renderable base) fall back to a 2-way manual choice — never a silent merge:
/// MINE is kept on disk, a 2-way diff is disclosed, and publish is blocked until the author resolves.
///
/// The workbench is a WORKBENCH here too: it holds this person's files WITH the team's beside them as
/// `.topos-theirs` siblings, so the by-hand merge the row asks for can actually be done in one folder.
/// Those siblings are topos's own scaffolding — they never become published bundle content — and the
/// exit, `diff`, and `--reset` all work afterwards.
#[test]
fn no_base_falls_back_to_two_way_never_silent() {
    let rig = Rig::new("nobase");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# independent\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    // Sever the recorded base so it cannot be rendered (an unrelated/pruned-base history).
    rig.patch_lock(&id, |l| {
        l.base_commit = "f".repeat(64);
        l.bundle_digest = "e".repeat(64);
    });

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(!mr.clean);
    assert!(mr.drop_diff.is_some(), "a 2-way diff is disclosed");
    // MINE is never silently overwritten by theirs.
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(rig.conflict_exists(&id));

    // BOTH SIDES ARE IN THE WORKBENCH: this person's files at their own paths, the team's beside
    // them. Without them the folder invites a merge while holding only one of the two things to
    // merge.
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# independent\n"
    );
    assert_eq!(
        std::fs::read(copy.join("SKILL.md.topos-theirs")).unwrap(),
        V1[0].2,
        "the team's version is beside this person's"
    );
    assert!(
        copy.join("ref/notes.md.topos-theirs").exists(),
        "every file of theirs, including ones this person does not have"
    );
    // And nothing of the sort reached a folder an agent reads.
    assert!(!rig.placement().join("SKILL.md.topos-theirs").exists());

    // THE EXIT: a hand merge in that folder commits, and topos's own siblings are stripped out of
    // it — a `.topos-theirs` file must never become published bundle content.
    let resolved: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# reconciled\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ("SKILL.md.topos-theirs", FileMode::Regular, V1[0].2),
    ];
    write_tree(&copy, resolved);
    let escaped = only(
        &pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: name.clone(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .unwrap(),
    )
    .clone();
    assert_eq!(escaped.action, PullAction::Merged);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[
            ("SKILL.md", FileMode::Regular, b"# reconciled\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ])),
        "the sibling is scaffolding, never content"
    );
    assert!(!rig.conflict_exists(&id));
    assert!(!copy.exists());

    // AFTERWARDS, the ordinary verbs work: the base renders (it is the team's version now), so
    // `diff` answers and `--reset` restores it.
    let diffed = ops::diff(
        &rig.ctx(&plane, &foll),
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect("diff reads the draft against a renderable base");
    assert!(diffed.diff.contains("reconciled"), "{}", diffed.diff);
    ops::reset(
        &rig.ctx(&plane, &foll),
        &[name],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .expect("the reset restores the team's version");
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
}

/// Structural author-only: the merge code is unreachable from a clean follower state. A behind-clean pull
/// fast-forwards (never merges); a draft with no pending update is a no-op (never merges); neither writes
/// a conflict record nor produces a `Merged`/`Conflicted` outcome.
#[test]
fn merge_unreachable_from_clean_follower_states() {
    // BEHIND (clean): no local edit; a pending update fast-forwards, it does NOT enter the merge.
    {
        let rig = Rig::new("reach-behind");
        let (id, _name, genesis) = rig.adopt(BASE); // placement == base (no edit)
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        let row =
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
        assert_eq!(row.action, PullAction::FastForwarded);
        assert!(row.merge.is_none());
        assert!(!rig.conflict_exists(&id));
    }
    // DRAFT (no pending): a local edit but `current` unchanged → a no-op; never the merge.
    {
        let rig = Rig::new("reach-draft");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(
            &rig.placement(),
            &[("SKILL.md", FileMode::Regular, b"# draft\n")],
        );
        let v0 = mk_version(&[genesis], BASE, "d_pub", "v0"); // not used as a move
        let _ = v0;
        let mut plane = FixturePlane::default();
        // `current` is the genesis the client already has applied → nothing pending.
        plane.set_current(&id, served(WS, &id, genesis, 0));
        let foll = follow(&id);
        let row =
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
        assert!(
            matches!(row.action, PullAction::UpToDate),
            "a draft with no pending update is a no-op, got {:?}",
            row.action
        );
        assert!(row.merge.is_none());
        assert!(!rig.conflict_exists(&id));
    }
}

/// A binary (non-UTF-8) file diverging three ways is never line-merged: theirs is kept at the path
/// and mine in a `.topos-mine` sibling — both inside the CONFLICT COPY, never in a placement, so a
/// `.topos-mine` file can never become publishable bundle content (`publish` ships a placement's
/// bytes). The copy scans back to the recorded conflict digest (the sidecar round-trips through the
/// scanner — the untouched signal stays valid).
#[test]
fn binary_conflict_keeps_both_sides_via_sidecar() {
    let rig = Rig::new("binary");
    // 0xFF is never a valid UTF-8 lead byte → genuinely binary content (so it is never line-merged).
    let base: &[(&str, FileMode, &[u8])] = &[("logo.bin", FileMode::Regular, &[0xffu8, 1, 2])];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: &[(&str, FileMode, &[u8])] = &[("logo.bin", FileMode::Regular, &[0xffu8, 9, 9])];
    write_tree(&rig.placement(), mine);
    let theirs_files: &[(&str, FileMode, &[u8])] =
        &[("logo.bin", FileMode::Regular, &[0xffu8, 7, 7])];
    let v1 = mk_version(&[genesis], theirs_files, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    // The placement keeps MINE whole — no `.topos-mine` sibling, nothing of theirs. That is what
    // makes the sibling unpublishable: `publish` ships a PLACEMENT's bytes.
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    // theirs kept at the path, mine in the sidecar — both inside the conflict copy.
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        std::fs::read(copy.join("logo.bin")).unwrap(),
        &[0xffu8, 7, 7]
    );
    assert_eq!(
        std::fs::read(copy.join("logo.bin.topos-mine")).unwrap(),
        &[0xffu8, 9, 9]
    );
    // The copy scans back to the recorded conflict digest (sidecars survive the scanner).
    let cs: topos_types::persisted::ConflictState =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid(&id)).conflict)
            .unwrap()
            .unwrap();
    let scanned = crate::scan::scan(&copy).unwrap();
    assert_eq!(to_hex(&scanned.bundle_digest), cs.conflicted_digest);
}

/// The release-blocker crash gate: fault every fs op during an auto conflict resolve and assert (a)
/// the agent-readable placement holds the author's OWN complete bytes at EVERY fault — never
/// markers, never a torn tree, never theirs (the guarantee is structural: the conflict path writes
/// no placement at all, so no crash window can put dangerous bytes in a folder an agent reads); (b)
/// a marked-up copy on disk always has its guard record beside it (a marker tree is never
/// publishable); and (c) a clean re-run always converges to the blocked conflict state, with the
/// complete marker tree in the sidecar copy.
#[test]
fn resolve_crash_gate_converges_and_never_writes_markers_into_an_agent_folder() {
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    // Capture the completed conflict copy + op count from a clean run.
    let (conflict_tree, n_ops) = {
        let rig = Rig::new("cg-count");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        let fs = FaultFs::new(0);
        pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
        (snapshot(&rig.conflict_copy(&id)), fs.ops_attempted())
    };
    assert!(n_ops > 4, "expected several durable ops, got {n_ops}");
    assert!(conflict_tree.is_some(), "the clean run writes the copy");

    for fail_at in 1..=n_ops {
        let rig = Rig::new(&format!("cg-{fail_at}"));
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        let copy_root = rig.layout().home().join("conflicts").join("pr-describe");

        // Fault the Nth op (may error mid-resolve).
        let fs = FaultFs::new(fail_at);
        let _ = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed);

        // THE SAFETY PROPERTY: whatever the fault did, the folder an agent reads still holds the
        // author's own complete bytes.
        assert_eq!(
            snapshot(&rig.placement()),
            Some(expect(mine)),
            "fail_at={fail_at}: the agent folder must never hold anything but the author's version"
        );
        // A marked-up copy on disk always has its guard record: the record is written + fsynced
        // before the copy, so this holds at every fault.
        if copy_root.exists() {
            assert!(
                rig.conflict_exists(&id),
                "fail_at={fail_at}: a conflict copy exists without its guard record"
            );
        }

        // A clean re-run converges: blocked conflict, the complete marker tree in the copy,
        // applied == observed — and the agent folder still untouched.
        let row =
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
        assert_eq!(
            row.action,
            PullAction::Conflicted,
            "fail_at={fail_at}: did not converge to a blocked conflict"
        );
        assert!(
            rig.conflict_exists(&id),
            "fail_at={fail_at}: no guard after converge"
        );
        assert_eq!(
            snapshot(&rig.conflict_copy(&id)),
            conflict_tree,
            "fail_at={fail_at}: the conflict copy did not converge to the complete marker tree"
        );
        assert_eq!(
            snapshot(&rig.placement()),
            Some(expect(mine)),
            "fail_at={fail_at}: converging must not touch the agent folder either"
        );
        assert_eq!(rig.read_sync(&id).applied, 1);
    }
}

/// The same gate over the way OUT. The escape commits its resolution, marks the record CONCLUDED,
/// converges the placements, and clears the record last — so a crash mid-exit leaves either a
/// live block (unmarked, nothing placed) or a marked conclusion to finish, and the workbench
/// folder holding a by-hand merge is the only copy of bytes nothing else in the system has
/// written down. Faulting every fs op of `--keep-mine`, both exits, asserts:
///
/// (a) the agent folder always holds ONE complete bundle — the author's own version or the
///     committed resolution, never markers, never a torn tree;
/// (b) while the record still stands — marked or not — the workbench is byte-identical to how
///     the person left it (the clear removes the record FIRST, the folder after — the order that
///     makes a re-run safe);
/// (c) a clean re-run FINISHES a standing record to `Merged`, whether the crash left it unmarked
///     (a live block, escaped afresh) or marked (the crashed exit's own conclusion, finished
///     idempotently — where the deleted document-pair inference refused `NoStoppedMerge` with the
///     record still standing); `NoStoppedMerge` remains only for faults past the record's
///     removal, where the merge had already concluded whole — and the placement holds the
///     expected resolution either way.
#[test]
fn escape_crash_gate_keeps_one_coherent_bundle_and_never_eats_the_hand_merge() {
    let mine: FileSet = MINE_OVER_BASE;
    let hand: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    // Both exits: an UNTOUCHED workbench (keep my version) and a hand-merged one.
    for by_hand in [false, true] {
        let want: FileSet = if by_hand { hand } else { KEPT_OVER_V1 };
        // A clean run first, to size the fault sweep.
        let n_ops = {
            let rig = Rig::new(&format!("eg-count-{by_hand}"));
            let (id, name, genesis) = rig.adopt(BASE);
            write_tree(&rig.placement(), mine);
            let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
            let mut plane = FixturePlane::default();
            plane.add_version(&id, &v1);
            plane.set_current(&id, served(WS, &id, v1.id, 1));
            let foll = follow(&id);
            pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
            if by_hand {
                write_tree(&rig.conflict_copy(&id), hand);
            }
            let fs = FaultFs::new(0);
            pull_data(
                &rig.ctx_fs(&fs, &plane, &foll),
                keep_mine_scope(name.clone()),
            )
            .unwrap();
            fs.ops_attempted()
        };
        assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

        for fail_at in 1..=n_ops {
            let rig = Rig::new(&format!("eg-{by_hand}-{fail_at}"));
            let (id, name, genesis) = rig.adopt(BASE);
            write_tree(&rig.placement(), mine);
            let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
            let mut plane = FixturePlane::default();
            plane.add_version(&id, &v1);
            plane.set_current(&id, served(WS, &id, v1.id, 1));
            let foll = follow(&id);
            pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
            let copy = rig.conflict_copy(&id);
            if by_hand {
                write_tree(&copy, hand);
            }
            let untouched = snapshot(&copy);

            // Fault the Nth op (may error mid-exit).
            let fs = FaultFs::new(fail_at);
            let _ = pull_data(
                &rig.ctx_fs(&fs, &plane, &foll),
                keep_mine_scope(name.clone()),
            );

            // (a) whatever the fault did, the folder an agent reads holds one complete bundle.
            let on_disk = snapshot(&rig.placement());
            assert!(
                on_disk == Some(expect(mine)) || on_disk == Some(expect(want)),
                "by_hand={by_hand} fail_at={fail_at}: the agent folder holds neither the author's \
                 own version nor the resolution: {on_disk:?}"
            );
            // (b) the record still stands ⇒ the workbench is exactly as the person left it.
            if rig.conflict_exists(&id) {
                assert_eq!(
                    snapshot(&copy),
                    untouched,
                    "by_hand={by_hand} fail_at={fail_at}: the workbench moved while the block stood"
                );
            }

            // (c) a clean re-run finishes it. A STANDING record — the live unmarked block, or the
            // marked conclusion a crash stranded — must re-run to `Merged`; the refusal is honest
            // only once the record's removal already landed (the merge concluded whole, and the
            // fault hit the workbench cleanup after it).
            let record_stood = rig.conflict_exists(&id);
            match pull_data(&rig.ctx(&plane, &foll), keep_mine_scope(name.clone())) {
                Ok(d) => assert_eq!(
                    only(&d).action,
                    PullAction::Merged,
                    "by_hand={by_hand} fail_at={fail_at}"
                ),
                Err(crate::error::ClientError::NoStoppedMerge { .. }) => assert!(
                    !record_stood,
                    "by_hand={by_hand} fail_at={fail_at}: a standing record must re-run to Merged, \
                     never be refused away"
                ),
                Err(e) => panic!("by_hand={by_hand} fail_at={fail_at}: {e:?}"),
            }
            assert!(
                !rig.conflict_exists(&id),
                "by_hand={by_hand} fail_at={fail_at}: the block outlived its resolution"
            );
            assert_eq!(
                snapshot(&rig.placement()),
                Some(expect(want)),
                "by_hand={by_hand} fail_at={fail_at}: did not converge to the resolution"
            );
        }
    }
}

/// The `--reset` half of the same gate: this exit DISCARDS, so what it must never do under a fault
/// is leave a folder holding half of anything, or take the workbench with it before the record.
/// The reset marks the record CONCLUDED only once every copy has proven settled, so a standing
/// record after a fault is either still unmarked (a live block, whatever the placements hold) or a
/// marked conclusion whose clear the next run finishes — and in BOTH shapes the workbench must be
/// intact (the removal runs through the record-first clear, which spares an edited folder). A clean
/// re-run always converges on the team's version with no block and no workbench left.
#[test]
fn reset_crash_gate_converges_and_never_takes_the_workbench_before_the_record() {
    let mine: FileSet = MINE_OVER_BASE;
    let n_ops = {
        let rig = Rig::new("rg-count");
        let (id, name, genesis) = reset_conflict_rig(&rig, mine);
        let (mut plane, foll) = (FixturePlane::default(), follow(&id));
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let fs = FaultFs::new(0);
        ops::reset(
            &rig.ctx_fs(&fs, &plane, &foll),
            &[name],
            true,
            ops::StoreScope::Here,
            &ops::Selection::default(),
        )
        .unwrap();
        fs.ops_attempted()
    };
    assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

    for fail_at in 1..=n_ops {
        let rig = Rig::new(&format!("rg-{fail_at}"));
        let (id, name, genesis) = reset_conflict_rig(&rig, mine);
        let (mut plane, foll) = (FixturePlane::default(), follow(&id));
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let copy = rig.conflict_copy(&id);
        let marked = snapshot(&copy);

        let fs = FaultFs::new(fail_at);
        let _ = ops::reset(
            &rig.ctx_fs(&fs, &plane, &foll),
            std::slice::from_ref(&name),
            true,
            ops::StoreScope::Here,
            &ops::Selection::default(),
        );

        // One complete bundle in the agent folder at every fault: the author's draft, or the
        // team's version the reset restores.
        let on_disk = snapshot(&rig.placement());
        assert!(
            on_disk == Some(expect(mine)) || on_disk == Some(expect(V1)),
            "fail_at={fail_at}: the agent folder holds neither side whole: {on_disk:?}"
        );
        // The record goes FIRST, so a standing record — marked concluded or not — means the
        // workbench is still intact.
        if rig.conflict_exists(&id) {
            assert_eq!(
                snapshot(&copy),
                marked,
                "fail_at={fail_at}: the workbench went before the record it belongs to"
            );
        }

        // A clean re-run converges: the team's version, no block, no workbench.
        ops::reset(
            &rig.ctx(&plane, &foll),
            std::slice::from_ref(&name),
            true,
            ops::StoreScope::Here,
            &ops::Selection::default(),
        )
        .unwrap_or_else(|e| panic!("fail_at={fail_at}: the re-run must converge: {e:?}"));
        assert_eq!(
            snapshot(&rig.placement()),
            Some(expect(V1)),
            "fail_at={fail_at}"
        );
        assert!(!rig.conflict_exists(&id), "fail_at={fail_at}");
        // A crash BETWEEN the two removals leaves the folder standing — documented litter, never
        // loss, and the next conflict for this bundle simply takes the next free name. What must
        // hold is that the residue is topos's OWN partial copy of the marked-up tree and nothing
        // else: no byte a person wrote can end up stranded there.
        if let Some(left) = snapshot(&copy) {
            let full = marked
                .clone()
                .expect("the marked-up copy is written at conflict time");
            for (path, bytes) in &left {
                assert!(
                    full.iter().any(|(p, b)| p == path && b == bytes),
                    "fail_at={fail_at}: the leftover holds something topos never wrote: {path}"
                );
            }
        }
    }
}

/// A blocked bundle, ready for a reset: adopt, edit, and let the sweep stop on the overlap.
fn reset_conflict_rig(rig: &Rig, mine: FileSet) -> (String, String, [u8; 32]) {
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    (id, name, genesis)
}

/// `topos update <name> --keep-mine`, as a scope.
fn keep_mine_scope(name: String) -> ops::PullScope {
    ops::PullScope::One {
        store: ops::StoreScope::Here,
        name,
        workspace: None,
        mode: ops::TargetMode::KeepMine,
    }
}

// --- review-driven regression tests ---

/// **The "keep my version" exit.** Leaving the conflict folder alone and running `--keep-mine`
/// commits the merge with this person's side kept on the contested file, never the raw marker tree
/// — otherwise the markers would become a publishable bundle. The folder goes with the block.
#[test]
fn escape_of_unedited_conflict_commits_the_resolution_not_markers() {
    let rig = Rig::new("escape-unedited");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Auto sweep → conflict (overlapping SKILL.md) → the markers are in the CONFLICT COPY, and the
    // placement still holds MINE.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let copy = rig.conflict_copy(&id);
    assert!(
        std::fs::read_to_string(copy.join("SKILL.md"))
            .unwrap()
            .contains("<<<<<<<")
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // Escape WITHOUT touching the folder → commits the resolution, not the markers; clears the
    // block and removes the folder.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id), "escape clears the block");
    assert!(!copy.exists(), "and removes the marked-up copy");
    // The placement holds this person's `SKILL.md` and the team's other changes — no markers
    // anywhere.
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(
        !std::fs::read_to_string(rig.placement().join("SKILL.md"))
            .unwrap()
            .contains("<<<<<<<"),
        "the escape must not commit unresolved markers"
    );
    // The committed escape is a 1-parent commit on `current` carrying that tree — the markers never
    // entered the publishable lineage.
    let m = mk_version(&[v1.id], KEPT_OVER_V1, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// Hand-resolving the CONFLICT FOLDER and then running `--keep-mine` commits those bytes — the
/// author's resolution — onto `current`, and writes them to every managed placement.
#[test]
fn escape_of_edited_conflict_commits_the_resolution() {
    let rig = Rig::new("escape-edited");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The author hand-resolves the marked-up copy (removes markers) and then escapes. The
    // placement is deliberately left alone — the folder is the resolution surface now.
    let copy = rig.conflict_copy(&id);
    let resolved: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&copy, resolved);
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id));
    assert!(!copy.exists(), "the resolved copy is consumed and removed");
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "the escape commits the author's hand resolution and places it"
    );
    // Publishable: the resolution is a real 1-parent commit on `current`.
    let m = mk_version(&[v1.id], resolved, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

// --- the workbench folder is named by parsing, never by trusting ---

/// A minimal PROJECT store holding one blocked bundle: the store tree, a `lock.json`, and
/// (optionally) the `conflict.json` that names the workbench folder. Enough for the ONE act these
/// tests exercise — clearing the block, which is the act a receipt tells the reader to run.
struct HostileCheckout {
    layout: Layout,
    sp: crate::sidecar::SkillPaths,
}

impl HostileCheckout {
    /// `lock.name` / `lock.skill_id` are the two UNTRUSTED strings a clone controls; `record` is
    /// the `conflict.json` `copy_dir` it can also commit (`None` writes no record at all, so the
    /// lock's own strings are what the removal has to name the folder from).
    fn plant(project: &Path, name: &str, skill_id: &str, record: Option<&str>) -> Self {
        let layout = crate::sidecar::project_store_layout(project);
        let sp = layout.published(&sid("topos_conflict1"));
        std::fs::create_dir_all(&sp.store).unwrap();
        let lock = topos_types::persisted::Lock {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            base_commit: "0".repeat(64),
            bundle_digest: "0".repeat(64),
            files: Vec::new(),
        };
        doc::write_doc(&RealFs, &sp.lock, &lock).unwrap();
        if let Some(copy_dir) = record {
            let cs = topos_types::persisted::ConflictState {
                schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
                base_commit: "0".repeat(64),
                base_digest: "0".repeat(64),
                current_commit: "1".repeat(64),
                current_digest: "1".repeat(64),
                draft_commit: "2".repeat(64),
                draft_digest: "2".repeat(64),
                result_commit: "3".repeat(64),
                conflicted_digest: "3".repeat(64),
                copy_dir: Some(copy_dir.to_owned()),
                reason: topos_types::persisted::ConflictReason::ThreeWay,
                concluded: None,
                paths: Vec::new(),
            };
            doc::write_doc(&RealFs, &sp.conflict, &cs).unwrap();
        }
        Self { layout, sp }
    }
}

/// **A hostile checkout must not be able to aim a recursive delete out of its own store.**
///
/// `clear_conflict` is what `--keep-mine` and `--reset` both call — the two commands a conflict
/// receipt tells the reader to run — so a clone that ships `.topos/state/<user>/conflicts` as a
/// SYMLINK pointing at a home directory, plus a `conflict.json` naming a plain component inside
/// it, used to get `~/Documents` deleted recursively: the removal was path-based, and the kernel
/// resolved that symlinked intermediate component normally.
///
/// The removal now descends the same way the WRITE does — a held handle on the store's own
/// `conflicts/` directory, opened `O_NOFOLLOW` — so the swapped component is met as itself and
/// refused. Nothing outside the store is touched.
#[test]
fn a_symlinked_conflicts_component_deletes_nothing_outside_the_store() {
    let rig = Rig::new("hostile-symlink");
    let project = rig.work.0.join("checkout");
    let victim = rig.work.0.join("victim-home");
    let precious = victim.join("Documents");
    std::fs::create_dir_all(precious.join("taxes")).unwrap();
    std::fs::write(precious.join("taxes/2025.pdf"), b"irreplaceable\n").unwrap();

    // The clone's committed state: the store, a record naming a plainly SAFE component, and the
    // `conflicts` component itself swapped for a link to the victim's home.
    let planted = HostileCheckout::plant(
        &project,
        "pr-describe",
        "topos_conflict1",
        Some("Documents"),
    );
    std::fs::create_dir_all(planted.layout.home()).unwrap();
    std::os::unix::fs::symlink(&victim, planted.layout.conflicts_dir()).unwrap();

    let (p, f) = (InertPlane, InertFollow);
    let ctx = rig.ctx_at(planted.layout.clone(), &p, &f);
    let cleared = ops::merge_resolve::clear_conflict(
        &ctx,
        &planted.sp,
        ops::merge_resolve::Workbench::Unread,
    );

    assert!(
        precious.join("taxes/2025.pdf").exists(),
        "nothing outside the store may be deleted"
    );
    assert!(victim.exists() && precious.exists());
    assert!(
        cleared.is_err(),
        "a `conflicts` component that is not a real directory must refuse, not be followed"
    );
}

/// The same rule for the OTHER door into the join: the fallbacks the removal used to derive a
/// folder name from when no record could be read. A clone commits a `lock.json` whose display name
/// sanitizes to nothing (all fullwidth) and whose `skill_id` — a raw string on disk, not the
/// validated newtype — climbs out of the store with `..`. With no `conflict.json` at all, that raw
/// id used to be joined straight onto `conflicts/` and the resulting path deleted recursively.
///
/// The whole ladder is gone: with no readable record nothing names a folder, so the removal has
/// nothing to act on and does nothing at all.
#[test]
fn a_hostile_lock_skill_id_cannot_traverse_out_of_the_store() {
    let rig = Rig::new("hostile-traversal");
    let project = rig.work.0.join("checkout");
    let precious = project.join("src");
    std::fs::create_dir_all(&precious).unwrap();
    std::fs::write(precious.join("main.rs"), b"fn main() {}\n").unwrap();

    // `<project>/.topos/state/<user>/conflicts/../../../../src` == `<project>/src`.
    let planted = HostileCheckout::plant(&project, "ＡＢＣ", "../../../../src", None);
    std::fs::create_dir_all(planted.layout.conflicts_dir()).unwrap();

    let (p, f) = (InertPlane, InertFollow);
    let ctx = rig.ctx_at(planted.layout.clone(), &p, &f);
    ops::merge_resolve::clear_conflict(&ctx, &planted.sp, ops::merge_resolve::Workbench::Unread)
        .unwrap();

    assert!(
        precious.join("main.rs").exists(),
        "an unvalidated on-disk string must never reach a path join"
    );
    assert!(
        !planted.sp.conflict.exists(),
        "the block itself still clears"
    );
}

/// **With no readable record, remove nothing.**
///
/// Two bundles can legitimately carry the same display name — two workspaces, or a workspace copy
/// beside a local one — and the workbench folder is keyed by that name. So a removal that derives
/// its target from the name, as this one did whenever no record could be read, deletes the OTHER
/// bundle's live hand merge; `--reset` reached that derivation unconditionally, and the quiet
/// sweep defensively. Git never re-derives a deletion target from a user-facing name.
#[test]
fn a_clear_with_no_readable_record_never_names_a_folder_from_the_bundle_name() {
    let rig = Rig::new("no-record-no-removal");
    // The OTHER bundle's live workbench, under the name the two share.
    let live = rig.layout().home().join("conflicts").join("pr-describe");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("SKILL.md"), b"# my hand merge\n").unwrap();

    // THIS bundle: the same display name, and a `conflict.json` that cannot be read.
    let sp = rig.layout().published(&sid("topos_twin1"));
    std::fs::create_dir_all(&sp.store).unwrap();
    doc::write_doc(
        &rig.fs,
        &sp.lock,
        &topos_types::persisted::Lock {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            skill_id: "topos_twin1".to_owned(),
            name: "pr-describe".to_owned(),
            base_commit: "0".repeat(64),
            bundle_digest: "0".repeat(64),
            files: Vec::new(),
        },
    )
    .unwrap();
    std::fs::write(&sp.conflict, b"{ not json at all").unwrap();

    let (p, f) = (InertPlane, InertFollow);
    let ctx = rig.ctx(&p, &f);
    ops::merge_resolve::clear_conflict(&ctx, &sp, ops::merge_resolve::Workbench::Unread).unwrap();

    assert_eq!(
        std::fs::read(live.join("SKILL.md")).unwrap(),
        b"# my hand merge\n",
        "another bundle's live hand merge must survive"
    );
    assert!(
        !sp.conflict.exists(),
        "the block itself still clears — only the folder is spared"
    );
}

/// **An unscannable workbench folder must refuse, never destroy the hand resolution in it.**
///
/// The folder is the ONLY copy of a hand merge — it sits outside the placement map, so the
/// materializer's snapshot rail never sees it. `--keep-mine` used to fold every scan failure
/// into "the folder is absent": it committed the ORIGINAL draft, wrote it over every placement,
/// deleted the folder, and reported `Merged`. The scanner rejects a tree holding a symlink (as
/// here), a non-regular file, a non-UTF-8 name, or no files at all — every one of them a plausible
/// state for someone mid-merge.
#[test]
fn an_unreadable_conflict_folder_refuses_instead_of_destroying_the_hand_resolution() {
    let rig = Rig::new("unreadable-copy");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The hand merge, plus one thing the scanner refuses — a symlink to a note kept elsewhere.
    let copy = rig.conflict_copy(&id);
    write_tree(
        &copy,
        &[
            ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ],
    );
    let elsewhere = rig.work.0.join("notes.md");
    std::fs::write(&elsewhere, b"scratch\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, copy.join("notes.md")).unwrap();

    let err = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .expect_err("an unreadable workbench folder must refuse");
    match &err {
        crate::error::ClientError::ConflictCopyUnreadable { skill, reason, .. } => {
            assert_eq!(skill, "pr-describe");
            assert!(reason.contains("symlink"), "{reason}");
        }
        other => panic!("expected the unreadable-workbench refusal, got {other:?}"),
    }
    // NOTHING moved: the hand resolution is still on disk, the block still stands, and the
    // placement still holds the author's own version.
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# hand-resolved\n",
        "the only copy of the hand merge must survive"
    );
    assert!(rig.conflict_exists(&id), "the block still stands");
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // Remove the offending entry and the same command resolves normally — the refusal is a state
    // to fix, never a dead end.
    std::fs::remove_file(copy.join("notes.md")).unwrap();
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[
            ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ]))
    );
}

/// `.topos-mine` siblings are topos's own compare-and-resolve scaffolding, so they never become
/// bundle content. Keeping them out of the placements holds only while the block stands: the
/// escape COMMITS the workbench folder and writes it to every placement, and `publish` ships a
/// placement's bytes — so the moment the author edits anything and escapes, an unstripped sibling
/// would ship to the team. It is stripped instead of refused, so resolving a binary conflict the
/// obvious way (fix the file, leave the aid alone) is not a dead end.
#[test]
fn a_hand_resolution_never_commits_topos_mine_scaffolding() {
    let rig = Rig::new("mine-strip");
    let base: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 1, 2])];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 9, 9])];
    write_tree(&rig.placement(), mine);
    let theirs: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 7, 7])];
    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The binary conflict kept both sides in the workbench. The author resolves the real file and
    // leaves the aid exactly where topos put it.
    let copy = rig.conflict_copy(&id);
    assert!(copy.join("logo.bin.topos-mine").exists());
    std::fs::write(copy.join("logo.bin"), [0xffu8, 4, 4]).unwrap();

    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    // The placement — which is what `publish` ships — holds the resolution and nothing else.
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[("logo.bin", FileMode::Regular, &[0xffu8, 4, 4])])),
        "the .topos-mine sibling must not become bundle content"
    );
    assert!(!rig.placement().join("logo.bin.topos-mine").exists());
    // And the committed version carries the same file set, so nothing can publish it later either.
    let committed: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 4, 4])];
    let m = mk_version(&[v1.id], committed, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// An exit marks the record CONCLUDED, converges its placements, and clears `conflict.json` LAST,
/// so a crash in that beat leaves a fully resolved bundle still carrying the MARKED record — that
/// is the one shape a real crash leaves, and both arms must FINISH it rather than read it as a
/// live block. Read live, the leftover does damage in both directions: a sweep re-settles
/// `work_hash` to the pre-escape draft (naming bytes that are nowhere on disk) and re-discloses a
/// block on a resolved bundle, and `--keep-mine` sees the already-removed workbench folder as
/// "untouched" and commits the ORIGINAL DRAFT over the resolution the placements hold. The mark —
/// never a document comparison — is what tells the leftover from a live block.
#[test]
fn a_record_that_outlived_its_resolution_is_cleared_not_re_blocked() {
    let rig = Rig::new("record-outlived");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // Hand-resolve, escape — and then put the record back MARKED, which is exactly the on-disk
    // state a crash between the escape's placement write and its record removal leaves (the mark
    // goes down before any placement moves).
    let record = rig.conflict_state(&id);
    let copy = rig.conflict_copy(&id);
    let resolved: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&copy, resolved);
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    let after_escape = rig.read_sync(&id);
    let crashed = topos_types::persisted::ConflictState {
        concluded: Some(topos_types::persisted::ConcludedExit::Escape),
        ..record.clone()
    };
    doc::write_doc(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
        &crashed,
    )
    .unwrap();

    // (a) the marked leftover under `--keep-mine`: the crashed exit's own command re-runs it to
    // completion, idempotently — the content-addressed conclusion is the SAME commit, the
    // materializer heals the already-landed placement with no second swap, and the record clears
    // with the real finish. Nothing re-commits the pre-escape draft over the hand resolution.
    let finished = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(
        only(&finished).action,
        PullAction::Merged,
        "a marked-Escape leftover finishes, never refuses"
    );
    assert!(!rig.conflict_exists(&id));
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "the hand resolution must survive a leftover record"
    );
    assert_eq!(
        rig.read_sync(&id).work_hash,
        after_escape.work_hash,
        "the finish re-lands the escape's own state"
    );

    // (b) the same marked leftover under a bare sweep, after the team publishes AGAIN. The sweep
    // FIRST finishes the marked conclusion — its row IS the finished merge — and only a LATER
    // sweep raises a fresh block against the version that actually moved. What must never happen
    // is the STALE record's re-disclosure, which would re-settle `work_hash` to the pre-escape
    // draft and send the reader to a workbench describing bytes that are nowhere on disk. The
    // discriminator is the MARK, not whether a block stands.
    //
    // The team publishes AGAIN first, so a block really is raised. Without a pending update the
    // second sweep has nothing to merge, and the assertions about the FRESH record below never run
    // — the situation they check has to be created for them to check anything at all.
    let v2files: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# v2\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v2.id, 2));
    doc::write_doc(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
        &crashed,
    )
    .unwrap();
    let first =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(
        first.action,
        PullAction::Merged,
        "the sweep finishes the marked conclusion before anything else"
    );
    assert!(
        !rig.conflict_exists(&id),
        "the finished conclusion takes the leftover record with it"
    );
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "v2 contests the same line the hand resolution rewrote"
    );
    let now = rig.read_sync(&id);
    assert_eq!(now.work_hash, after_escape.work_hash);
    assert_ne!(
        now.work_hash, record.draft_digest,
        "the docs must not name the pre-escape draft — nothing on disk holds it"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(resolved)));
    let fresh = doc::read_doc::<topos_types::persisted::ConflictState>(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
    )
    .unwrap()
    .expect("the new divergence raises a block of its own");
    assert_eq!(
        fresh.draft_digest, after_escape.work_hash,
        "a re-raised block describes the bytes on disk, never the pre-escape draft"
    );
    assert_ne!(
        fresh.result_commit, record.result_commit,
        "…and it is a fresh merge, not the leftover re-disclosed"
    );
    assert_eq!(
        fresh.current_commit,
        to_hex(&v2.id),
        "…against the version that actually moved"
    );
}

/// **The regression pin for the deleted inference.** An UNMARKED record whose durable documents
/// happen to satisfy the retired landed-exit pair — `map.applied_commit == current_commit` and
/// `map.materialized_sha == sync.work_hash`, with a `copy_dir` on the record — is a LIVE stopped
/// merge and must be treated as one: re-disclosed by the sweep, never cleared, and FINISHED (not
/// refused) by `--keep-mine`. A narrowed `--dest` reset produces exactly this document shape while
/// another copy still holds the merge, which is why liveness is the recorded mark and never a
/// document comparison.
#[test]
fn an_unmarked_record_matching_the_old_landed_pair_is_still_live() {
    let rig = Rig::new("old-pair-live");
    let (id, _name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), MINE_OVER_BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // Plant exactly the pair the deleted inference read as "an exit already landed".
    let cs = rig.conflict_state(&id);
    assert!(cs.copy_dir.is_some() && cs.concluded.is_none());
    let work_hash = rig.read_sync(&id).work_hash;
    rig.patch_map(&id, |m| {
        m.applied_commit = cs.current_commit.clone();
        m.materialized_sha = work_hash.clone();
    });

    // The sweep re-discloses the live block — record and workbench both stand.
    let copy = rig.conflict_copy(&id);
    let workbench = snapshot(&copy);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted,
        "an unmarked record is live whatever the other documents say"
    );
    assert!(rig.conflict_exists(&id), "never cleared on a document pair");
    assert_eq!(
        snapshot(&copy),
        workbench,
        "the workbench survives untouched"
    );

    // …and the escape still FINISHES it, because the merge really is stopped.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        keep_mine_scope("pr-describe".into()),
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!rig.conflict_exists(&id));
}

/// A `--reset` marks the record CONCLUDED only once every copy has PROVEN settled, so a marked-Reset
/// record describes exactly one state: the reset's placements landed and the clear did not. The next
/// bare sweep FINISHES it — the record and the untouched workbench go, the team's version stays on
/// disk, the discarded draft survives as a content-addressed store snapshot — and the sweep then
/// reads an ordinary current bundle. An edit made between the mark and the recovery is an ordinary
/// draft on the reset state: the clear still lands, and the edit is left exactly where it is.
#[test]
fn a_marked_reset_record_is_finished_by_the_next_sweep() {
    let rig = Rig::new("marked-reset-finish");
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), MINE_OVER_BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // The workbench as topos wrote it, kept aside so the crash state can be re-planted byte for byte
    // (the copy carries the modes, so the restored folder still reads as UNTOUCHED).
    let cs = rig.conflict_state(&id);
    let copy = rig.conflict_copy(&id);
    let kept = rig.work.0.join("kept-workbench");
    copy_tree(&copy, &kept);

    // A REAL full reset first: the placements land, the settle proof passes, the mark goes down and
    // the clear takes the record with it. Re-planting that record marked is then exactly the state a
    // crash between those last two writes leaves — and the ONLY state a marked-Reset record can be in.
    ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    assert!(!rig.conflict_exists(&id));
    let marked = topos_types::persisted::ConflictState {
        concluded: Some(topos_types::persisted::ConcludedExit::Reset),
        ..cs
    };
    let replant = || {
        copy_tree(&kept, &copy);
        doc::write_doc(
            &rig.fs,
            &rig.layout().published(&sid(&id)).conflict,
            &marked,
        )
        .unwrap();
    };

    replant();
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(
        row.action,
        PullAction::UpToDate,
        "the sweep finishes the reset and reads the bundle as current"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the team's version stands"
    );
    assert!(
        !rig.conflict_exists(&id),
        "the finished reset clears the block"
    );
    assert!(!copy.exists(), "and the untouched workbench with it");
    // The draft is never lost: the reset's snapshot rail committed it on the post-conflict base
    // (the team's version — what the conflict made the draft's recorded base).
    let stored = mk_version(&[v1.id], MINE_OVER_BASE, DEVICE, "topos: draft snapshot");
    assert!(
        rig.open_store(&id)
            .list_versions()
            .unwrap()
            .contains(&stored.id),
        "the discarded draft survives as a store snapshot"
    );

    // The same leftover, with the person back at work in the folder: the finish is a CLEAR and
    // nothing else, so an edit made after the mark is never overwritten and never lost.
    replant();
    let after: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# after the reset\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&rig.placement(), after);
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(row.action, PullAction::UpToDate);
    assert!(
        !rig.conflict_exists(&id),
        "the clear lands over a re-edited folder too"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(after)),
        "an edit made after the mark is an ordinary draft — left exactly where it is"
    );
}

/// **A reset that cannot settle every copy leaves the merge LIVE.** The reset WRITES the current
/// plan's placements while the settled proof READS every recorded copy — so a copy the plan omits
/// (here one recorded for a harness this machine does not place into) keeps its edits through the
/// reset and the proof fails. Concluding the record before that proof would strand it: no later run
/// re-plans the omitted copy either, so nothing could ever settle it, and `--keep-mine` — which
/// routes a marked record through the finisher — could no longer end the merge. So an unproven reset
/// writes no mark at all: the record stays a LIVE stopped merge, re-disclosed by the sweep, with
/// both ways out still open.
#[test]
fn a_reset_that_cannot_settle_every_copy_leaves_the_merge_live() {
    let rig = Rig::new("reset-unsettled-live");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let copy = rig.conflict_copy(&id);

    // Two copies holding the same un-merged draft, each recorded for its own agent — and the machine
    // detects only one of them. The plan names the detected agent's copy alone; the other is outside
    // it (`placement::managed_indices`: frozen in place, never written, never deleted).
    let replica = rig.work.0.join("replica");
    add_replica(&rig, &id, &replica, mine);
    rig.patch_map(&id, |m| {
        m.placement_state[0].agent = Some("claude-code".to_owned());
        m.placement_state[1].agent = Some("cursor".to_owned());
    });
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let detected = topos_harness::registry::detected_harnesses(&rig.home.0, None);
    assert!(
        detected.iter().any(|h| h.slug == "claude-code"),
        "the rig's home must detect the agent whose copy the plan names"
    );
    assert!(
        !detected.iter().any(|h| h.slug == "cursor"),
        "and must NOT detect the one holding the copy outside the plan"
    );
    let ctx = Ctx {
        roots: Some(crate::ctx::AgentRoots {
            home: rig.home.0.clone(),
            cwd: None,
        }),
        ..rig.ctx(&plane, &foll)
    };

    // The FULL reset — no selector, so it means "every copy" — but it can only reach the copy the
    // plan names.
    ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the planned copy is back at the team's version"
    );
    assert_eq!(
        snapshot(&replica),
        Some(expect(mine)),
        "the copy outside the plan is untouched — which is what the proof then reads"
    );
    assert!(
        rig.conflict_exists(&id),
        "a copy still holds the merge, so the record — the publish guard — must stand"
    );
    assert!(
        rig.conflict_state(&id).concluded.is_none(),
        "and it stands as a LIVE record: an unproven reset concludes nothing"
    );
    assert!(copy.exists(), "the workbench stands with it");

    // Live means live: the sweep re-discloses the block…
    assert_eq!(
        only(&pull_data(&ctx, ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted,
        "an unmarked record is re-disclosed, never finished away"
    );
    assert!(rig.conflict_exists(&id));

    // …and the `--keep-mine` the receipt names still FINISHES the merge, taking the record and the
    // workbench with the real conclusion.
    let escaped = pull_data(&ctx, keep_mine_scope(name)).unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(
        !rig.conflict_exists(&id),
        "the escape ends what the reset could not"
    );
    assert!(!copy.exists());
}

/// Copy a whole tree, modes included — how a by-hand fixture re-plants a folder exactly as topos
/// wrote it.
fn copy_tree(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// **Every record this build writes NAMES its workbench** — the one field every read, write and
/// removal of the marked-up copy keys on. A recording path that forgot it would raise a block
/// whose markers nothing names: no folder on the disclosure, and none for the escape to read. So
/// it is pinned here rather than left to the two recording sites' good behaviour.
#[test]
fn every_conflict_record_this_build_writes_names_its_workbench() {
    // The three-way conflict.
    {
        let rig = Rig::new("copydir-threeway");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), MINE_OVER_BASE);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        assert_eq!(
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::Conflicted
        );
        assert!(rig.conflict_state(&id).copy_dir.is_some());
    }
    // The 2-way no-base fallback.
    {
        let rig = Rig::new("copydir-nobase");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), MINE_OVER_BASE);
        rig.patch_lock(&id, |l| {
            l.base_commit = "f".repeat(64);
            l.bundle_digest = "e".repeat(64);
        });
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        assert_eq!(
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::Conflicted
        );
        assert!(rig.conflict_state(&id).copy_dir.is_some());
    }
}

/// **A record naming NO folder names nothing — and the escape still finishes.** The field is
/// optional on the document and an unparseable value reads the same way, so the state is reachable
/// from a hostile checkout as well as from a truncated write. Nothing is derived from the bundle's
/// name to fill the gap: the block stays LIVE, the row names no folder, no folder under
/// `conflicts/` is written or removed, and `--keep-mine` concludes from what the placements hold —
/// the exit that must never deadlock.
#[test]
fn a_record_that_names_no_workbench_is_live_names_no_folder_and_still_escapes() {
    let rig = Rig::new("no-workbench-named");
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), MINE_OVER_BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // Strip the one field that names a folder, leaving the folder itself on disk: a record that
    // names nothing must not write into, scan, or REMOVE anything under `conflicts/`.
    let copy = rig.conflict_copy(&id);
    let workbench = snapshot(&copy);
    let cs = rig.conflict_state(&id);
    doc::write_doc(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
        &topos_types::persisted::ConflictState {
            copy_dir: None,
            ..cs
        },
    )
    .unwrap();

    // The sweep re-discloses a live block whose row names no folder.
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("the block is re-disclosed");
    assert_eq!(mr.copy_dir, None, "a record naming no folder names none");
    assert!(
        !mr.placements.is_empty(),
        "…and the placements it does name still hold this person's own version"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(MINE_OVER_BASE)));
    assert_eq!(
        snapshot(&copy),
        workbench,
        "nothing is written into a folder no record names"
    );

    // …and the escape concludes from the copies the folders hold.
    let escaped = pull_data(&rig.ctx(&plane, &foll), keep_mine_scope(name)).unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!rig.conflict_exists(&id));
    assert_eq!(
        snapshot(&copy),
        workbench,
        "…and it removes no folder it never named"
    );
}

/// **`--keep-mine` commits the folder as it stands.** Publishing is blocked while a merge is
/// stopped, so "keep working, finish it later" is the expected sequence — and the exit used to
/// render the draft as of when the merge STOPPED and write it over every copy, reverting
/// everything done since with no word on the receipt. Git concludes a stopped merge from the
/// working tree; so does this.
#[test]
fn keep_mine_commits_the_folder_as_it_stands_not_the_conflict_time_snapshot() {
    let rig = Rig::new("keep-mine-live");
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), MINE_OVER_BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // Kept working while the merge sat stopped: the contested file moved on, and a file the team
    // never touched was added.
    let later: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine, thought about\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ("notes.md", FileMode::Regular, b"kept working\n"),
    ];
    write_tree(&rig.placement(), later);

    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    // The work done since is IN, on top of the team's other changes.
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[
            ("SKILL.md", FileMode::Regular, b"# mine, thought about\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
            ("notes.md", FileMode::Regular, b"kept working\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
        ])),
        "the exit commits the folder as it stands"
    );
    // And the committed version is that same tree — nothing to publish later that the folder
    // does not hold.
    let m = mk_version(
        &[v1.id],
        &[
            ("SKILL.md", FileMode::Regular, b"# mine, thought about\n"),
            ("notes.md", FileMode::Regular, b"kept working\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ],
        DEVICE,
        "topos: merge escape",
    );
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// An accept RESOLVES against the version this very call discovered. `current` sits at v2 (whose
/// parent v1 the client never saw), the author's copy is edited, and the accept merges against v2 —
/// the version it raised — rather than deferring it to a second command.
#[test]
fn an_accept_resolves_against_a_version_raised_in_the_same_pull() {
    let rig = Rig::new("raised");
    let (id, _name, genesis) = rig.adopt(BASE);
    let edited: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let foll = follow(&id);

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2files: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# v2\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v2\n"),
    ];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v2.id, 2));

    let row = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    let row = only(&row);
    // Both sides edited SKILL.md, so the resolution conflicts — the point is that it RAN, against
    // the version the same call discovered.
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "an accept resolves a version raised in the same call, it does not defer it"
    );
    let mr = row.merge.as_ref().expect("a merge report");
    assert_eq!(mr.theirs_version_id, to_hex(&v2.id));
    assert_eq!(rig.read_sync(&id).observed, 2);
}

/// A `.topos-mine` sidecar must be disambiguated against the kernel's collision rule (NFC + case-fold),
/// not just exact bytes — otherwise a publisher-added path that case-folds to the sidecar name wedges the
/// resolution into a `Corrupt` digest error instead of a clean conflict.
#[test]
fn sidecar_avoids_case_fold_collision_with_a_real_path() {
    let rig = Rig::new("sidecar-collide");
    let base: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 1, 2])];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 9, 9])];
    write_tree(&rig.placement(), mine);
    // theirs changes the binary AND adds a path that ASCII-case-folds to `logo.bin.topos-mine`.
    let theirs_files: FileSet = &[
        ("logo.bin", FileMode::Regular, &[0xffu8, 7, 7]),
        (
            "LOGO.BIN.TOPOS-MINE",
            FileMode::Regular,
            b"real theirs file\n",
        ),
    ];
    let v1 = mk_version(&[genesis], theirs_files, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "the binary conflict must resolve cleanly, not error on a digest collision"
    );
    // In the CONFLICT COPY: theirs at the path, theirs' real file kept, and mine's sidecar
    // DISAMBIGUATED away from the collision. (The placement, as always, keeps mine untouched.)
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        std::fs::read(copy.join("logo.bin")).unwrap(),
        &[0xffu8, 7, 7]
    );
    assert!(copy.join("LOGO.BIN.TOPOS-MINE").exists());
    assert_eq!(
        std::fs::read(copy.join("logo.bin.topos-mine-1")).unwrap(),
        &[0xffu8, 9, 9],
        "the sidecar was disambiguated to avoid the case-fold collision"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    // The written tree scans back to the recorded conflict digest (no kernel rejection).
    let cs: topos_types::persisted::ConflictState =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid(&id)).conflict)
            .unwrap()
            .unwrap();
    let scanned = crate::scan::scan(&copy).unwrap();
    assert_eq!(to_hex(&scanned.bundle_digest), cs.conflicted_digest);
}

// ---------------------------------------------------------------------------------------------
// The sweep's plane-down circuit breaker + the machine-visible per-skill warnings.
// ---------------------------------------------------------------------------------------------

/// A counting transport whose `get_current` always fails at the given level — the breaker's oracle
/// (every network call the sweep makes is a counter tick).
#[derive(Default)]
struct CountingDownPlane {
    /// `true` ⇒ connect-level (`Unreachable`, trips the breaker); `false` ⇒ HTTP-level (`Unavailable`).
    connect_level: bool,
    gets: std::cell::Cell<u32>,
    lists: std::cell::Cell<u32>,
}
impl PlaneSource for CountingDownPlane {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        self.gets.set(self.gets.get() + 1);
        Err(if self.connect_level {
            PlaneError::Unreachable("connect refused".into())
        } else {
            PlaneError::Unavailable("HTTP 500".into())
        })
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, PlaneError> {
        Err(PlaneError::Unavailable("HTTP 500".into()))
    }
    fn list_open_proposals(&self, _skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        self.lists.set(self.lists.get() + 1);
        Ok(Vec::new())
    }
}

/// A follow source listing the SAME skill N times — the cheapest way to drive an N-skill sweep against
/// one adopted sidecar (each pass takes and releases the per-skill lock sequentially).
fn follow_n(skill_id: &str, n: usize) -> FixtureFollow {
    FixtureFollow {
        entries: (0..n)
            .map(|_| {
                (
                    skill_id.to_owned(),
                    FollowContext {
                        workspace_id: WS.to_owned(),
                        review_required: false,
                        following: true,
                    },
                )
            })
            .collect(),
    }
}

#[test]
fn sweep_breaker_trips_on_first_connect_failure_and_skips_all_remaining_network_calls() {
    let rig = Rig::new("breaker");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = CountingDownPlane {
        connect_level: true,
        ..Default::default()
    };
    let foll = follow_n(&id, 3);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // Every skill still gets a local-state row (the engine falls through to the local drive)...
    assert_eq!(out.data.skills.len(), 3);
    // ...but the plane was dialed exactly ONCE: the first connect-level failure tripped the breaker,
    // and the remaining sweep passes + the proposals count made ZERO further network calls.
    assert_eq!(
        plane.gets.get(),
        1,
        "one connect timeout, not one per skill"
    );
    assert_eq!(
        plane.lists.get(),
        0,
        "the proposals count is skipped once the breaker tripped"
    );
    assert_eq!(out.data.proposals_awaiting, 0);
}

#[test]
fn sweep_breaker_never_trips_on_an_http_level_failure() {
    let rig = Rig::new("nobreak");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = CountingDownPlane {
        connect_level: false,
        ..Default::default()
    };
    let foll = follow_n(&id, 3);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // An HTTP 500 means the plane ANSWERED — per-skill isolation, no breaker: all three are dialed,
    // and the proposals count still runs.
    assert_eq!(out.data.skills.len(), 3);
    assert_eq!(plane.gets.get(), 3);
    assert_eq!(plane.lists.get(), 3);
}

#[test]
fn go_back_is_plane_independent_and_spends_no_network_call() {
    let rig = Rig::new("gbnonet");
    let (id, name, genesis) = rig.adopt(BASE);
    let plane = CountingDownPlane {
        connect_level: true,
        ..Default::default()
    };
    let foll = follow(&id);

    // A go-back to the adopted genesis (recorded locally) must complete with the plane fully down —
    // and make ZERO network calls (including the proposals count, which is documented plane-independent).
    let out = ops::pull(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(genesis)),
        },
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(plane.gets.get(), 0, "go-back never dials the plane");
    assert_eq!(plane.lists.get(), 0, "no proposals GET on the go-back path");
    assert_eq!(out.data.proposals_awaiting, 0);
}

#[test]
fn sweep_surfaces_an_isolated_per_skill_failure_as_an_envelope_warning() {
    let rig = Rig::new("warn");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = FixturePlane::default(); // serves nothing → the healthy skill reads NotFound → UpToDate
    let foll = FixtureFollow {
        entries: vec![
            // A followed id with NO sidecar docs — the sweep must isolate it, not abort.
            (
                "topos_missing".to_owned(),
                FollowContext {
                    workspace_id: WS.to_owned(),
                    review_required: false,
                    following: true,
                },
            ),
            (
                id.clone(),
                FollowContext {
                    workspace_id: WS.to_owned(),
                    review_required: false,
                    following: true,
                },
            ),
        ],
    };

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The healthy skill still produced its row (isolation)...
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(out.data.skills[0].action, PullAction::UpToDate);
    // ...and the failed one is machine-visible: one stable-shape warning naming the code + the skill.
    assert_eq!(out.warnings.len(), 1);
    let w = &out.warnings[0];
    assert!(
        w.text.contains("topos_missing"),
        "the warning names the failed skill: {w:?}"
    );
    // The CODE rides its own field now — never the prose a person reads.
    let code = w.code.clone().expect("the stable error code");
    assert!(
        code.starts_with(char::is_uppercase) && !code.contains(' '),
        "the code is one SCREAMING_SNAKE token: {w:?}"
    );
    assert!(
        !w.text.starts_with(&code),
        "the text never repeats the code: {w:?}"
    );
}

#[test]
fn a_wedged_skills_sweep_failure_surfaces_in_its_topos_log() {
    let rig = Rig::new("wedgelog");
    let (id, name, _genesis) = rig.adopt(BASE);
    // Wedge the tracked skill: a corrupt sync.json makes every sweep of it fail. lock.json + the store
    // stay intact, so `log` still resolves the skill.
    std::fs::write(rig.layout().published(&sid(&id)).sync, b"{not json").unwrap();
    let plane = FixturePlane::default();
    let foll = follow(&id);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert!(out.data.skills.is_empty(), "the wedged skill has no row");
    assert_eq!(out.warnings.len(), 1);

    // The REAL read path: `topos log <skill>` filters on the first-class skill_id field, so the wedged
    // skill's error event surfaces in its own log.
    let dir = |_: &str| -> Box<dyn crate::plane::DirectorySource> {
        unreachable!("local log builds no directory transport")
    };
    let nosess = |_s: &crate::sessions::Session| -> ops::SessionTransports {
        unreachable!("local log builds no session transports")
    };
    let connectors = ops::LogConnectors {
        directory: &dir,
        session: &nosess,
    };
    let log = ops::log(
        &rig.ctx(&plane, &foll),
        &connectors,
        &name,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let errors: Vec<_> = log
        .events
        .iter()
        .filter(|e| e.get("action").and_then(|v| v.as_str()) == Some("error"))
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "the wedged skill's failure is in its log: {:?}",
        log.events
    );
    assert_eq!(
        errors[0].get("skill_id").and_then(|v| v.as_str()),
        Some(id.as_str())
    );
    assert_eq!(errors[0].get("verb").and_then(|v| v.as_str()), Some("pull"));

    // The TTY renderer's error arm renders it readably (verb + code).
    let text = crate::render::log_tty(&log);
    assert!(text.contains("error  pull ["), "{text}");
}

#[test]
fn sweep_refuses_a_traversal_follow_id_as_a_warning_never_a_join() {
    let rig = Rig::new("hostileid");
    let (_id, _name, _genesis) = rig.adopt(BASE);
    let plane = FixturePlane::default();
    let foll = FixtureFollow {
        entries: vec![(
            "../../evil".to_owned(),
            FollowContext {
                workspace_id: WS.to_owned(),
                review_required: false,
                following: true,
            },
        )],
    };

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The hostile id never reaches a path join: no row, one warning, and nothing appears at the
    // would-be escape target beside the home.
    assert!(out.data.skills.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert!(
        out.warnings[0].code.as_deref() == Some("CORRUPT_STATE"),
        "{:?}",
        out.warnings
    );
    assert!(
        !rig.home.0.parent().unwrap().join("evil").exists(),
        "no directory materialized outside the home"
    );
}

// ---------------------------------------------------------------------------------------------
// The per-op durability bound: a pull fsyncs the fetched version's objects + ref — and ONLY those —
// before any doc records the applied version (the fetch-then-record contract).
// ---------------------------------------------------------------------------------------------

/// Wraps [`RealFs`] and records every mutating op (label + the affected path) in call order, so a test
/// can pin WHAT a pull made durable, that the set is bounded (no historical object re-synced), and that
/// the store fsyncs precede the doc writes recording the result. Reads/locks are not recorded.
struct RecordingFs {
    inner: RealFs,
    ops: std::cell::RefCell<Vec<(&'static str, PathBuf)>>,
}
impl RecordingFs {
    fn new() -> Self {
        Self {
            inner: RealFs,
            ops: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn record(&self, label: &'static str, path: &Path) {
        self.ops.borrow_mut().push((label, path.to_path_buf()));
    }
    fn ops(&self) -> Vec<(&'static str, PathBuf)> {
        self.ops.borrow().clone()
    }
}
impl FsOps for RecordingFs {
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.record("write_temp", path);
        self.inner.write_temp(path, bytes)
    }
    fn fsync_file(&self, path: &Path) -> std::io::Result<()> {
        self.record("fsync_file", path);
        self.inner.fsync_file(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.record("rename", to);
        self.inner.rename(from, to)
    }
    fn fsync_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.record("fsync_dir", dir);
        self.inner.fsync_dir(dir)
    }
    fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.record("rename_dir_noreplace", to);
        self.inner.rename_dir_noreplace(from, to)
    }
    fn create_dir_all(&self, dir: &Path) -> std::io::Result<()> {
        self.record("create_dir_all", dir);
        self.inner.create_dir_all(dir)
    }
    fn append_fsync(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
        self.record("append_fsync", path);
        self.inner.append_fsync(path, line)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.record("remove_file", path);
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.record("remove_dir_all", path);
        self.inner.remove_dir_all(path)
    }
    fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> std::io::Result<()> {
        self.record("write_staged", path);
        self.inner.write_staged(path, bytes, executable)
    }
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.record("write_private", path);
        self.inner.write_private(path, bytes)
    }
    fn open_dir_handle(&self, dir: &Path) -> std::io::Result<crate::fs_seam::DirHandle> {
        self.inner.open_dir_handle(dir)
    }
    fn rename_at(
        &self,
        h: &crate::fs_seam::DirHandle,
        from: &str,
        to: &str,
    ) -> std::io::Result<()> {
        self.record("rename_at", &h.path().join(to));
        self.inner.rename_at(h, from, to)
    }
    fn rename_at_noreplace_src(
        &self,
        h: &crate::fs_seam::DirHandle,
        from: &str,
        to: &str,
        src: &crate::fs_seam::DirHandle,
    ) -> std::io::Result<()> {
        self.record("rename_at_noreplace_src", &h.path().join(to));
        self.inner.rename_at_noreplace_src(h, from, to, src)
    }
    fn exchange_at(&self, h: &crate::fs_seam::DirHandle, a: &str, b: &str) -> std::io::Result<()> {
        self.record("exchange_at", &h.path().join(b));
        self.inner.exchange_at(h, a, b)
    }
    fn exchange_at_src(
        &self,
        h: &crate::fs_seam::DirHandle,
        a: &str,
        b: &str,
        src: &crate::fs_seam::DirHandle,
    ) -> std::io::Result<()> {
        self.record("exchange_at_src", &h.path().join(b));
        self.inner.exchange_at_src(h, a, b, src)
    }
    fn create_dir_nofollow(
        &self,
        base: &Path,
        dir: &Path,
    ) -> std::io::Result<crate::fs_seam::DirHandle> {
        self.record("create_dir_nofollow", dir);
        self.inner.create_dir_nofollow(base, dir)
    }
    fn create_dir_nofollow_at(
        &self,
        base: &crate::fs_seam::DirHandle,
        dir: &Path,
    ) -> std::io::Result<crate::fs_seam::DirHandle> {
        self.record("create_dir_nofollow_at", dir);
        self.inner.create_dir_nofollow_at(base, dir)
    }
    fn remove_dir_all_at(&self, h: &crate::fs_seam::DirHandle, leaf: &str) -> std::io::Result<()> {
        self.record("remove_dir_all_at", &h.path().join(leaf));
        self.inner.remove_dir_all_at(h, leaf)
    }
    fn create_dir_at(&self, h: &crate::fs_seam::DirHandle, leaf: &str) -> std::io::Result<()> {
        self.record("create_dir_at", &h.path().join(leaf));
        self.inner.create_dir_at(h, leaf)
    }
    fn write_new_at(
        &self,
        h: &crate::fs_seam::DirHandle,
        leaf: &str,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        self.record("write_new_at", &h.path().join(leaf));
        self.inner.write_new_at(h, leaf, bytes)
    }
    fn write_staged_at(
        &self,
        h: &crate::fs_seam::DirHandle,
        leaf: &str,
        bytes: &[u8],
        executable: bool,
    ) -> std::io::Result<()> {
        self.record("write_staged_at", &h.path().join(leaf));
        self.inner.write_staged_at(h, leaf, bytes, executable)
    }
    fn rename_file_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.record("rename_file_noreplace", to);
        self.inner.rename_file_noreplace(from, to)
    }
    fn read_opt(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read_opt(path)
    }
    fn read_opt_nofollow(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read_opt_nofollow(path)
    }
    fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.read_dir(dir)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn path_kind(&self, path: &Path) -> std::io::Result<Option<crate::fs_seam::PathKind>> {
        self.inner.path_kind(path)
    }
    fn private_perms_ok(&self, path: &Path) -> std::io::Result<bool> {
        self.inner.private_perms_ok(path)
    }
    fn lock_exclusive(&self, path: &Path) -> std::io::Result<crate::fs_seam::LockGuard> {
        self.inner.lock_exclusive(path)
    }
    fn try_lock_exclusive(
        &self,
        path: &Path,
    ) -> std::io::Result<Option<crate::fs_seam::LockGuard>> {
        self.inner.try_lock_exclusive(path)
    }
}

/// Every loose object file currently under `<store>/objects/` (the shard walk).
fn store_loose_objects(store_dir: &Path) -> std::collections::HashSet<PathBuf> {
    let mut out = std::collections::HashSet::new();
    let objects = store_dir.join("objects");
    for shard in std::fs::read_dir(&objects).unwrap().flatten() {
        let p = shard.path();
        if p.is_dir() {
            for f in std::fs::read_dir(&p).unwrap().flatten() {
                if f.path().is_file() {
                    out.insert(f.path());
                }
            }
        }
    }
    out
}

/// V2 in the genesis → v1 → v2 chain — every file's bytes differ from BOTH earlier generations, so the
/// three versions share no blobs and the era sets below partition cleanly.
const V2: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# v2\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v2\n"),
    ("ref/notes.md", FileMode::Regular, b"new in v2\n"),
];

#[test]
fn pull_fsyncs_exactly_the_fetched_version_plus_its_direct_parent() {
    // Chain genesis → v1 → v2. Land v1 with a plain pull, then record a pull of v2 and pin its
    // durability frontier: the fetched version's own writes PLUS its direct parent's set (present ≠
    // durable, so a present v1 is re-fsynced — no-ops when it already was) — and NOTHING beyond:
    // grandparent-era (genesis) objects are never re-fsynced when the parent was present.
    let rig = Rig::new("fsyncset");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2 = mk_version(&[v1.id], V2, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let store_dir = rig.layout().published(&sid(&id)).store;
    let genesis_era = store_loose_objects(&store_dir);
    assert!(!genesis_era.is_empty(), "adopt left genesis objects");

    // Land v1 first (not the pull under test) — v2's direct parent becomes present + recorded.
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);
    let after_v1 = store_loose_objects(&store_dir);
    let v1_era: Vec<&PathBuf> = after_v1
        .iter()
        .filter(|p| !genesis_era.contains(*p))
        .collect();
    assert!(!v1_era.is_empty(), "the v1 pull wrote v1's objects");

    // The recorded pull: v2 arrives; its direct parent v1 is already present.
    plane.set_current(&id, served(WS, &id, v2.id, 2));
    let fs = RecordingFs::new();
    let data = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);

    let ops_log = fs.ops();
    let store_fsyncs: Vec<(usize, &PathBuf)> = ops_log
        .iter()
        .enumerate()
        .filter(|(_, (label, p))| *label == "fsync_file" && p.starts_with(&store_dir))
        .map(|(i, (_, p))| (i, p))
        .collect();
    let synced: std::collections::HashSet<&PathBuf> =
        store_fsyncs.iter().map(|&(_, p)| p).collect();

    // (a) COMPLETE: every loose object the fetch wrote — and v2's version ref — was fsynced before the
    // pull returned (the crash-safety contract: reachable ⇒ durable before recorded).
    let new: Vec<PathBuf> = store_loose_objects(&store_dir)
        .into_iter()
        .filter(|p| !after_v1.contains(p))
        .collect();
    assert!(!new.is_empty(), "the fetch wrote v2's objects");
    for p in &new {
        assert!(synced.contains(p), "fetched object {p:?} was not fsynced");
    }
    let v2_ref = store_dir.join("refs/topos/versions").join(to_hex(&v2.id));
    assert!(synced.contains(&v2_ref), "v2's version ref was not fsynced");

    // (b) PARENT INCLUDED: the direct parent's whole era was re-fsynced too — a present parent may sit
    // in the crash window between its write and its fsync, and this pull records a child naming it.
    for p in &v1_era {
        assert!(
            synced.contains(*p),
            "direct-parent object {p:?} was not re-fsynced — present was treated as durable"
        );
    }
    let v1_ref = store_dir.join("refs/topos/versions").join(to_hex(&v1.id));
    assert!(
        synced.contains(&v1_ref),
        "v1's version ref was not re-fsynced"
    );

    // (c) BOUNDED: nothing beyond the fetched version + its direct parent — no grandparent-era
    // (genesis) object or ref was re-fsynced, because the present parent's arm returns before walking
    // ITS parents. The per-pull durability set stays bounded, never the store's lifetime history.
    for p in &genesis_era {
        assert!(
            !synced.contains(p),
            "grandparent-era object {p:?} was re-fsynced — the durability set is unbounded"
        );
    }
    let genesis_ref = store_dir.join("refs/topos/versions").join(to_hex(&genesis));
    assert!(
        !synced.contains(&genesis_ref),
        "the grandparent's version ref was re-fsynced"
    );

    // (d) ORDERED: every store fsync precedes the first doc write that records the applied version
    // (map/lock are written only by the post-swap doc commit; sync.json's floor raise is earlier by
    // design and names no local bytes).
    let last_store_fsync = store_fsyncs.iter().map(|&(i, _)| i).max().unwrap();
    let first_apply_doc = ops_log
        .iter()
        .enumerate()
        .find(|(_, (label, p))| {
            *label == "write_temp"
                && p.file_name()
                    .is_some_and(|f| f.to_string_lossy().starts_with("map.json"))
        })
        .map(|(i, _)| i)
        .expect("the apply committed its docs");
    assert!(
        last_store_fsync < first_apply_doc,
        "a store fsync ({last_store_fsync}) landed after the doc commit began ({first_apply_doc})"
    );
}

#[test]
fn pull_fsyncs_a_present_but_unrecorded_parent() {
    // The crash window itself: a prior pull wrote v1's objects + ref but died BEFORE its fsync and
    // before any doc recorded it — v1 is present-and-renderable yet recorded nowhere and possibly not
    // durable. A pull of its child v2 must fsync v1's whole set too (never fetching it — it IS present),
    // not just v2's own writes.
    let rig = Rig::new("fsyncparent");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2 = mk_version(&[v1.id], V2, "d_pub", "v2");

    // Simulate the crash: commit v1 straight into the sidecar store — no fsync, no doc record.
    {
        let store = rig.open_store(&id);
        let import: Vec<topos_gitstore::ImportFile<'_>> = v1
            .fetched
            .files
            .iter()
            .map(|f| topos_gitstore::ImportFile {
                path: &f.path,
                mode: f.mode,
                bytes: &f.bytes,
            })
            .collect();
        let tree = store.write_bundle(&import).unwrap();
        store
            .commit(
                v1.id,
                &[genesis],
                &tree,
                &v1.fetched.author,
                &v1.fetched.message,
            )
            .unwrap();
    }

    // The plane serves ONLY v2 — the pull must not need to fetch the present parent.
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v2.id, 1));
    let foll = follow(&id);

    let store_dir = rig.layout().published(&sid(&id)).store;
    let fs = RecordingFs::new();
    let data = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);

    let synced: std::collections::HashSet<PathBuf> = fs
        .ops()
        .into_iter()
        .filter(|(label, p)| *label == "fsync_file" && p.starts_with(&store_dir))
        .map(|(_, p)| p)
        .collect();

    // v1's entire durability set (ref + commit + trees + blobs) was fsynced by the pull of v2, closing
    // the window where a doc records a child whose parent lineage could vanish on power loss.
    let v1_set = rig.open_store(&id).version_durability(&v1.id).unwrap();
    assert!(!v1_set.files.is_empty(), "v1 names a durability set");
    for p in &v1_set.files {
        assert!(
            synced.contains(p),
            "present-but-unrecorded parent path {p:?} was not fsynced"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// revert — the two-phase describe + the byte-level no-op. A forward revert mints a NEW commit id
// over IDENTICAL bytes, so repeated identical reverts must be caught by comparing TREE digests
// (not commit ids), else they mint generation after generation.
// ---------------------------------------------------------------------------------------------

/// The fixed forward-revert message. MIRRORS `ops::contribute::REVERT_MESSAGE` (that const is private
/// to the ops module); the forward commit id folds it in, so the served pointer names the same id.
const REVERT_MSG: &str = "topos: revert";

/// A contribute transport that COUNTS the writes it receives and answers a fixed receipt. The
/// two-phase describe and the no-op must never reach it (zero POSTs).
struct RecordingContribute {
    posts: std::rc::Rc<std::cell::Cell<usize>>,
    receipt: crate::plane::WriteReceipt,
}
impl crate::plane::ContributeSource for RecordingContribute {
    fn publish(
        &self,
        _b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        unreachable!("revert never publishes")
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        unreachable!("revert never proposes")
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        self.posts.set(self.posts.get() + 1);
        Ok(self.receipt.clone())
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, crate::error::ClientError> {
        unreachable!("revert never reviews")
    }
}

/// The tree (bundle) digest of a version's files — the value the no-op compares.
fn tree_of(v: &Version) -> [u8; 32] {
    let entries: Vec<ManifestEntry> = v
        .fetched
        .files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    digest::bundle_digest(&entries).unwrap()
}

/// Write the enrolled `instance.json` `revert` reads (the follow-state comes from the [`FixtureFollow`]
/// the caller hands `ctx`, not from disk).
fn seed_instance(rig: &Rig) {
    crate::sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        crate::sessions::Session {
            host: "topos.example".to_owned(),
            base_url: "https://topos.example/api".to_owned(),
            workspace_id: WS.to_owned(),
            workspace_name: "acme".to_owned(),
            display_name: "Acme".to_owned(),
            session_id: "sn_1".to_owned(),
            credential: "cred-1".to_owned(),
            status: crate::sessions::SESSION_ACTIVE.to_owned(),
            logged_in_at: 1,
        },
    )
    .unwrap();
}

/// An OK revert receipt naming `record` as the moved-to pointer (the server echoes the forward id).
fn ok_revert_receipt(record: WireCurrentRecord) -> crate::plane::WriteReceipt {
    crate::plane::WriteReceipt {
        receipt: Some(topos_types::Receipt {
            schema_version: 1,
            op_id: "op-rv".to_owned(),
            command: "reverts".to_owned(),
            outcome: topos_types::TerminalOutcome::Ok,
            workspace_id: WS.to_owned(),
            skill_id: Some(record.scope.skill_id.clone()),
            version_id: Some(record.record.version_id.clone()),
            bundle_digest: None,
            expected_generation: None,
            current_generation: Some(record.record.generation),
            created_at: "2026-07-16T00:00:00Z".to_owned(),
            details: None,
        }),
        error: None,
        wire_record: Some(record),
    }
}

#[test]
fn revert_bare_describes_without_writing_then_yes_applies() {
    let rig = Rig::new("rv-2phase");
    let (id, name, _genesis) = rig.adopt(&[("SKILL.md", FileMode::Regular, b"base\n")]);
    seed_instance(&rig);
    let foll = follow(&id);

    // good (tree A) and current (tree B, DIFFERENT) — both served by the plane.
    let good = mk_version(
        &[],
        &[("SKILL.md", FileMode::Regular, b"good bytes\n")],
        "auth",
        "m-good",
    );
    let current = mk_version(
        &[],
        &[("SKILL.md", FileMode::Regular, b"current bytes\n")],
        "auth",
        "m-current",
    );
    let mut plane = FixturePlane::default();
    plane.set_current(&id, served(WS, &id, current.id, 5));
    plane.add_version(&id, &good);
    plane.add_version(&id, &current);

    // The forward commit id the client computes + the server would echo (I-COMMIT-PARITY).
    let forward = identity::commit_id(&Commit {
        parents: &[current.id],
        tree: tree_of(&good),
        author: DEVICE,
        message: REVERT_MSG,
    })
    .unwrap();
    let posts = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let receipt = ok_revert_receipt(served(WS, &id, forward, 6));
    let connect = {
        let posts = posts.clone();
        move |_b: &str, _c: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
            Box::new(RecordingContribute {
                posts: posts.clone(),
                receipt: receipt.clone(),
            })
        }
    };
    let good_hex = to_hex(&good.id);
    let ctx = rig.ctx(&plane, &foll);

    // Bare = DESCRIBE: nothing written — no POST, no op-WAL. The paste-ready apply carries no
    // `--workspace` when none was given.
    let described = ops::revert(&ctx, &connect, &name, &good_hex, false, None).unwrap();
    match &described {
        ops::RevertOutcome::Describe { yes_argv, .. } => {
            assert_eq!(
                yes_argv,
                &vec![
                    "topos".to_owned(),
                    "revert".to_owned(),
                    name.clone(),
                    "--to".to_owned(),
                    good_hex.clone(),
                    "--yes".to_owned(),
                ],
            );
        }
        other => panic!("bare revert describes, got {other:?}"),
    }
    assert_eq!(posts.get(), 0, "a describe POSTs nothing");

    // A `--workspace` disambiguation is PRESERVED on the paste-ready apply (as the canonical id),
    // so the suggested command re-resolves to exactly the skill described.
    let described_ws = ops::revert(&ctx, &connect, &name, &good_hex, false, Some(WS)).unwrap();
    match &described_ws {
        ops::RevertOutcome::Describe { yes_argv, .. } => {
            assert_eq!(
                yes_argv,
                &vec![
                    "topos".to_owned(),
                    "revert".to_owned(),
                    name.clone(),
                    "--to".to_owned(),
                    good_hex.clone(),
                    "--workspace".to_owned(),
                    WS.to_owned(),
                    "--yes".to_owned(),
                ],
            );
        }
        other => panic!("bare revert with --workspace describes, got {other:?}"),
    }
    assert_eq!(
        posts.get(),
        0,
        "the workspace-scoped describe POSTs nothing either"
    );
    assert!(
        crate::op_wal::find_pending_for_skill(
            &rig.fs,
            &rig.layout(),
            WS,
            &id,
            &[topos_types::persisted::OpKind::Revert],
        )
        .unwrap()
        .is_none(),
        "a describe writes no op-WAL"
    );

    // `--yes` = apply: exactly one POST; the forward move lands.
    let applied = ops::revert(&ctx, &connect, &name, &good_hex, true, None).unwrap();
    match applied {
        ops::RevertOutcome::Applied(data) => {
            assert_eq!(data.reverted_to, good_hex);
            assert_eq!(data.new_version_id, to_hex(&forward));
        }
        other => panic!("--yes applies, got {other:?}"),
    }
    assert_eq!(posts.get(), 1, "--yes POSTs exactly once");
}

#[test]
fn revert_over_identical_bytes_is_a_no_op_under_differing_commit_ids() {
    let rig = Rig::new("rv-noop");
    let (id, name, _genesis) = rig.adopt(&[("SKILL.md", FileMode::Regular, b"base\n")]);
    seed_instance(&rig);
    let foll = follow(&id);

    // good and current share the SAME tree but DIFFERENT commit ids — current is a forward revert over
    // good's bytes, exactly the state one revert leaves behind (the repeated-revert bug's trigger).
    let files: &[(&str, FileMode, &[u8])] = &[("SKILL.md", FileMode::Regular, b"shared bytes\n")];
    let good = mk_version(&[], files, "auth", "m-good");
    let current = mk_version(&[good.id], files, DEVICE, REVERT_MSG);
    assert_ne!(good.id, current.id, "the ids differ");
    assert_eq!(tree_of(&good), tree_of(&current), "the bytes are identical");

    let mut plane = FixturePlane::default();
    plane.set_current(&id, served(WS, &id, current.id, 6));
    plane.add_version(&id, &good);
    plane.add_version(&id, &current);

    let posts = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let receipt = ok_revert_receipt(served(WS, &id, current.id, 6));
    let connect = {
        let posts = posts.clone();
        move |_b: &str, _c: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
            Box::new(RecordingContribute {
                posts: posts.clone(),
                receipt: receipt.clone(),
            })
        }
    };
    let good_hex = to_hex(&good.id);
    let ctx = rig.ctx(&plane, &foll);

    // Both bare and `--yes` are a typed no-op that mints no forward commit and POSTs nothing — the
    // pre-fix id compare (good.id != current.id) would have minted one.
    let bare = ops::revert(&ctx, &connect, &name, &good_hex, false, None).unwrap();
    assert!(
        matches!(&bare, ops::RevertOutcome::NoOp(d) if d.is_noop),
        "bare: {bare:?}"
    );
    let yes = ops::revert(&ctx, &connect, &name, &good_hex, true, None).unwrap();
    assert!(
        matches!(yes, ops::RevertOutcome::NoOp(_)),
        "--yes acknowledges the no-op"
    );
    assert_eq!(posts.get(), 0, "a no-op POSTs nothing on either path");
}

// ---------------------------------------------------------------------------------------------
// The settled-draft fan-out: one bundle, one scope, several agent folders. A draft that is
// UNCHANGED across two runs is copied onto the bundle's other placements (their recorded
// baselines advance with it); a mid-edit file never spreads; true competitors still freeze.
// ---------------------------------------------------------------------------------------------

/// Append a REPLICA placement row (holding the current lock's bytes) to a tracked skill's map —
/// the multi-folder shape the fan-out serves. The dir is created from `files`.
fn add_replica(rig: &Rig, id: &str, dir: &Path, files: &[(&str, FileMode, &[u8])]) {
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    write_tree(dir, files);
    let sp = rig.layout().published(&sid(id));
    let lock: topos_types::persisted::Lock = doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(dir.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: None,
        materialized_sha: Some(lock.bundle_digest),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();
}

/// Append a STALE-CLEAN replica: the dir holds `files` and its recorded baseline names EXACTLY
/// those bytes, so it scans CLEAN (no local edit to protect) — but at an older version than the
/// lock's base. That is the crash-window residue a local converge REFRESHES.
fn add_stale_replica(rig: &Rig, id: &str, dir: &Path, files: &[(&str, FileMode, &[u8])]) {
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    write_tree(dir, files);
    let own = to_hex(&crate::scan::scan(dir).unwrap().bundle_digest);
    let sp = rig.layout().published(&sid(id));
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(dir.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: None,
        materialized_sha: Some(own),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();
}

/// The standard fan-out rig: a followed skill fast-forwarded to v1 with a byte-identical replica
/// folder beside the primary placement.
fn fanout_rig(tag: &str) -> (Rig, String, FixturePlane, FixtureFollow, PathBuf) {
    let rig = Rig::new(tag);
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    {
        let ctx = rig.ctx(&plane, &foll);
        assert_eq!(
            only(&pull_data(&ctx, ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::FastForwarded
        );
    }
    let replica = rig.work.0.join("replica");
    add_replica(&rig, &id, &replica, V1);
    (rig, id, plane, foll, replica)
}

#[test]
fn a_settled_draft_spreads_and_advances_the_sibling_baselines() {
    let (rig, id, plane, foll, replica) = fanout_rig("spread");
    let ctx = rig.ctx(&plane, &foll);

    // THE draft: edit the primary copy.
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();

    // Sweep 1: the observation only — a first sighting never spreads.
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::UpToDate);
    assert_eq!(
        snapshot(&replica),
        Some(expect(V1)),
        "no spread on the first sighting"
    );
    let d_hex = to_hex(&crate::scan::scan(&rig.placement()).unwrap().bundle_digest);
    assert_eq!(
        rig.read_sync(&id).draft_observed.as_deref(),
        Some(d_hex.as_str()),
        "the observation is durable"
    );

    // Sweep 2: SETTLED — the draft lands on the replica, disclosed on the DISCLOSURE channel (it
    // worked; the warning channel is what the receipt counts as failures).
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(row.action, PullAction::DraftSynced);
    assert_eq!(row.synced_placements, Some(1));
    // THE EDITS STILL STAND, and the row says so. The draft flag was attached below this arm's
    // early returns, so the two rows that leave early — a settled draft fanning out, and a folder
    // this run re-created — reported `draft: false` about a machine `list` was calling drafted.
    assert!(
        row.draft,
        "a fan-out moves the draft around; it does not end it: {row:?}"
    );
    assert_eq!(
        row.destinations,
        vec![replica.display().to_string()],
        "the landed folder is named — the destination convention"
    );
    assert!(
        crate::message::legacy_lines(&out.disclosures)
            .into_iter()
            .any(|w| w.starts_with("DRAFT_SYNCED") && w.contains("1 other folder")),
        "{:?}",
        out.disclosures
    );
    assert!(
        out.warnings.is_empty(),
        "a successful fan-out reports no failure: {:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# my draft\n"
    );
    // The draft copy is untouched; the LOCK still names the pristine v1 (the draft detector's
    // reference did not move); the replica's recorded baseline is the DRAFT digest now.
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"# my draft\n"
    );
    let sp = rig.layout().published(&sid(&id));
    let map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    assert_eq!(
        map.placement_state[1].materialized_sha.as_deref(),
        Some(d_hex.as_str()),
        "the synced folder's baseline advances to the draft"
    );
    // A changed sweep for the hook: the fan-out counts as moved bytes.
    assert!(ops::sweep_changed_bytes(&out.data));

    // Sweep 3: a stable fixpoint — everything already carries the draft.
    assert_eq!(
        only(&pull_data(&ctx, ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::UpToDate
    );

    // A LATER edit at the SYNCED replica is a fresh draft against its advanced baseline — the
    // primary (still holding the old draft) is stale behind it, never a competitor: no freeze.
    std::fs::write(replica.join("SKILL.md"), b"# my draft, refined\n").unwrap();
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(only(&out.data).action, PullAction::UpToDate);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"# my draft\n",
        "the stale copy is untouched until the refined draft settles"
    );
}

#[test]
fn a_converge_landing_survives_the_same_runs_settled_fanout() {
    // The corruption regression: ONE run both CONVERGES a reservation (a never-materialized
    // placement first-installs from the local store) and runs the settled-draft fan-out. The
    // fan-out's materialize must commit the POST-converge map — handing it the stale pre-converge
    // map would erase the reservation's just-recorded baseline, and the next scan would classify
    // the installed dir as FOREIGN (a managed placement lost to its own sweep).
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    let (rig, id, plane, foll, replica) = fanout_rig("converge-settle");
    let ctx = rig.ctx(&plane, &foll);

    // THE draft, observed once (sweep 1) — the next sweep's fan-out will see it settled.
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    // A RESERVATION row: recorded, never materialized, dir absent — exactly what the converge
    // first-installs (a newly added target).
    let extra = rig.work.0.join("extra");
    let sp = rig.layout().published(&sid(&id));
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(extra.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: Some("extra".to_owned()),
        materialized_sha: None,
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();

    // Sweep 2: the converge lands `extra` at the pristine v1 AND the settled draft spreads onto
    // the replica — in ONE run.
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        only(&out.data).action,
        PullAction::DraftSynced,
        "{:?}",
        out.warnings
    );
    assert_eq!(
        snapshot(&extra),
        Some(expect(V1)),
        "the converge landed the reservation from the local store"
    );
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# my draft\n",
        "the settled draft spread onto the replica in the same run"
    );
    // THE regression assertion: the converge's recorded baseline SURVIVES the fan-out's commit.
    let map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    let extra_idx = map
        .placements
        .iter()
        .position(|p| Path::new(p) == extra)
        .expect("the reservation row is still recorded");
    let v1_digest = {
        let lock: topos_types::persisted::Lock = doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
        lock.bundle_digest
    };
    assert_eq!(
        map.placement_state[extra_idx].materialized_sha.as_deref(),
        Some(v1_digest.as_str()),
        "the just-converged placement stays MANAGED — the fan-out must not commit a stale map"
    );

    // And the placement keeps behaving managed: the next sweep spreads the settled draft onto it
    // (a clean copy stale behind the draft), never reading it as foreign.
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        std::fs::read(extra.join("SKILL.md")).unwrap(),
        b"# my draft\n",
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_refreshed_stale_replica_never_reads_all_up_to_date() {
    // The crash-window residue: a managed copy whose bytes AND recorded baseline sit at an OLDER
    // version than the one this machine holds. The converge rewrites it — bytes moved on disk, so
    // the run may not claim "all up to date". A refresh is not a first install, so the row says
    // `updated` and names WHERE THE BUNDLE NOW STANDS — every copy at the applied version, not
    // just the one the converge happened to rewrite — and the quiet hook's changed-bytes signal
    // fires.
    let (rig, id, plane, foll, replica) = fanout_rig("stale-refresh");
    let stale = rig.work.0.join("stale");
    add_stale_replica(&rig, &id, &stale, BASE);
    let ctx = rig.ctx(&plane, &foll);

    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(
        row.action,
        PullAction::Refreshed,
        "a rewritten stale copy is a refresh, never up-to-date: {row:?}"
    );
    // THE anti-regression: three folders hold this bundle and only one needed rewriting. A row
    // that named the rewritten one alone would read as if the other two had gone.
    let sp = rig.layout().published(&sid(&id));
    let placements = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap().placements;
    assert_eq!(placements.len(), 3, "{placements:?}");
    assert_eq!(
        row.destinations, placements,
        "every copy now holding the applied version is named, in map order"
    );
    for named in [&stale, &replica] {
        assert!(
            row.destinations.contains(&named.display().to_string()),
            "{:?} names {}",
            row.destinations,
            named.display()
        );
    }
    assert_eq!(row.note, None, "nothing else was written this run");
    assert_eq!(
        snapshot(&stale),
        Some(expect(V1)),
        "the stale copy now holds the version this machine applied"
    );
    // No version moved: the refresh is purely a local catch-up.
    assert_eq!(row.observed, row.applied);
    assert!(
        ops::sweep_changed_bytes(&out.data),
        "the quiet hook must hear about bytes that moved"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(tty.contains("updated (3 folders)"), "{tty}");
    // Counted rows spell their folders out — a number nobody can act on is not an answer.
    assert!(
        tty.contains(&format!("\n    {}\n", stale.display())),
        "{tty}"
    );
    assert!(
        !tty.contains("refreshed"),
        "the internal word never reaches a person: {tty}"
    );
    assert!(
        !tty.contains("all up to date"),
        "a run that rewrote a folder may not claim it: {tty}"
    );
    assert!(
        !tty.contains("installed"),
        "a refresh is not an install: {tty}"
    );

    // Refreshed and untouched again → the honest all-up-to-date summary is back.
    let again = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&again.data).action, PullAction::UpToDate);
    assert!(!ops::sweep_changed_bytes(&again.data));
    assert!(
        crate::render::pull_tty(
            &again.data,
            &again.decisions,
            &again.warnings,
            &again.advisories,
            &again.disclosures,
            0,
        )
        .contains("all up to date")
    );
}

#[test]
fn a_heal_riding_along_with_a_settled_fanout_is_named_on_the_row() {
    // ONE run both HEALS an absent placement (the converge first-installs it from the local
    // store) and spreads a settled draft onto a sibling. The fan-out chose its targets from the
    // pre-converge scan, so it SKIPS the just-healed dir — and the `synced` column cannot name it.
    // The row states it as its second fact: nothing this run wrote goes unnamed until the next
    // sweep.
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    let (rig, id, plane, foll, replica) = fanout_rig("heal-fanout");
    let ctx = rig.ctx(&plane, &foll);

    // THE draft, observed once (sweep 1) — the next sweep's fan-out sees it settled.
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    // A recorded placement whose dir is absent (a hand-deleted folder, a fresh target).
    let extra = rig.work.0.join("extra");
    let sp = rig.layout().published(&sid(&id));
    let mut map = doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(extra.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: Some("extra".to_owned()),
        materialized_sha: None,
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    doc::write_map(&rig.fs, &sp.map, &map).unwrap();

    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(row.action, PullAction::DraftSynced, "{:?}", out.warnings);
    assert_eq!(
        row.destinations,
        vec![replica.display().to_string()],
        "the fan-out's own column names only what took the draft"
    );
    assert_eq!(
        row.note.as_deref(),
        Some(format!("also installed {}", extra.display()).as_str()),
        "the healed folder is named on the row: {row:?}"
    );
    assert_eq!(
        snapshot(&extra),
        Some(expect(V1)),
        "the heal landed the pristine version from the local store"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(tty.contains("synced your edits to"), "{tty}");
    assert!(
        tty.contains(&format!("also installed {}", extra.display())),
        "{tty}"
    );
    assert!(!tty.contains("all up to date"), "{tty}");
    assert!(ops::sweep_changed_bytes(&out.data));
}

#[test]
fn an_unsettled_draft_never_spreads() {
    let (rig, id, plane, foll, replica) = fanout_rig("unsettled");
    let ctx = rig.ctx(&plane, &foll);

    std::fs::write(rig.placement().join("SKILL.md"), b"# edit one\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap(); // observes edit one

    // The file MOVES between sweeps: only the observation updates — nothing spreads.
    std::fs::write(rig.placement().join("SKILL.md"), b"# edit two\n").unwrap();
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::UpToDate);
    assert_eq!(
        snapshot(&replica),
        Some(expect(V1)),
        "a mid-edit file never spreads"
    );
    let d2 = to_hex(&crate::scan::scan(&rig.placement()).unwrap().bundle_digest);
    assert_eq!(
        rig.read_sync(&id).draft_observed.as_deref(),
        Some(d2.as_str())
    );

    // Once it settles, the NEXT sweep spreads exactly the settled bytes.
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::DraftSynced);
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# edit two\n"
    );
}

/// The OTHER early return out of the no-pending-update arm: a folder this run RE-CREATED. Healing
/// a hand-deleted copy is an install, and the row said so — while dropping the fact that the
/// person's edits are still sitting in the copy that survived. A receipt that reports an install
/// and denies the draft sends an agent to `update` when the answer is `publish`.
#[test]
fn a_healed_folder_still_reports_the_draft_standing_beside_it() {
    let (rig, id, plane, foll, replica) = fanout_rig("healed-draft");
    let ctx = rig.ctx(&plane, &foll);

    // The draft, unsettled (first sighting — the fan-out deliberately does not run).
    std::fs::write(rig.placement().join("SKILL.md"), b"# my draft\n").unwrap();
    // …and the sibling copy is hand-deleted, so the same sweep must re-create it.
    std::fs::remove_dir_all(&replica).unwrap();

    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&out.data);
    assert_eq!(row.action, PullAction::Installed, "{row:?}");
    assert!(replica.is_dir(), "the hand-deleted folder is healed");
    assert!(
        row.draft,
        "the heal is not the whole news — the edits still stand: {row:?}"
    );
    let _ = id;
}

#[test]
fn true_competitors_freeze_and_the_fanout_never_runs() {
    let (rig, id, plane, foll, replica) = fanout_rig("compete");
    let ctx = rig.ctx(&plane, &foll);

    std::fs::write(rig.placement().join("SKILL.md"), b"# primary edit\n").unwrap();
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap(); // observes the primary draft

    // A DIFFERENT edit lands in the replica: two contents, neither at the other's baseline —
    // true competitors. The sweep freezes the skill (typed, isolated) and syncs NOTHING.
    std::fs::write(replica.join("SKILL.md"), b"# replica edit\n").unwrap();
    let out = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("PLACEMENTS_DIVERGED")),
        "{:?}",
        out.warnings
    );
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"# primary edit\n"
    );
    assert_eq!(
        std::fs::read(replica.join("SKILL.md")).unwrap(),
        b"# replica edit\n"
    );
    let _ = id;
}
