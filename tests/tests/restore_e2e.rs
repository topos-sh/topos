//! RESTORE — the backup/restore rehearsal: the operator epoch-bump helper (`restore-bump-epoch`) proven
//! over the REAL loopback HTTP plane + the REAL client pull engine.
//!
//! One real `plane-store` [`Authority`] (seeded through the feature-gated test-fixtures shims) is served by
//! the composed [`topos_plane::router`] on a real `127.0.0.1:0` socket; the client side is the GENUINE pull
//! engine over the GENUINE `ureq` transport ([`topos::test_support::PullHarness`]), exactly as in `hero.rs`.
//!
//! **The restore simulation is a SQL rewind**: right after v1 the test captures the full `current` row
//! (incl. the unsigned `record` bytes), lets the plane advance to v2 and the follower pull it, then writes
//! the captured v1 row back over `current` and DELETEs v2's commit-side rows (`commit_object` →
//! `skill_commit` → its `op_receipts` row) — mirroring exactly what an older database dump would and would
//! not contain. That is honest for these per-test fresh databases: a `pg_dump`/restore of the same
//! container would land the identical row set and prove nothing more.
//!
//! The trust model has NO anti-rollback floor and NO signed pointers, so a server restore is simply a team
//! rollback the client applies toward whatever is served — backward included, silently, drafts still
//! preserved. Two rehearsals:
//! 1. **with the epoch-bump helper**: the bump REWRITES the restored v1 pointer at `(2,1)` (same commit,
//!    one epoch forward — nothing re-signed; `EpochBumpReport` carries no key id); the follower rolls
//!    forward onto v1's OLDER bytes as an ordinary forward move, with NO error, and the author's next
//!    publish resumes normal life at `(2,2)` — a subsequent publish proceeds at the bumped epoch.
//! 2. **without the helper**: the plane simply serves the restored v1 pointer at its ORIGINAL `(1,1)` — a
//!    LOWER generation than the follower's `(1,2)`. The follower SILENTLY rolls BACKWARD onto v1's bytes;
//!    there is no anti-rollback ALARM — a server restore is a team rollback the client applies toward
//!    whatever is served, backward included. (The bump in rehearsal 1 is what restores strict monotonicity
//!    so a subsequent same-tuple re-publish is not lost to the generation-keyed currency.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod common;
use std::sync::atomic::{AtomicU32, Ordering};

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
/// The publisher device's workspace Bearer credential — and the one the follower presents to read (a
/// confirmed member reads every skill; per-skill read tokens are gone).
const CRED: &str = "wc_restore_secret_value";
const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos publish";
const CREATED_AT: &str = "2026-07-01T00:00:00Z";
const NOW: i64 = 1_000_000;
/// The device's registered 32-byte public key (a fixed test value; nothing verifies against it).
const DEVICE_PUBKEY: [u8; 32] = [7u8; 32];
const GENESIS_OP: &str = "b0000000-0000-4000-8000-000000000001";
const V2_OP: &str = "b0000000-0000-4000-8000-000000000002";
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
                    CRED,
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

/// Seed a real authority (device+credential+confirmed-member → v1 genesis), then serve `router(state)` on a
/// real loopback socket on a background runtime.
fn start_plane(tag: &str) -> Plane {
    let dir = Scratch::new(tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let (authority, pool, genesis) = rt.block_on(async {
        let pool = common::provision_pg().await;
        let authority =
            Authority::from_pool(pool.clone(), &dir.0.join("git"), &dir.0.join("large"))
                .expect("open authority");

        let ws = WorkspaceId::parse(WS).unwrap();
        let skill = SkillId::parse(SKILL).unwrap();
        let principal = Principal::parse(PRINCIPAL).unwrap();

        // Register the publisher device WITH its workspace credential + seat its principal as a confirmed
        // member — the whole authorization for the genesis/child WRITES and the follower's READS (per-skill
        // roster grants nothing; the follower presents this same credential).
        authority
            .seed_device(&ws, DKID, &DEVICE_PUBKEY, &principal, false, CRED)
            .await
            .expect("seed device");
        authority
            .seed_workspace_member(&ws, &principal, "member", "confirmed")
            .await
            .expect("seat confirmed member");
        let receipt = authority
            .seed_published_genesis(
                &ws,
                &skill,
                CRED,
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
        (authority, pool, genesis)
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
        genesis,
        _dir: dir,
    }
}

// ── the restore simulation (the SQL rewind) ─────────────────────────────────────────────────────────

/// The full `current` row as the backup held it (incl. the unsigned `record` bytes — restored verbatim,
/// exactly as a dump would restore it).
struct CurrentRowSnapshot {
    commit_id: Vec<u8>,
    epoch: i64,
    seq: i64,
    record: Option<Vec<u8>>,
    updated_at: i64,
}

/// Capture the skill's live `current` row — "take the backup".
fn capture_current(plane: &Plane) -> CurrentRowSnapshot {
    use sqlx::Row as _;
    plane.rt.block_on(async {
        let row = sqlx::query(
            "SELECT commit_id, epoch, seq, record, updated_at FROM current \
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
            record: row.get("record"),
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
            "UPDATE current SET commit_id = $1, epoch = $2, seq = $3, record = $4, \
             updated_at = $5 WHERE workspace_id = $6 AND skill_id = $7",
        )
        .bind(&snapshot.commit_id)
        .bind(snapshot.epoch)
        .bind(snapshot.seq)
        .bind(&snapshot.record)
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
    client.adopt_followed(SKILL, WS, CRED, Follow::Auto, LOCAL_PLACEHOLDER);

    // The follower lands v1 at (1,1)...
    let ff = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(ff.skills[0].action, PullAction::FastForwarded);

    // ...the backup is taken right after v1...
    let backup = capture_current(plane);

    // ...the team keeps working: v2 lands at (1,2) and the follower pulls it.
    let v2 = plane.publish_child(plane.genesis, bundle("v2"), V2_OP);
    let ff2 = client.run_pull(&plane.base_url, Scope::AllFollowed);
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

// ── rehearsal 1: the epoch-bump helper — the fleet rolls forward onto the restored bytes, no error ────

#[test]
fn restore_bump_rolls_the_fleet_forward() {
    let plane = start_plane("rollfwd");
    let (client, _v2) = rehearse_to_restored(&plane);

    // The operator runs the epoch-bump helper: the restored (1,1) v1 pointer is REWRITTEN at (2,1) — same
    // commit, one epoch forward (nothing is re-signed; the stored record is unsigned).
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

    // The follower (last observed (1,2)/v2) sees (2,1): epoch-dominant ⇒ strictly higher ⇒ an ORDINARY
    // forward move onto v1's OLDER bytes. It applies SILENTLY, with NO error — the served record IS the
    // sync target; there is no anti-rollback floor and no signature to verify.
    let rolled = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(
        rolled.skills[0].action,
        PullAction::FastForwarded,
        "the restored pointer is an ordinary forward move: {:?}",
        rolled.skills[0]
    );
    assert_eq!(rolled.skills[0].applied, Generation { epoch: 2, seq: 1 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v1")),
        "v1's older bytes materialize byte-exact, no error"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.base_commit, hex::encode(plane.genesis.as_bytes()));

    // Normal life resumes: the author publishes v4 on the restored current ⇒ (2,2) ⇒ the follower
    // fast-forwards, proving a subsequent publish proceeds at the bumped epoch.
    let v4 = plane.publish_child(plane.genesis, bundle("v4"), V4_OP);
    let ff = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(ff.skills[0].action, PullAction::FastForwarded);
    assert_eq!(ff.skills[0].applied, Generation { epoch: 2, seq: 2 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v4"))
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.base_commit, hex::encode(v4.as_bytes()));
}

// ── rehearsal 2: WITHOUT the helper — the fleet silently rolls BACK onto the restored bytes, no alarm ──

#[test]
fn restore_without_bump_silently_rolls_back() {
    let plane = start_plane("silent");
    let (client, _v2) = rehearse_to_restored(&plane);

    // No epoch bump, no re-publish: the plane simply serves the restored v1 pointer at its ORIGINAL (1,1).
    // The follower (last observed (1,2)/v2) sees a LOWER generation naming a different commit and SILENTLY
    // rolls BACKWARD onto v1 — a server restore is a team rollback the client applies toward whatever is
    // served, backward included; there is no anti-rollback ALARM anymore, and nothing is refused.
    let rolled = client.run_pull(&plane.base_url, Scope::AllFollowed);
    assert_eq!(
        rolled.skills[0].action,
        PullAction::FastForwarded,
        "a backward-moving restored pointer is silently applied, never alarmed: {:?}",
        rolled.skills[0]
    );
    assert_eq!(rolled.skills[0].applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(
        client.placement_files(SKILL),
        expected_placement(&bundle("v1")),
        "v1's restored bytes materialize byte-exact, no error"
    );
    let sync = client.sync_state(SKILL);
    assert_eq!(sync.applied, Generation { epoch: 1, seq: 1 });
    assert_eq!(sync.base_commit, hex::encode(plane.genesis.as_bytes()));
}
