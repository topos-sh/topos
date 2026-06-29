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
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    FollowContext, FollowMode, FollowSource, InertFollow, InertPlane, PlaneError, PlaneSource,
    PointerFetch,
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
        known: Option<Generation>,
    ) -> Result<PointerFetch, PlaneError> {
        let Some(rec) = self.records.get(skill_id) else {
            return Err(PlaneError::NotFound);
        };
        // The conditional GET (ETag "<epoch>.<seq>"): a client already at the served generation gets 304.
        if let Some(k) = known
            && k.epoch == rec.record.generation.epoch
            && k.seq == rec.record.generation.seq
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
    fn proposals_awaiting(&self) -> u32 {
        0
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
        Ctx {
            fs: &self.fs,
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
        doc::read_doc(&self.fs, &self.layout().published(id).sync)
            .unwrap()
            .unwrap()
    }
    fn patch_sync(&self, id: &str, f: impl FnOnce(&mut SyncState)) {
        let mut s = self.read_sync(id);
        f(&mut s);
        doc::write_doc(&self.fs, &self.layout().published(id).sync, &s).unwrap();
    }
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();

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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let data = ops::pull(
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

#[test]
fn draft_diverges_and_is_never_clobbered() {
    let rig = Rig::new("diverge");
    let (id, _name, genesis) = rig.adopt(BASE);
    // Edit the placement → a local draft.
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Diverged);
    let conflict = row.conflict.as_ref().expect("a conflict panel");
    assert_eq!(conflict.remote_version_id, to_hex(&v1.id));
    assert!(
        conflict.local_version_id.is_some(),
        "the draft was snapshotted"
    );
    // NEVER clobbered: the placement still holds the local edit, not the remote bytes.
    assert_eq!(snapshot(&rig.placement()), Some(expect(edited)));
    // `applied` did not advance (nothing was auto-applied).
    assert_eq!(rig.read_sync(&id).applied, Generation { epoch: 0, seq: 0 });
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
    ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    // Go back to genesis: old bytes installed, `held` set, the floor (`observed`) untouched.
    let ctx = rig.ctx(&plane, &foll);
    let data = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: name.clone(),
            mode: ops::TargetMode::GoBack(genesis),
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
    ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "hold suppresses auto-FF"
    );

    // A bare explicit `pull <skill>` resumes (clears the hold) and fast-forwards back to v1.
    let ctx = rig.ctx(&plane, &foll);
    let data = ops::pull(
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
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

/// A plane that returns a structurally-malformed response (a corrupt/forged record or bytes).
struct MalformedPlane;
impl PlaneSource for MalformedPlane {
    fn get_current(&self, _: &str, _: Option<Generation>) -> Result<PointerFetch, PlaneError> {
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
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
    let data = ops::pull(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::Alarm);
    assert_eq!(snapshot(&rig.placement()), Some(expect(BASE)));
    assert_eq!(rig.read_sync(&id).observed, Generation { epoch: 0, seq: 0 });
}
