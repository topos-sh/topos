//! End-to-end tests of the pull/apply engine against a real git store + fixture plane responses (no
//! HTTP). These exercise the release-blocker invariants: the anti-rollback floor (F), never-clobber-draft
//! (D), the reused-tuple ALARM (G), the crash-after-swap heal, cross-workspace replay, go-back/resume, and
//! the confirm-each offer→accept (APPLY_WAITING_UPDATE), all through the public `ops::pull` entry point.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::sign::{self, Commit, CurrentPointer};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::{RecordedTuple, SyncState};
use topos_types::results::{PullAction, PullData};
use topos_types::{
    CurrencyKind, CurrentRecord, Generation, HarnessId, PointerScope, Signature, SignatureAlg,
    SignedCurrentRecord, TriggerReport, TriggerState,
};

use crate::ctx::Ctx;
use crate::fs_seam::{FaultFs, FsOps, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    FollowContext, FollowMode, FollowSource, InertFollow, InertPlane, KnownCurrent, PlaneError,
    PlaneSource, PointerFetch,
};
use crate::sidecar::Layout;
use crate::{doc, ops};

const WS: &str = "w_acme";
const DEVICE: &str = "d_test";
const PLANE_SEED: [u8; 32] = [7u8; 32];

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
    fn placement_for(&self, skill_id: &str, _d: Option<&DiscoveredPlacement>) -> PlacementTarget {
        PlacementTarget {
            dir: PathBuf::from(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        report()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        report()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
fn report() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test".into(),
        state: TriggerState::Inactive,
    }
}

#[derive(Default)]
struct FixturePlane {
    records: HashMap<String, SignedCurrentRecord>,
    versions: HashMap<(String, String), crate::plane::FetchedVersion>,
}
impl FixturePlane {
    fn set_current(&mut self, skill: &str, rec: SignedCurrentRecord) {
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
            && k.generation.epoch == rec.record.generation.epoch
            && k.generation.seq == rec.record.generation.seq
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
    let id = sign::commit_id(&Commit {
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

fn plane_pubkey() -> [u8; 32] {
    SigningKey::from_bytes(&PLANE_SEED)
        .verifying_key()
        .to_bytes()
}

/// A correctly-signed `current` record for the given scope + version + generation.
fn signed(
    ws: &str,
    skill: &str,
    version_id: [u8; 32],
    epoch: u64,
    seq: u64,
) -> SignedCurrentRecord {
    let pointer = CurrentPointer {
        workspace_id: ws,
        skill_id: skill,
        version_id,
        epoch,
        seq,
    };
    let msg = sign::pointer_preimage(&pointer).unwrap();
    let sig = SigningKey::from_bytes(&PLANE_SEED).sign(msg.as_bytes());
    let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes());
    SignedCurrentRecord {
        schema_version: 1,
        scope: PointerScope {
            workspace_id: ws.to_owned(),
            skill_id: skill.to_owned(),
        },
        record: CurrentRecord {
            version_id: to_hex(&version_id),
            generation: Generation { epoch, seq },
        },
        signature: Signature {
            alg: SignatureAlg::Ed25519,
            key_id: "plane".to_owned(),
            value,
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
    /// A [`Ctx`] over an arbitrary [`FsOps`] (the crash gate injects a [`FaultFs`]).
    fn ctx_fs<'a>(
        &'a self,
        fs: &'a dyn FsOps,
        plane: &'a dyn PlaneSource,
        follow: &'a dyn FollowSource,
    ) -> Ctx<'a> {
        Ctx {
            fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE.to_owned(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            plane_key: plane_pubkey(),
            follow,
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
        let genesis = ops::parse_hex32(&data.version_id).unwrap();
        (data.skill_id, data.name, genesis)
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

fn follow(skill_id: &str, mode: FollowMode) -> FixtureFollow {
    FixtureFollow {
        entries: vec![(
            skill_id.to_owned(),
            FollowContext {
                workspace_id: WS.to_owned(),
                mode,
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    let row = only(&data);
    assert_eq!(row.action, PullAction::FastForwarded);
    assert_eq!(row.applied, Generation { epoch: 1, seq: 1 });
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

#[test]
fn confirm_each_offers_then_explicit_pull_accepts() {
    let rig = Rig::new("confirm");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::ConfirmEach);

    // The bare sweep OFFERS (raises the floor) but does not apply.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Offered);
    assert!(row.offer.is_some());
    assert_eq!(
        row.observed,
        Generation { epoch: 1, seq: 1 },
        "floor raised"
    );
    assert_eq!(row.applied, Generation { epoch: 0, seq: 0 }, "not applied");
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "bytes untouched"
    );

    // The explicit `pull <skill>` accepts the pending update (APPLY_WAITING_UPDATE).
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            name,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::FastForwarded);
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    assert_eq!(rig.read_sync(&id).applied, Generation { epoch: 1, seq: 1 });
}

/// Full-auto: an AUTO follower's bare sweep RESOLVES a diverged draft. Here the local edit
/// overlaps theirs' edit to `SKILL.md`, so the merge conflicts: the complete conflict tree is materialized
/// (markers carrying BOTH sides, the other files merged clean), the draft is snapshotted recoverably, and
/// a durable conflict record blocks publish. The edit is never lost — it lives inside the markers.
#[test]
fn auto_sweep_resolves_a_diverged_draft_into_a_conflict_tree() {
    let rig = Rig::new("diverge");
    let (id, _name, genesis) = rig.adopt(BASE);
    // Edit SKILL.md (overlaps theirs' SKILL.md edit → a conflict) and leave run.sh at base.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);

    // Resolved (not merely surfaced): a conflict, with a merge report listing the conflicting path.
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(!mr.clean);
    assert_eq!(mr.theirs_version_id, to_hex(&v1.id));
    assert!(mr.conflicts.iter().any(|c| c.path == "SKILL.md"));

    // The COMPLETE conflict tree is on the placement: SKILL.md has diff3 markers carrying BOTH sides; the
    // non-overlapping files are merged clean (run.sh → theirs, the new ref/notes.md → theirs).
    let skill = std::fs::read_to_string(rig.placement().join("SKILL.md")).unwrap();
    assert!(
        skill.contains("<<<<<<<") && skill.contains(">>>>>>>"),
        "{skill}"
    );
    assert!(
        skill.contains("my local edit") && skill.contains("# v1"),
        "the edit must survive inside the markers: {skill}"
    );
    assert_eq!(
        std::fs::read(rig.placement().join("run.sh")).unwrap(),
        b"#!/bin/sh\necho v1\n"
    );
    assert!(rig.placement().join("ref/notes.md").exists());

    // Never clobbered: the pre-merge draft is snapshotted into the sidecar store (recoverable).
    let draft = mk_version(&[genesis], edited, DEVICE, "topos: draft snapshot");
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid(&id)).store).unwrap();
    assert!(
        store.list_versions().unwrap().contains(&draft.id),
        "the diverged draft must be snapshotted before the merge overwrites the placement"
    );

    // A durable conflict record blocks publish; the pending update is consumed into the (blocked) draft.
    assert!(rig.layout().published(&sid(&id)).conflict.exists());
    assert_eq!(rig.read_sync(&id).applied, Generation { epoch: 1, seq: 1 });
}

#[test]
fn go_back_then_resume() {
    let rig = Rig::new("goback");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    // Fast-forward to v1.
    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    // Go back to genesis: old bytes installed, `held` set, the floor (`observed`) untouched.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            name: name.clone(),
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
    assert_eq!(
        s.observed,
        Generation { epoch: 1, seq: 1 },
        "floor NOT lowered"
    );
    assert_eq!(
        s.applied,
        Generation { epoch: 0, seq: 0 },
        "applied dropped to the old gen"
    );

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
            name,
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
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1, "the @-named skill resolved");

    // Neither interpretation tracked → the typed NoSuchSkill names the FULL argument the user typed.
    let err = match crate::app::pull_with_name_fallback(
        &rig.ctx(&inert_p, &inert_f),
        Some("nope@abcdef12".to_owned()),
        false,
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    let out = crate::app::pull_with_name_fallback(
        &rig.ctx(&plane, &foll),
        Some(format!("{name}@{}", to_hex(&genesis))),
        false,
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            name: name.clone(),
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
            name,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Prefix("ffffffffffff".into())),
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "UNKNOWN_GOBACK_VERSION");
}

#[test]
fn downgrade_floor_holds_and_drives_to_observed() {
    // F: the floor is at v2 while `applied` is stuck at v1 (a prior apply failed); a served OLDER signed
    // pointer must not downgrade, and the client drives forward to the floor target (v2), backfilling v1.
    let rig = Rig::new("downgrade");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2files: &[(&str, FileMode, &[u8])] = &[("SKILL.md", FileMode::Regular, b"# v2\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    // The plane (attacker/CDN) serves the OLDER pointer v1@(1,1).
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));

    // The client already authenticated the floor at (1,2)=v2 but is materialized at v1 (apply failed).
    rig.patch_sync(&id, |s| {
        s.observed = Generation { epoch: 1, seq: 2 };
        s.applied = Generation { epoch: 1, seq: 1 };
        s.recorded = vec![
            RecordedTuple {
                generation: Generation { epoch: 0, seq: 0 },
                commit_id: to_hex(&genesis),
            },
            RecordedTuple {
                generation: Generation { epoch: 1, seq: 1 },
                commit_id: to_hex(&v1.id),
            },
            RecordedTuple {
                generation: Generation { epoch: 1, seq: 2 },
                commit_id: to_hex(&v2.id),
            },
        ];
        s.base_commit = to_hex(&genesis);
    });

    let foll = follow(&id, FollowMode::Auto);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    // The served v1 is a no-op (no downgrade); the client reaches the floor target v2.
    assert_eq!(row.action, PullAction::FastForwarded);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(v2files)),
        "reached v2, NOT downgraded to v1"
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        s.observed,
        Generation { epoch: 1, seq: 2 },
        "floor unchanged"
    );
}

#[test]
fn reused_tuple_raises_alarm() {
    // G: the client is at the floor (1,2); the plane (restored, no epoch bump) re-serves (1,1) naming a
    // DIFFERENT commit than recorded[(1,1)] → a loud ALARM; nothing is applied.
    let rig = Rig::new("alarm");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let evil = mk_version(
        &[genesis],
        &[("SKILL.md", FileMode::Regular, b"# EVIL\n")],
        "d_evil",
        "evil",
    );

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &evil);
    // The plane re-serves (1,1) but pointing at `evil` (not the recorded v1).
    plane.set_current(&id, signed(WS, &id, evil.id, 1, 1));

    rig.patch_sync(&id, |s| {
        s.observed = Generation { epoch: 1, seq: 2 };
        s.applied = Generation { epoch: 1, seq: 2 };
        s.recorded = vec![
            RecordedTuple {
                generation: Generation { epoch: 0, seq: 0 },
                commit_id: to_hex(&genesis),
            },
            RecordedTuple {
                generation: Generation { epoch: 1, seq: 1 },
                commit_id: to_hex(&v1.id),
            },
            RecordedTuple {
                generation: Generation { epoch: 1, seq: 2 },
                commit_id: to_hex(&v1.id),
            },
        ];
        s.base_commit = to_hex(&v1.id);
    });
    let before = snapshot(&rig.placement());

    let foll = follow(&id, FollowMode::Auto);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::Alarm);
    assert_eq!(
        snapshot(&rig.placement()),
        before,
        "nothing applied on an alarm"
    );
    // The floor is unchanged (the reused lower tuple cannot move it).
    assert_eq!(rig.read_sync(&id).observed, Generation { epoch: 1, seq: 2 });
}

#[test]
fn cross_workspace_pointer_is_rejected() {
    // A pointer correctly signed by the plane key but scoped to ANOTHER workspace must not apply, even for
    // the same skill id and key (a cross-workspace replay).
    let rig = Rig::new("xws");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed("w_other", &id, v1.id, 1, 1)); // wrong workspace scope

    let foll = follow(&id, FollowMode::Auto);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::Alarm);
    assert_eq!(snapshot(&rig.placement()), Some(expect(BASE)), "untouched");
    assert_eq!(
        rig.read_sync(&id).observed,
        Generation { epoch: 0, seq: 0 },
        "floor not raised"
    );
}

#[test]
fn crash_after_swap_heals_without_false_divergence() {
    // The bytes were swapped to v1 but `applied` never advanced (a crash between the swap and the sync
    // write). The next pull must HEAL forward (advance `applied`), never show a false DIVERGED panel.
    let rig = Rig::new("heal");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    // Simulate the post-swap, pre-commit state: placement holds v1 bytes; sync says observed=(1,1) but
    // applied still (0,0); recorded has v1.
    write_tree(&rig.placement(), V1);
    rig.patch_sync(&id, |s| {
        s.observed = Generation { epoch: 1, seq: 1 };
        s.applied = Generation { epoch: 0, seq: 0 };
        s.recorded.push(RecordedTuple {
            generation: Generation { epoch: 1, seq: 1 },
            commit_id: to_hex(&v1.id),
        });
        // base/work still describe genesis (the docs never advanced).
    });

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::FastForwarded,
        "healed, not diverged"
    );
    assert_ne!(row.action, PullAction::Diverged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    assert_eq!(rig.read_sync(&id).applied, Generation { epoch: 1, seq: 1 });
}

#[test]
fn at_floor_tuple_reuse_is_an_alarm() {
    // The plane re-serves the SAME (epoch,seq) the client already holds, but naming a DIFFERENT commit.
    // The conditional GET must NOT 304 it away (it keys on the commit too) — it is a reused-tuple ALARM.
    let rig = Rig::new("atfloor");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let evil = mk_version(
        &[genesis],
        &[("SKILL.md", FileMode::Regular, b"# EVIL at the same tuple\n")],
        "d_evil",
        "evil",
    );
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
    // Fast-forward to v1 @ (1,1).
    {
        let ctx = rig.ctx(&plane, &foll);
        pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    }
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    // Now re-serve (1,1) but pointing at `evil` (a different commit at the same tuple).
    plane.set_current(&id, signed(WS, &id, evil.id, 1, 1));
    let before = snapshot(&rig.placement());
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        only(&data).action,
        PullAction::Alarm,
        "a same-tuple different-commit record must alarm, not 304"
    );
    assert_eq!(snapshot(&rig.placement()), before, "nothing applied");
}

#[test]
fn confirm_each_accept_reoffers_a_version_that_moved() {
    // A confirm-each skill is offered v1; the plane advances to v2 BEFORE the user accepts. The explicit
    // `pull <skill>` must RE-OFFER v2 (re-disclose its digest), never silently apply the undisclosed v2.
    let rig = Rig::new("moved");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2files: &[(&str, FileMode, &[u8])] = &[("SKILL.md", FileMode::Regular, b"# v2\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::ConfirmEach);
    // The sweep offers v1 (raises the floor, does not apply).
    {
        let ctx = rig.ctx(&plane, &foll);
        let d = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
        assert_eq!(only(&d).action, PullAction::Offered);
    }
    // The plane moves to v2 before the user accepts.
    plane.set_current(&id, signed(WS, &id, v2.id, 1, 2));
    let ctx = rig.ctx(&plane, &foll);
    let d = pull_data(
        &ctx,
        ops::PullScope::One {
            name,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    let row = only(&d);
    assert_eq!(
        row.action,
        PullAction::Offered,
        "a version discovered during the accept is re-offered, not applied"
    );
    assert_eq!(row.offer.as_ref().unwrap().version_id, to_hex(&v2.id));
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "v2 never applied"
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
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
            name,
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
fn malformed_plane_response_is_an_alarm() {
    let rig = Rig::new("malformed");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = MalformedPlane;
    let foll = follow(&id, FollowMode::Auto);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::Alarm);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "nothing applied"
    );
}

#[test]
fn bad_signature_is_an_alarm() {
    let rig = Rig::new("badsig");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    // A record with a valid shape but a tampered signature.
    let mut rec = signed(WS, &id, v1.id, 1, 1);
    rec.signature.value = "A".repeat(86); // wrong (but well-formed base64url) signature
    plane.set_current(&id, rec);

    let foll = follow(&id, FollowMode::Auto);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::Alarm);
    assert_eq!(snapshot(&rig.placement()), Some(expect(BASE)));
    assert_eq!(rig.read_sync(&id).observed, Generation { epoch: 0, seq: 0 });
}

// =================================================================================================
// Author-side merge resolution (the diff3 increment): clean merge, the fixpoint, confirm-each surface,
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

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
    assert_eq!(s.applied, Generation { epoch: 1, seq: 1 });
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
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
    plane2.set_current(&id, signed(WS, &id, v2.id, 1, 2));
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

/// A confirm-each follower's BARE sweep surfaces a divergence — it never auto-merges (that would land
/// theirs-incorporated bytes without the one-tap). The placement is left exactly as the author left it.
#[test]
fn confirm_each_bare_sweep_surfaces_without_merging() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("confirm");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::ConfirmEach);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Diverged);
    assert!(
        row.merge.is_none(),
        "confirm-each bare sweep must not merge"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(mine)),
        "left untouched"
    );
    assert!(!rig.conflict_exists(&id));
    assert_eq!(rig.read_sync(&id).applied, Generation { epoch: 0, seq: 0 });

    // The explicit accept (the one-tap) then runs the merge.
    let accepted = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            name: "pr-describe".into(),
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(only(&accepted).action, PullAction::Merged);
}

/// The disclosed escape (`--onto-current`): commit MY bytes on top of `current`, dropping the merge. It
/// always produces a clean, publishable draft-on-current (no deadlock), discloses what it drops, and the
/// pre-escape bytes stay recoverable in the sidecar store.
#[test]
fn escape_commits_mine_on_current_and_is_publishable() {
    let rig = Rig::new("escape");
    let (id, _name, genesis) = rig.adopt(BASE);
    // An overlapping edit (would conflict if merged) — the escape sidesteps the merge entirely.
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my way\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let data = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            name: "pr-describe".into(),
            mode: ops::TargetMode::OntoCurrent,
        },
    )
    .unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Merged);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(mr.clean);
    assert!(mr.drop_diff.is_some(), "the escape discloses what it drops");

    // The placement holds exactly MY bytes (theirs was dropped); publishable (no conflict record).
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(!rig.conflict_exists(&id));
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(s.base_commit, to_hex(&v1.id));

    // The pre-escape draft is recoverable: MINE re-parented on `current` is a real commit in the store.
    let m = mk_version(&[v1.id], mine, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

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

    // The author edits the markers by hand — the block STILL stands (presence-based, not a marker scan).
    write_tree(
        &rig.placement(),
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

    // The escape resolves it: the block clears + a publishable draft-on-current results.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            name: "pr-describe".into(),
            mode: ops::TargetMode::OntoCurrent,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id), "the escape clears the block");
}

/// Unrelated histories (no renderable base) fall back to a 2-way manual choice — never a silent merge:
/// MINE is kept on disk, a 2-way diff is disclosed, and publish is blocked until the author resolves.
#[test]
fn no_base_falls_back_to_two_way_never_silent() {
    let rig = Rig::new("nobase");
    let (id, _name, genesis) = rig.adopt(BASE);
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(!mr.clean);
    assert!(mr.drop_diff.is_some(), "a 2-way diff is disclosed");
    // MINE is never silently overwritten by theirs.
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(rig.conflict_exists(&id));
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
        plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
        let foll = follow(&id, FollowMode::Auto);
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
        plane.set_current(&id, signed(WS, &id, genesis, 0, 0));
        let foll = follow(&id, FollowMode::Auto);
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

/// A binary (non-UTF-8) file diverging three ways is never line-merged: theirs is kept at the path and
/// mine in a `.topos-mine` sidecar, and the materialized tree scans back to the recorded conflict digest
/// (the sidecar round-trips through the scanner — the heal signal stays valid).
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    // theirs kept at the path, mine in the sidecar.
    assert_eq!(
        std::fs::read(rig.placement().join("logo.bin")).unwrap(),
        &[0xffu8, 7, 7]
    );
    assert_eq!(
        std::fs::read(rig.placement().join("logo.bin.topos-mine")).unwrap(),
        &[0xffu8, 9, 9]
    );
    // The on-disk tree scans back to the recorded conflict digest (sidecars survive the scanner).
    let cs: topos_types::persisted::ConflictState =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid(&id)).conflict)
            .unwrap()
            .unwrap();
    let scanned = crate::scan::scan(&rig.placement()).unwrap();
    assert_eq!(to_hex(&scanned.bundle_digest), cs.conflicted_digest);
}

/// The release-blocker crash gate: fault every fs op during an auto conflict resolve and assert (a) a
/// completed conflict tree is NEVER on disk without its guard record (a marker tree is never publishable),
/// and (b) a clean re-run always converges to the blocked conflict state — the placement holding a
/// complete tree throughout (never torn).
#[test]
fn resolve_crash_gate_converges_and_never_unguards_markers() {
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    // Capture the completed conflict tree + op count from a clean run.
    let (conflict_tree, n_ops) = {
        let rig = Rig::new("cg-count");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
        let foll = follow(&id, FollowMode::Auto);
        let fs = FaultFs::new(0);
        pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
        (snapshot(&rig.placement()), fs.ops_attempted())
    };
    assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

    for fail_at in 1..=n_ops {
        let rig = Rig::new(&format!("cg-{fail_at}"));
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
        let foll = follow(&id, FollowMode::Auto);

        // Fault the Nth op (may error mid-resolve).
        let fs = FaultFs::new(fail_at);
        let _ = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed);

        // SAFETY: if the completed conflict tree is on disk, its guard record MUST be present (a marker
        // tree is never publishable). It is written + fsynced before the swap, so this holds at every fault.
        if snapshot(&rig.placement()) == conflict_tree {
            assert!(
                rig.conflict_exists(&id),
                "fail_at={fail_at}: a conflict tree is on disk without its guard record"
            );
        }

        // A clean re-run converges: blocked conflict, complete conflict tree on disk, applied == observed.
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
            snapshot(&rig.placement()),
            conflict_tree,
            "fail_at={fail_at}: placement did not converge to the complete conflict tree"
        );
        assert_eq!(rig.read_sync(&id).applied, Generation { epoch: 1, seq: 1 });
    }
}

// --- review-driven regression tests ---

/// Escaping a recorded conflict WITHOUT editing must commit the author's ORIGINAL draft (drop the merge),
/// never the raw conflict-marker tree — otherwise the markers would become a publishable bundle.
#[test]
fn escape_of_unedited_conflict_commits_original_draft_not_markers() {
    let rig = Rig::new("escape-unedited");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    // Auto sweep → conflict (overlapping SKILL.md) → the placement holds markers.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(
        std::fs::read_to_string(rig.placement().join("SKILL.md"))
            .unwrap()
            .contains("<<<<<<<")
    );

    // Escape WITHOUT editing → commits MINE (the original draft), not the markers; clears the block.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            name: "pr-describe".into(),
            mode: ops::TargetMode::OntoCurrent,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id), "escape clears the block");
    // The placement is exactly MINE again — no markers anywhere.
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(
        !std::fs::read_to_string(rig.placement().join("SKILL.md"))
            .unwrap()
            .contains("<<<<<<<"),
        "the escape must not commit unresolved markers"
    );
}

/// Escaping a recorded conflict AFTER hand-editing it commits the author's resolution (the edited bytes).
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The author hand-resolves (removes markers) and then escapes.
    let resolved: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&rig.placement(), resolved);
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            name: "pr-describe".into(),
            mode: ops::TargetMode::OntoCurrent,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id));
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "the escape commits the author's hand resolution"
    );
}

/// A confirm-each accept must NOT merge a version whose digest was raised (newly discovered) in the same
/// pull — it re-offers instead, never applying undisclosed bytes.
#[test]
fn confirm_each_accept_reoffers_a_version_raised_in_the_same_pull() {
    let rig = Rig::new("ce-raised");
    let (id, _name, genesis) = rig.adopt(BASE);
    let edited: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let foll = follow(&id, FollowMode::ConfirmEach);

    // Step 1: a bare sweep discloses the divergence vs v1 (observed → (1,1)), surfaced not merged.
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut p1 = FixturePlane::default();
    p1.add_version(&id, &v1);
    p1.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    assert_eq!(
        only(&pull_data(&rig.ctx(&p1, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Diverged
    );
    assert_eq!(rig.read_sync(&id).observed, Generation { epoch: 1, seq: 1 });

    // Step 2: the plane has moved to v2 (1,2). An explicit accept would now merge an UNDISCLOSED version —
    // it must re-offer instead.
    let v2files: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# v2\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v2\n"),
    ];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut p2 = FixturePlane::default();
    p2.add_version(&id, &v1);
    p2.add_version(&id, &v2);
    p2.set_current(&id, signed(WS, &id, v2.id, 1, 2));
    let row = pull_data(
        &rig.ctx(&p2, &foll),
        ops::PullScope::One {
            name: "pr-describe".into(),
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(
        only(&row).action,
        PullAction::Diverged,
        "an accept must re-offer a version raised in the same call, not merge it"
    );
    assert!(only(&row).merge.is_none());
    assert!(!rig.conflict_exists(&id));
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "the binary conflict must resolve cleanly, not error on a digest collision"
    );
    // theirs at the path, theirs' real file kept, and mine's sidecar DISAMBIGUATED away from the collision.
    assert_eq!(
        std::fs::read(rig.placement().join("logo.bin")).unwrap(),
        &[0xffu8, 7, 7]
    );
    assert!(rig.placement().join("LOGO.BIN.TOPOS-MINE").exists());
    assert_eq!(
        std::fs::read(rig.placement().join("logo.bin.topos-mine-1")).unwrap(),
        &[0xffu8, 9, 9],
        "the sidecar was disambiguated to avoid the case-fold collision"
    );
    // The materialized tree scans back to the recorded conflict digest (no kernel rejection).
    let cs: topos_types::persisted::ConflictState =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid(&id)).conflict)
            .unwrap()
            .unwrap();
    let scanned = crate::scan::scan(&rig.placement()).unwrap();
    assert_eq!(to_hex(&scanned.bundle_digest), cs.conflicted_digest);
}

/// A recorded conflict must NOT hide the reused-tuple anti-rollback ALARM: even while blocked, the served
/// `current` is authenticated, so a plane reusing `(epoch,seq)` for a different commit still alarms.
#[test]
fn recorded_conflict_does_not_hide_the_reused_tuple_alarm() {
    let rig = Rig::new("conflict-alarm");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));

    // A compromised plane reuses (1,1) for a DIFFERENT commit. The bare sweep must ALARM despite the block.
    let forged: FileSet = &[("SKILL.md", FileMode::Regular, b"# forged\n")];
    let v2 = mk_version(&[genesis], forged, "d_pub", "v2");
    let mut compromised = FixturePlane::default();
    compromised.add_version(&id, &v2);
    compromised.set_current(&id, signed(WS, &id, v2.id, 1, 1)); // same generation, different commit
    let row = pull_data(&rig.ctx(&compromised, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        only(&row).action,
        PullAction::Alarm,
        "a reused tuple must alarm even while a conflict is pending"
    );
    assert!(
        rig.conflict_exists(&id),
        "the alarm does not clear the conflict record"
    );
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
                        mode: FollowMode::Auto,
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
    let foll = follow(&id, FollowMode::Auto);

    // A go-back to the adopted genesis (recorded locally) must complete with the plane fully down —
    // and make ZERO network calls (including the proposals count, which is documented plane-independent).
    let out = ops::pull(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            name,
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
                    mode: FollowMode::Auto,
                    review_required: false,
                    following: true,
                },
            ),
            (
                id.clone(),
                FollowContext {
                    workspace_id: WS.to_owned(),
                    mode: FollowMode::Auto,
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
        w.contains("topos_missing"),
        "the warning names the failed skill: {w}"
    );
    assert!(
        w.starts_with(char::is_uppercase) && w.contains(' '),
        "the warning leads with the stable error code: {w}"
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
    let foll = follow(&id, FollowMode::Auto);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert!(out.data.skills.is_empty(), "the wedged skill has no row");
    assert_eq!(out.warnings.len(), 1);

    // The REAL read path: `topos log <skill>` filters on the first-class skill_id field, so the wedged
    // skill's error event surfaces in its own log.
    let log = ops::log(&rig.ctx(&plane, &foll), &name).unwrap();
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
                mode: FollowMode::Auto,
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
        out.warnings[0].contains("CORRUPT_STATE"),
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
    fn exchange_dir(&self, a: &Path, b: &Path) -> std::io::Result<()> {
        self.record("exchange_dir", b);
        self.inner.exchange_dir(a, b)
    }
    fn read_opt(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read_opt(path)
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
    plane.set_current(&id, signed(WS, &id, v1.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

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
    plane.set_current(&id, signed(WS, &id, v2.id, 1, 2));
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
    plane.set_current(&id, signed(WS, &id, v2.id, 1, 1));
    let foll = follow(&id, FollowMode::Auto);

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
// validate_recorded_unique — identical semantics at O(n log n).
// ---------------------------------------------------------------------------------------------

#[test]
fn duplicate_generation_naming_two_commits_is_refused_as_corrupt() {
    let rig = Rig::new("dupgen");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = FixturePlane::default();
    let foll = follow(&id, FollowMode::Auto);

    // Forge local corruption: the SAME generation recorded under two DIFFERENT commits (non-adjacent in
    // list order — the sorted neighbour check must still pair them).
    rig.patch_sync(&id, |s| {
        let g = Generation { epoch: 5, seq: 5 };
        s.recorded.push(RecordedTuple {
            generation: g,
            commit_id: "11".repeat(32),
        });
        s.recorded.push(RecordedTuple {
            generation: Generation { epoch: 9, seq: 9 },
            commit_id: "33".repeat(32),
        });
        s.recorded.push(RecordedTuple {
            generation: g,
            commit_id: "22".repeat(32),
        });
    });

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert!(out.data.skills.is_empty(), "no row for the corrupt skill");
    assert_eq!(out.warnings.len(), 1);
    assert!(
        out.warnings[0].contains("CORRUPT_STATE"),
        "{:?}",
        out.warnings
    );
}

#[test]
fn exact_duplicate_recorded_tuples_stay_tolerated() {
    // The pre-existing semantics: a byte-identical duplicate tuple (same generation AND commit) is not
    // corruption — the neighbour scan must not turn it into a refusal.
    let rig = Rig::new("dupsame");
    let (id, _name, genesis) = rig.adopt(BASE);
    let plane = FixturePlane::default();
    let foll = follow(&id, FollowMode::Auto);
    rig.patch_sync(&id, |s| {
        let dup = s.recorded[0].clone();
        s.recorded.push(dup);
    });
    let _ = genesis;

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::UpToDate);
}
