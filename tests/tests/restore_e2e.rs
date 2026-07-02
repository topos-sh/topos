//! RESTORE — the backup/restore rehearsal: the operator epoch-bump helper (`restore-bump-epoch`) proven
//! over the REAL loopback HTTP plane + the REAL client pull engine.
//!
//! One real `plane-store` [`Authority`] (seeded through the feature-gated test-fixtures shims) is served by
//! the composed [`topos_plane::router`] on a real `127.0.0.1:0` socket; the client side is the GENUINE pull
//! engine over the GENUINE `ureq` transport ([`topos::test_support::PullHarness`]), exactly as in `hero.rs`.
//!
//! **The restore simulation is a SQL rewind**: right after v1 the test captures the full `current` row
//! (incl. the signed-record bytes), lets the plane advance to v2 and the follower pull it, then writes the
//! captured v1 row back over `current` and DELETEs v2's commit-side rows (`commit_object` → `skill_commit`
//! → its `op_receipts` row) — mirroring exactly what an older database dump would and would not contain.
//! That is honest for these per-test fresh databases: a `pg_dump`/restore of the same container would land
//! the identical row set and prove nothing more.
//!
//! Two rehearsals:
//! 1. **without the helper**: a post-restore re-publish re-issues generation `(1,2)` under DIFFERENT bytes
//!    — the follower's anti-rollback floor raises the reused-tuple ALARM and never clobbers the placed v2;
//!    THEN the helper bumps to `(2,2)` and the next pull is an ordinary forward move (it heals an
//!    already-alarmed fleet).
//! 2. **with the helper first**: the bump re-signs the restored v1 pointer at `(2,1)`; the follower rolls
//!    forward onto v1's older bytes with NO alarm — the real client verifying the re-signed record over
//!    HTTP is the envelope-parity oracle — and the author's next publish resumes normal life at `(2,2)`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod common;
use std::sync::atomic::{AtomicU32, Ordering};

use ed25519_dalek::SigningKey;
use plane_store::{
    Authority, CommitId, FileMode, OpId, Principal, SkillId, UploadedFile, WorkspaceId,
};
use sqlx::PgPool;
use topos::test_support::{Follow, PullHarness, Scope};
use topos_plane::{PlaneState, router};
use topos_types::results::PullAction;
use topos_types::{Generation, TerminalOutcome};

// ── shared constants (the hero.rs vocabulary) ─────────────────────────────────────────────────────
const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
const DKID: &str = "dk_a";
const PRINCIPAL: &str = "p_dev";
const READ_TOKEN: &str = "rt_restore_secret_value";
const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos publish";
const CREATED_AT: &str = "2026-07-01T00:00:00Z";
const NOW: i64 = 1_000_000;
/// The deterministic device signing seed; its public key is registered via `seed_device`.
const DEVICE_SEED: [u8; 32] = [7u8; 32];
const GENESIS_OP: &str = "b0000000-0000-4000-8000-000000000001";
const V2_OP: &str = "b0000000-0000-4000-8000-000000000002";
const V3_OP: &str = "b0000000-0000-4000-8000-000000000003";
const V4_OP: &str = "b0000000-0000-4000-8000-000000000004";

/// A one-file bundle whose content is distinct per tag (v1/v2/v3/v4 are all different bytes).
fn bundle(tag: &str) -> Vec<UploadedFile> {
    vec![UploadedFile {
        path: "SKILL.md".to_owned(),
        mode: FileMode::Regular,
        bytes: format!("# deploy {tag}\nDeploy the service ({tag}).\n").into_bytes(),
    }]
}

/// The LOCAL placeholder a client adopts before any pull (NOT the plane's genesis, so the first pull
/// genuinely fast-forwards onto the plane's bytes).
const LOCAL_PLACEHOLDER: &[(&str, bool, &[u8])] = &[("SKILL.md", false, b"# local placeholder\n")];

/// The placement-snapshot shape a plane bundle should materialize to (regular files at 0o644).
fn expected_placement(files: &[UploadedFile]) -> Vec<(String, u32, Vec<u8>)> {
    let mut out: Vec<(String, u32, Vec<u8>)> = files
        .iter()
        .map(|f| {
            let mode = match f.mode {
                FileMode::Executable => 0o755,
                FileMode::Regular => 0o644,
            };
            (f.path.clone(), mode, f.bytes.clone())
        })
        .collect();
    out.sort();
    out
}

// ── the loopback plane (hero.rs's rig + a retained pool for the SQL rewind) ─────────────────────────

/// A self-cleaning temp dir (RAII).
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "topos-restore-plane-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create plane scratch dir");
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running loopback plane. `pool` is a second handle onto the SAME per-test database the authority uses —
/// the rewind's raw-SQL seam. `_dir` drops LAST so the served store outlives the runtime/authority.
struct Plane {
    rt: tokio::runtime::Runtime,
    authority: Arc<Authority>,
    pool: PgPool,
    base_url: String,
    plane_key: [u8; 32],
    /// The genesis (v1) version id the plane published at `(1,1)`.
    genesis: CommitId,
    _dir: Scratch,
}

impl Plane {
    fn ws(&self) -> WorkspaceId {
        WorkspaceId::parse(WS).unwrap()
    }
    fn skill(&self) -> SkillId {
        SkillId::parse(SKILL).unwrap()
    }

    /// Publish a 1-parent child of `parent` through the real pointer-move (the author's next version).
    fn publish_child(&self, parent: CommitId, files: Vec<UploadedFile>, op: &str) -> CommitId {
        let (ws, skill) = (self.ws(), self.skill());
        self.rt.block_on(async {
            let receipt = self
                .authority
                .seed_published_child(
                    &ws,
                    &skill,
                    DKID,
                    &DEVICE_SEED,
                    &OpId::parse(op).unwrap(),
                    parent,
                    files,
                    AUTHOR,
                    MESSAGE,
                    CREATED_AT,
                    NOW,
                )
                .await
                .expect("publish child");
            assert_eq!(receipt.outcome, TerminalOutcome::Ok);
            receipt.version_id.expect("child version id")
        })
    }
}

/// Seed a real authority (device → roster → signed v1 genesis → read token), then serve `router(state)` on
/// a real loopback socket on a background runtime.
fn start_plane(tag: &str) -> Plane {
    let dir = Scratch::new(tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let (authority, pool, genesis, plane_key) = rt.block_on(async {
        let pool = common::provision_pg().await;
        let authority =
            Authority::from_pool(pool.clone(), &dir.0.join("git"), &dir.0.join("large"))
                .expect("open authority")
                .with_plane_key(&dir.0.join("plane.key"))
                .expect("load plane key");

        let ws = WorkspaceId::parse(WS).unwrap();
        let skill = SkillId::parse(SKILL).unwrap();
        let principal = Principal::parse(PRINCIPAL).unwrap();
        let device_pubkey = SigningKey::from_bytes(&DEVICE_SEED)
            .verifying_key()
            .to_bytes();

        authority
            .seed_device(&ws, DKID, &device_pubkey, &principal, false)
            .await
            .expect("seed device");
        authority
            .seed_roster(&ws, &skill, &principal)
            .await
            .expect("seed roster");
        let receipt = authority
            .seed_published_genesis(
                &ws,
                &skill,
                DKID,
                &DEVICE_SEED,
                &OpId::parse(GENESIS_OP).unwrap(),
                bundle("v1"),
                AUTHOR,
                MESSAGE,
                CREATED_AT,
                NOW,
            )
            .await
            .expect("seed genesis");
        assert_eq!(receipt.outcome, TerminalOutcome::Ok);
        assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
        let genesis = receipt.version_id.expect("genesis version id");
        authority
            .mint_read_token(&ws, &skill, &principal, READ_TOKEN)
            .await
            .expect("mint read token");
        let plane_key = authority.plane_public_key().expect("plane public key");
        (authority, pool, genesis, plane_key)
    });

    let authority = Arc::new(authority);
    let state = PlaneState::new(authority.clone());

    // Bind (and listen) BEFORE spawning serve, so a client connect queues in the backlog with no race.
    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    rt.spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    Plane {
        rt,
        authority,
        pool,
        base_url: format!("http://{addr}"),
        plane_key,
        genesis,
        _dir: dir,
    }
}

// ── the restore simulation (the SQL rewind) ─────────────────────────────────────────────────────────

/// The full `current` row as the backup held it (incl. the signed-record bytes — the pre-incident
/// signature is restored verbatim, exactly as a dump would restore it).
struct CurrentRowSnapshot {
    commit_id: Vec<u8>,
    epoch: i64,
    seq: i64,
    signed_record: Option<Vec<u8>>,
    updated_at: i64,
}

/// Capture the skill's live `current` row — "take the backup".
fn capture_current(plane: &Plane) -> CurrentRowSnapshot {
    use sqlx::Row as _;
    plane.rt.block_on(async {
        let row = sqlx::query(
            "SELECT commit_id, epoch, seq, signed_record, updated_at FROM current \
             WHERE workspace_id = $1 AND skill_id = $2",
        )
        .bind(WS)
        .bind(SKILL)
        .fetch_one(&plane.pool)
        .await
        .expect("capture the current row");
        CurrentRowSnapshot {
            commit_id: row.get("commit_id"),
            epoch: row.get("epoch"),
            seq: row.get("seq"),
            signed_record: row.get("signed_record"),
            updated_at: row.get("updated_at"),
        }
    })
}

/// "Restore the backup": write the captured row back over `current` and DELETE the newer version's
/// commit-side rows — `commit_object` first (its FK targets `skill_commit`), then the provenance row, then
/// its receipt — mirroring what an older dump would and would not contain. (`current`'s own FK is satisfied
/// because the rewind re-points it at the still-present v1 provenance row first.)
fn rewind_to(plane: &Plane, snapshot: &CurrentRowSnapshot, newer: CommitId, newer_op: &str) {
    plane.rt.block_on(async {
        sqlx::query(
            "UPDATE current SET commit_id = $1, epoch = $2, seq = $3, signed_record = $4, \
             updated_at = $5 WHERE workspace_id = $6 AND skill_id = $7",
        )
        .bind(&snapshot.commit_id)
        .bind(snapshot.epoch)
        .bind(snapshot.seq)
        .bind(&snapshot.signed_record)
        .bind(snapshot.updated_at)
        .bind(WS)
        .bind(SKILL)
        .execute(&plane.pool)
        .await
        .expect("rewind the current row");
        sqlx::query("DELETE FROM commit_object WHERE workspace_id = $1 AND commit_id = $2")
            .bind(WS)
            .bind(newer.as_bytes().as_slice())
            .execute(&plane.pool)
            .await
            .expect("delete the newer version's reachability edges");
        sqlx::query("DELETE FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2")
            .bind(WS)
            .bind(newer.as_bytes().as_slice())
            .execute(&plane.pool)
            .await
            .expect("delete the newer version's provenance row");
        sqlx::query("DELETE FROM op_receipts WHERE workspace_id = $1 AND op_id = $2")
            .bind(WS)
            .bind(newer_op)
            .execute(&plane.pool)
            .await
            .expect("delete the newer version's receipt");
    });
}

/// The shared prologue: plane at v1, follower pulled v1, plane advances to v2, follower pulls v2, then the
/// database is rewound to the captured v1 row. Returns the client + the v2 commit.
fn rehearse_to_restored(plane: &Plane) -> (PullHarness, CommitId) {
    let mut client = PullHarness::new("restore");
    client.adopt_followed(SKILL, WS, READ_TOKEN, Follow::Auto, LOCAL_PLACEHOLDER);

    // The follower lands v1 at (1,1)...
    let ff = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(ff.skills[0].action, PullAction::FastForwarded);

    // ...the backup is taken right after v1...
    let backup = capture_current(plane);

    // ...the team keeps working: v2 lands at (1,2) and the follower pulls it.
    let v2 = plane.publish_child(plane.genesis, bundle("v2"), V2_OP);
    let ff2 = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(ff2.skills[0].action, PullAction::FastForwarded);
    assert_eq!(ff2.skills[0].applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v2"))
    );

    // The incident: the database is restored from the v1 backup (v2 exists only on followers now).
    rewind_to(plane, &backup, v2, V2_OP);
    (client, v2)
}

// ── rehearsal 1: WITHOUT the helper, a re-publish alarms; the helper then heals the alarmed fleet ─────

#[test]
fn restore_without_helper_alarms() {
    let plane = start_plane("alarm");
    let (client, _v2) = rehearse_to_restored(&plane);

    // The author, unaware, publishes DIFFERENT bytes on the restored (1,1): the plane re-issues (1,2)
    // naming v3 — a generation tuple the follower already recorded under v2.
    let v3 = plane.publish_child(plane.genesis, bundle("v3"), V3_OP);

    // The follower's floor (observed (1,2), recorded v2) sees (1,2) name a DIFFERENT commit: a loud
    // reused-tuple ALARM — and the placed v2 bytes are never clobbered.
    let alarmed = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(
        alarmed.skills[0].action,
        PullAction::Alarm,
        "a reused generation tuple must alarm, not apply: {:?}",
        alarmed.skills[0]
    );
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v2")),
        "the alarmed pull must never clobber the placed v2 bytes"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.applied, Generation { epoch: 1, seq: 2 });
    assert_eq!(sync.observed, Generation { epoch: 1, seq: 2 });

    // NOW the operator runs the helper: (1,2) → (2,2), still naming v3, freshly signed.
    let selection = [plane.ws()];
    let reports = plane
        .rt
        .block_on(
            plane
                .authority
                .restore_bump_epochs(Some(&selection), None, NOW + 1),
        )
        .expect("restore_bump_epochs");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].old, Generation { epoch: 1, seq: 2 });
    assert_eq!(reports[0].new, Generation { epoch: 2, seq: 2 });
    assert_eq!(reports[0].commit, v3, "the bump keeps the commit (v3)");

    // The next pull is an ORDINARY forward move — the helper heals an already-alarmed fleet.
    let healed = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(
        healed.skills[0].action,
        PullAction::FastForwarded,
        "after the bump the fleet rolls forward: {:?}",
        healed.skills[0]
    );
    assert_eq!(healed.skills[0].applied, Generation { epoch: 2, seq: 2 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v3")),
        "v3 materializes byte-exact"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.base_commit, hex::encode(v3.as_bytes()));
}

// ── rehearsal 2: helper FIRST — the fleet rolls forward onto the restored bytes, no alarm ever ───────

#[test]
fn restore_with_helper_rolls_forward() {
    let plane = start_plane("heal");
    let (client, _v2) = rehearse_to_restored(&plane);

    // The operator runs the helper BEFORE anyone publishes: the restored (1,1) v1 pointer is re-signed at
    // (2,1) — same commit, fresh signature under the same pre-incident key.
    let selection = [plane.ws()];
    let reports = plane
        .rt
        .block_on(
            plane
                .authority
                .restore_bump_epochs(Some(&selection), None, NOW + 1),
        )
        .expect("restore_bump_epochs");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].old, Generation { epoch: 1, seq: 1 });
    assert_eq!(reports[0].new, Generation { epoch: 2, seq: 1 });
    assert_eq!(
        reports[0].commit, plane.genesis,
        "the bump keeps the v1 commit"
    );
    assert_eq!(
        reports[0].key_id,
        plane.authority.plane_key_id().expect("plane key id"),
        "the report's key id is the operator's pre-incident-key tripwire"
    );

    // The follower (floor (1,2), recorded v2) sees (2,1): epoch-dominant ⇒ strictly higher ⇒ an ORDINARY
    // forward move onto v1's OLDER bytes — the REAL client verifying the re-signed record over HTTP is the
    // envelope-parity oracle. No alarm, nothing refused.
    let rolled = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(
        rolled.skills[0].action,
        PullAction::FastForwarded,
        "the re-signed restored pointer is an ordinary forward move: {:?}",
        rolled.skills[0]
    );
    assert_eq!(rolled.skills[0].applied, Generation { epoch: 2, seq: 1 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v1")),
        "v1's older bytes materialize byte-exact"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.base_commit, hex::encode(plane.genesis.as_bytes()));

    // Normal life resumes: the author publishes v4 on the restored current ⇒ (2,2) ⇒ the follower
    // fast-forwards.
    let v4 = plane.publish_child(plane.genesis, bundle("v4"), V4_OP);
    let ff = client.run_pull(&plane.base_url, plane.plane_key, Scope::AllFollowed);
    assert_eq!(ff.skills[0].action, PullAction::FastForwarded);
    assert_eq!(ff.skills[0].applied, Generation { epoch: 2, seq: 2 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v4"))
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.base_commit, hex::encode(v4.as_bytes()));
}
