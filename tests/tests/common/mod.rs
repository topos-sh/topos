//! Shared harness for the loopback e2e tests: a fresh, migrated per-test Postgres database
//! ([`provision_pg`]) plus the loopback-plane scaffold ([`Scratch`] / [`Plane`] / [`start_plane`]) every
//! suite stands its scenario on. Only the scenario-specific SEEDING stays per-file — each suite hands
//! [`start_plane`] a seed closure and gets a served plane back.
//!
//! Each e2e (HERO / follow / contribute) runs a **blocking `ureq` client** on the test thread alongside a
//! live `axum` server on a self-owned **multi-thread** runtime, so it cannot use `#[sqlx::test]` — that
//! macro drives the test on a **current-thread** runtime, where the blocking client would starve the
//! server and deadlock. Instead each test calls [`provision_pg`] inside its own runtime to get a `PgPool`
//! over a fresh database, then builds `Authority::from_pool(pool, git_root, large_root)`.
//!
//! The provisioned databases are left behind on the target Postgres — the CI / local build Postgres is
//! disposable (a container), and dropping a database while its pool still holds connections is racy.

// Each e2e binary compiles this module independently and drives a SUBSET of the harness — what one binary
// leaves unused is exercised by a sibling, so the module-level allow is deliberate, not a loophole.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ed25519_dalek::{Signer as _, SigningKey};
use plane_store::{
    Authority, CommitId, CreateInviteOutcome, DeploymentMode, EnrollmentConfig, FileMode,
    GovernanceOp, GovernanceSignedOp, OpId, Principal, Role, SkillId, UploadedFile, WorkspaceId,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};
use topos_core::sign::{GovernanceOpFields, GovernanceOpKind, governance_op_preimage};
use topos_plane::{PlaneState, router};
use topos_types::{Generation, TerminalOutcome};

// ── the shared scenario constants ───────────────────────────────────────────────────────────────────

/// The one workspace every e2e scenario plays in.
pub(crate) const WS: &str = "w_acme";
/// The one skill every e2e scenario distributes.
pub(crate) const SKILL: &str = "s_deploy";
/// The fixed wall-clock the seedings stamp.
pub(crate) const NOW: i64 = 1_000_000;

// ── per-test Postgres provisioning ──────────────────────────────────────────────────────────────────

/// Create a uniquely-named database on the `$DATABASE_URL` server, run the production migrations
/// ([`plane_store::MIGRATOR`]) on it, and return a pool over it. Panics with a clear message if
/// `DATABASE_URL` is unset or the server is unreachable (the e2e suite requires a Postgres, exactly like
/// the in-crate `#[sqlx::test]` suite).
pub(crate) async fn provision_pg() -> PgPool {
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::var("DATABASE_URL")
        .expect("the e2e suite requires DATABASE_URL to point at a Postgres");
    let opts: PgConnectOptions = base
        .parse()
        .expect("DATABASE_URL must be a valid Postgres connection string");
    let name = format!(
        "topos_e2e_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );

    // CREATE the fresh database on the base connection (identifier-quoted; the name is ASCII-safe anyway).
    let mut admin = PgConnection::connect_with(&opts)
        .await
        .expect("connect to the base Postgres database");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create the per-test database");
    admin.close().await.ok();

    // Connect to the fresh database and apply the SAME migrations production runs.
    let pool = PgPoolOptions::new()
        .connect_with(opts.database(&name))
        .await
        .expect("connect to the per-test database");
    plane_store::MIGRATOR
        .run(&pool)
        .await
        .expect("migrate the per-test database");
    pool
}

// ── the loopback plane scaffold ─────────────────────────────────────────────────────────────────────

/// A self-cleaning temp dir (RAII).
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(prefix: &str, tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{tag}-{}-{n}", std::process::id()));
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

/// What a scenario's seed closure stood up (beyond the authority's own state).
#[derive(Default)]
pub(crate) struct Seeded {
    /// The genesis version id, when the seeding published one.
    pub(crate) genesis: Option<CommitId>,
    /// The `/i/` invite links minted at standup, in mint order.
    pub(crate) invites: Vec<String>,
}

/// A running loopback plane. Holds the runtime + authority handle alive for the test's duration; `_dir`
/// drops LAST so the served store outlives the runtime/authority.
pub(crate) struct Plane {
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) authority: Arc<Authority>,
    pub(crate) base_url: String,
    pub(crate) plane_key: [u8; 32],
    seeded: Seeded,
    _dir: Scratch,
}

impl Plane {
    pub(crate) fn ws(&self) -> WorkspaceId {
        WorkspaceId::parse(WS).unwrap()
    }

    pub(crate) fn skill(&self) -> SkillId {
        SkillId::parse(SKILL).unwrap()
    }

    /// The genesis version id the seeding published (panics if the scenario stood none up).
    pub(crate) fn genesis(&self) -> CommitId {
        self.seeded
            .genesis
            .expect("the seeding published a genesis")
    }

    /// The `i`-th `/i/` invite link the seeding minted.
    pub(crate) fn invite(&self, i: usize) -> &str {
        &self.seeded.invites[i]
    }
}

/// Stand a loopback plane up: bind the socket FIRST (an enrollment-configured plane's bootstrap echoes
/// the real `base_url`, and an early client connect queues in the backlog with no race), open the
/// authority over a fresh migrated database (+ the plane key, + the device-code enrollment config when
/// `enrollment`), run the scenario's `seed`, then serve `router(state)` on a background multi-thread
/// runtime. Returns the live [`Plane`].
pub(crate) fn start_plane(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    let dir = Scratch::new(scratch_prefix, tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{addr}");

    let (authority, seeded, plane_key) = rt.block_on(async {
        let mut authority = Authority::from_pool(
            provision_pg().await,
            &dir.0.join("git"),
            &dir.0.join("large"),
        )
        .expect("open authority")
        .with_plane_key(&dir.0.join("plane.key"))
        .expect("load plane key");
        if enrollment {
            authority = authority
                .with_enrollment_config(EnrollmentConfig {
                    secret_path: dir.0.join("enroll.key"),
                    base_url: base_url.clone(),
                    verify_base_url: None,
                    deployment_mode: DeploymentMode::Cloud,
                    enrollment_method: "device_code".to_owned(),
                })
                .expect("load enrollment secret");
        }
        let seeded = seed(&authority).await;
        let plane_key = authority.plane_public_key().expect("plane public key");
        (authority, seeded, plane_key)
    });

    let authority = Arc::new(authority);
    let state = PlaneState::new(authority.clone());
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
        base_url,
        plane_key,
        seeded,
        _dir: dir,
    }
}

// ── shared seeding helpers ──────────────────────────────────────────────────────────────────────────

/// The distribute-plane standup (what the HERO + contribute scenarios share): register the publishing
/// device, roster its principal, publish a signed genesis at `(1,1)`, and mint the follower read token.
pub(crate) struct GenesisSpec<'a> {
    pub(crate) dkid: &'a str,
    pub(crate) device_seed: &'a [u8; 32],
    pub(crate) op_id: &'a str,
    pub(crate) files: Vec<UploadedFile>,
    pub(crate) principal: &'a str,
    pub(crate) author: &'a str,
    pub(crate) message: &'a str,
    pub(crate) created_at: &'a str,
    pub(crate) read_token: &'a str,
}

/// Run a [`GenesisSpec`] against a fresh authority; returns the genesis version id.
pub(crate) async fn seed_genesis_plane(authority: &Authority, spec: GenesisSpec<'_>) -> CommitId {
    let ws = WorkspaceId::parse(WS).unwrap();
    let skill = SkillId::parse(SKILL).unwrap();
    let principal = Principal::parse(spec.principal).unwrap();
    let device_pubkey = SigningKey::from_bytes(spec.device_seed)
        .verifying_key()
        .to_bytes();

    authority
        .seed_device(&ws, spec.dkid, &device_pubkey, &principal, false)
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
            spec.dkid,
            spec.device_seed,
            &OpId::parse(spec.op_id).unwrap(),
            spec.files,
            spec.author,
            spec.message,
            spec.created_at,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
    let genesis = receipt.version_id.expect("genesis version id");
    authority
        .mint_read_token(&ws, &skill, &principal, spec.read_token)
        .await
        .expect("mint read token");
    genesis
}

/// Mint a governance-signed `/i/` invite pre-offering `skill` to `email`. The `signer` (device key id +
/// signing seed) signs the SAME `topos-core` frame the plane re-derives + verifies (role byte 3 = Member,
/// `expires_at = 0`, the invited-email set, the offered-skill-id set), so this in-process mint cannot
/// drift from production.
pub(crate) async fn mint_invite(
    authority: &Authority,
    ws: &WorkspaceId,
    signer: (&str, &[u8; 32]),
    op_id: &str,
    email: &str,
    skill: &str,
    at: &str,
) -> String {
    let (signer_dkid, signer_seed) = signer;
    let hyphenless: String = op_id.chars().filter(|c| *c != '-').collect();
    let mut op_id_bytes = [0u8; 16];
    hex::decode_to_slice(&hyphenless, &mut op_id_bytes).expect("op_id is 16 hex bytes");

    let fields = GovernanceOpFields {
        workspace_id: WS,
        op_id: op_id_bytes,
        device_key_id: signer_dkid,
        op: GovernanceOpKind::Invite {
            role: 3, // Member
            expires_at: 0,
            emails: &[email],
            skills: &[skill],
        },
    };
    let preimage = governance_op_preimage(&fields).expect("governance preimage");
    let signature = SigningKey::from_bytes(signer_seed)
        .sign(&preimage)
        .to_bytes();
    let signed = GovernanceSignedOp {
        device_key_id: signer_dkid.to_owned(),
        op: GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![Principal::parse(email).unwrap()],
            skills: vec![(SkillId::parse(skill).unwrap(), None)],
        },
        signature,
    };
    match authority
        .create_invite(ws, op_id, signed, at)
        .await
        .expect("create_invite")
    {
        CreateInviteOutcome::Created(invite) => invite.link,
        CreateInviteOutcome::Denied(reason) => panic!("invite denied: {reason}"),
    }
}

// ── shared bundle expectations ──────────────────────────────────────────────────────────────────────

/// The standard genesis bundle the HERO + follow scenarios publish: a regular doc + an EXECUTABLE script
/// (the exec bit must survive end to end).
pub(crate) fn genesis_files() -> Vec<UploadedFile> {
    vec![
        UploadedFile {
            path: "SKILL.md".to_owned(),
            mode: FileMode::Regular,
            bytes: b"# deploy\nDeploy the service.\n".to_vec(),
        },
        UploadedFile {
            path: "run.sh".to_owned(),
            mode: FileMode::Executable,
            bytes: b"#!/bin/sh\necho deploying\n".to_vec(),
        },
    ]
}

/// The placement-snapshot shape (`(path, mode & 0o777, bytes)`, sorted) a plane bundle must materialize
/// to: regular files at 0o644, executable files at 0o755.
pub(crate) fn expected_placement(files: &[UploadedFile]) -> Vec<(String, u32, Vec<u8>)> {
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

/// The same expectation for a `(path, is_executable, bytes)` literal bundle.
pub(crate) fn expected(files: &[(&str, bool, &[u8])]) -> Vec<(String, u32, Vec<u8>)> {
    let mut out: Vec<(String, u32, Vec<u8>)> = files
        .iter()
        .map(|(p, exec, b)| {
            (
                (*p).to_owned(),
                if *exec { 0o755 } else { 0o644 },
                b.to_vec(),
            )
        })
        .collect();
    out.sort();
    out
}
