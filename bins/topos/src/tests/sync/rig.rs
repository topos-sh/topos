//! The shared rig every `sync` suite runs on: the scratch dirs, the fixture plane/follow
//! transports, version construction, the topos home + workspace `Rig`, and the bundle fixtures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::identity::{self, Commit};
use topos_types::persisted::SyncState;
use topos_types::results::PullData;
use topos_types::{CurrentRecord, PointerScope, WireCurrentRecord};

use crate::ctx::Ctx;
use crate::fs_seam::{FsOps, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    FollowContext, FollowSource, InertFollow, InertPlane, KnownCurrent, PlaneError, PlaneSource,
    PointerFetch,
};
use crate::sidecar::Layout;
use crate::test_support::MockHarness;
use crate::{doc, ops};

pub(super) const WS: &str = "w_acme";
pub(super) const DEVICE: &str = "d_test";

// ---------------------------------------------------------------------------------------------
// Scratch + fixtures.
// ---------------------------------------------------------------------------------------------

pub(super) struct Scratch(pub(super) PathBuf);
impl Scratch {
    pub(super) fn new(tag: &str) -> Self {
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

/// A minimal harness stub — the engine reads the placement from `map.json`, never the adapter, so it
/// is never asked for a dir during a pull (and `add` of a plain dir does not recognize it).
pub(super) fn no_harness() -> MockHarness {
    MockHarness::joining("")
}

#[derive(Default)]
pub(super) struct FixturePlane {
    pub(super) records: HashMap<String, WireCurrentRecord>,
    pub(super) versions: HashMap<(String, String), crate::plane::FetchedVersion>,
}
impl FixturePlane {
    pub(super) fn set_current(&mut self, skill: &str, rec: WireCurrentRecord) {
        self.records.insert(skill.to_owned(), rec);
    }
    pub(super) fn add_version(&mut self, skill: &str, v: &Version) {
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

pub(super) struct FixtureFollow {
    pub(super) entries: Vec<(String, FollowContext)>,
}
impl FollowSource for FixtureFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        self.entries.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Version construction + signing.
// ---------------------------------------------------------------------------------------------

pub(super) struct Version {
    pub(super) id: [u8; 32],
    pub(super) fetched: crate::plane::FetchedVersion,
}

pub(super) fn mk_version(
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
pub(super) fn served(
    ws: &str,
    skill: &str,
    version_id: [u8; 32],
    generation: u64,
) -> WireCurrentRecord {
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

pub(super) struct Rig {
    pub(super) home: Scratch,
    pub(super) work: Scratch,
    pub(super) fs: RealFs,
    pub(super) ids: SeqIds,
    pub(super) clock: FixedClock,
    pub(super) harness: MockHarness,
}
impl Rig {
    pub(super) fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1),
            harness: no_harness(),
        }
    }
    pub(super) fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }
    pub(super) fn ctx<'a>(
        &'a self,
        plane: &'a dyn PlaneSource,
        follow: &'a dyn FollowSource,
    ) -> Ctx<'a> {
        self.ctx_fs(&self.fs, plane, follow)
    }
    /// A [`Ctx`] over an arbitrary LAYOUT — the hostile-checkout tests drive a PROJECT store,
    /// whose `.topos/` (and therefore every document a resolution reads) travels with the clone.
    pub(super) fn ctx_at<'a>(
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
    pub(super) fn ctx_fs<'a>(
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
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane,
            follow,
            roots: None,
        }
    }
    /// Adopt a skill from the work dir (returns its id, name, and genesis version id).
    pub(super) fn adopt(&self, base: &[(&str, FileMode, &[u8])]) -> (String, String, [u8; 32]) {
        let dir = self.work.0.join("pr-describe");
        write_tree(&dir, base);
        let inert_p = InertPlane;
        let inert_f = InertFollow;
        let ctx = self.ctx(&inert_p, &inert_f);
        let data = ops::add(&ctx, &dir).unwrap();
        let genesis = ops::parse_hex32(data.version_id.as_deref().unwrap()).unwrap();
        (data.skill_id.unwrap(), data.name, genesis)
    }
    pub(super) fn placement(&self) -> PathBuf {
        self.work.0.join("pr-describe")
    }
    pub(super) fn read_sync(&self, id: &str) -> SyncState {
        doc::read_doc(&self.fs, &self.layout().published(&sid(id)).sync)
            .unwrap()
            .unwrap()
    }
    pub(super) fn patch_sync(&self, id: &str, f: impl FnOnce(&mut SyncState)) {
        let mut s = self.read_sync(id);
        f(&mut s);
        doc::write_doc(&self.fs, &self.layout().published(&sid(id)).sync, &s).unwrap();
    }
    pub(super) fn patch_map(
        &self,
        id: &str,
        f: impl FnOnce(&mut topos_types::persisted::PlacementMap),
    ) {
        let p = self.layout().published(&sid(id)).map;
        let mut m = doc::read_map(&self.fs, &p).unwrap().unwrap();
        f(&mut m);
        doc::write_map(&self.fs, &p, &m).unwrap();
    }
    pub(super) fn patch_lock(&self, id: &str, f: impl FnOnce(&mut topos_types::persisted::Lock)) {
        let p = self.layout().published(&sid(id)).lock;
        let mut l: topos_types::persisted::Lock = doc::read_doc(&self.fs, &p).unwrap().unwrap();
        f(&mut l);
        doc::write_doc(&self.fs, &p, &l).unwrap();
    }
    pub(super) fn open_store(&self, id: &str) -> topos_gitstore::Store {
        topos_gitstore::Store::open(&self.layout().published(&sid(id)).store).unwrap()
    }
    pub(super) fn conflict_exists(&self, id: &str) -> bool {
        self.layout().published(&sid(id)).conflict.exists()
    }
    pub(super) fn conflict_state(&self, id: &str) -> topos_types::persisted::ConflictState {
        doc::read_doc(&self.fs, &self.layout().published(&sid(id)).conflict)
            .unwrap()
            .expect("a recorded conflict")
    }
    /// Where the marked-up copy of a recorded conflict lives — read from the record itself, so the
    /// test asserts against the path the code actually documented, never a guess.
    pub(super) fn conflict_copy(&self, id: &str) -> PathBuf {
        self.layout().conflict_copy_dir(
            &crate::sidecar::ConflictDir::parse(
                self.conflict_state(id).copy_dir.as_deref().unwrap(),
            )
            .expect("a recorded workbench component parses"),
        )
    }
}

/// Parse a rig-minted skill id through the validated newtype (always charset-clean here).
pub(super) fn sid(id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(id).expect("rig skill id is charset-clean")
}

/// The test shim over [`ops::pull`]: project the schema payload (the envelope warnings have their own
/// dedicated tests below).
pub(super) fn pull_data(
    ctx: &Ctx<'_>,
    scope: ops::PullScope,
) -> Result<PullData, crate::error::ClientError> {
    ops::pull(ctx, scope).map(|o| o.data)
}

pub(super) fn follow(skill_id: &str) -> FixtureFollow {
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

pub(super) fn write_tree(dir: &Path, files: &[(&str, FileMode, &[u8])]) {
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

pub(super) fn snapshot(dir: &Path) -> Option<Vec<(String, Vec<u8>)>> {
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
pub(super) fn rendered(files: &[(&str, FileMode, &[u8])]) -> topos_gitstore::RenderedBundle {
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
pub(super) fn zero_hex() -> String {
    "0".repeat(64)
}

pub(super) fn expect(files: &[(&str, FileMode, &[u8])]) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(p, _, b)| ((*p).to_owned(), b.to_vec()))
        .collect();
    v.sort();
    v
}

pub(super) fn only(data: &PullData) -> &topos_types::results::PullSkill {
    assert_eq!(data.skills.len(), 1, "expected exactly one skill row");
    &data.skills[0]
}

pub(super) const BASE: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# v0\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
];
pub(super) const V1: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# v1\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
    ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
];
/// A draft over [`BASE`] that rewrites `SKILL.md` and nothing else — the shape every
/// conflict-with-[`V1`] fixture below uses.
pub(super) const MINE_OVER_BASE: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# mine\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
];
/// What `--keep-mine` commits for [`MINE_OVER_BASE`] against [`V1`]: this person's `SKILL.md` (the
/// one file both sides rewrote), plus everything V1 changed that they did not touch — `run.sh`
/// caught up, and the file V1 added. `git merge -X ours`, not `-s ours`.
pub(super) const KEPT_OVER_V1: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# mine\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
    ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
];

/// A static bundle fixture (path, mode, bytes).
pub(super) type FileSet = &'static [(&'static str, FileMode, &'static [u8])];

/// `topos update <name> --keep-mine`, as a scope.
pub(super) fn keep_mine_scope(name: String) -> ops::PullScope {
    ops::PullScope::One {
        store: ops::StoreScope::Here,
        name,
        workspace: None,
        mode: ops::TargetMode::KeepMine,
    }
}

/// Append a REPLICA placement row (holding the current lock's bytes) to a tracked skill's map —
/// the multi-folder shape the fan-out serves. The dir is created from `files`.
pub(super) fn add_replica(rig: &Rig, id: &str, dir: &Path, files: &[(&str, FileMode, &[u8])]) {
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
