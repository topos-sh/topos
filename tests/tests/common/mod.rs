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

use plane_store::{
    Authority, CommitId, CreateInviteOutcome, DeploymentMode, EnrollmentConfig, FileMode,
    GovernanceOp, GovernanceRequest, OpId, Principal, Role, SkillId, UploadedFile, WorkspaceId,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};
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
    /// The provisioned per-test database — for direct row-level witnesses only (e.g. the standup chain's
    /// "the admin_claim table stayed empty"), never a second write path.
    pub(crate) pool: PgPool,
    pub(crate) base_url: String,
    /// The base the minted `/i/` links ride — `base_url` unless the plane was started split
    /// ([`start_plane_split`]), the hosted links-on-the-web-origin shape.
    pub(crate) link_base_url: String,
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
/// runtime. Returns the live [`Plane`]. The plane runs at `Cloud` mode; the standup e2e's self-host
/// chain uses [`start_plane_mode`].
pub(crate) fn start_plane(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    start_plane_mode(scratch_prefix, tag, enrollment, DeploymentMode::Cloud, seed)
}

/// [`start_plane`] with an explicit deployment posture — a self-host plane's standup door is the uniform
/// miss and its redeem gate admits a bearer, so the standup e2e needs both modes.
pub(crate) fn start_plane_mode(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    mode: DeploymentMode,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    start_plane_impl(scratch_prefix, tag, enrollment, mode, false, seed)
}

/// [`start_plane`] with the SPLIT link base: the same listener answers two host strings — the API
/// `base_url` is `http://127.0.0.1:<port>` and the minted `/i/` links ride `http://localhost:<port>`
/// (the hosted user-visible-links-on-the-web-origin shape, without a second server). What only this
/// split can prove: the client re-roots off the link host onto the bootstrap-declared API base.
#[allow(dead_code)] // each e2e binary compiles the shared harness; only follow_e2e drives the split.
pub(crate) fn start_plane_split(
    scratch_prefix: &str,
    tag: &str,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    start_plane_impl(scratch_prefix, tag, true, DeploymentMode::Cloud, true, seed)
}

fn start_plane_impl(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    mode: DeploymentMode,
    split_link_base: bool,
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
    let link_base_url = if split_link_base {
        format!("http://localhost:{}", addr.port())
    } else {
        base_url.clone()
    };

    let (authority, seeded, pool) = rt.block_on(async {
        let pool = provision_pg().await;
        let mut authority =
            Authority::from_pool(pool.clone(), &dir.0.join("git"), &dir.0.join("large"))
                .expect("open authority");
        if enrollment {
            authority = authority
                .with_enrollment_config(EnrollmentConfig {
                    secret_path: dir.0.join("enroll.key"),
                    base_url: base_url.clone(),
                    verify_base_url: None,
                    link_base_url: split_link_base.then(|| link_base_url.clone()),
                    deployment_mode: mode,
                    enrollment_method: "device_code".to_owned(),
                })
                .expect("load enrollment secret");
        }
        let seeded = seed(&authority).await;
        (authority, seeded, pool)
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
        pool,
        base_url,
        link_base_url,
        seeded,
        _dir: dir,
    }
}

// ── shared seeding helpers ──────────────────────────────────────────────────────────────────────────

/// Register a device holding `credential` (bound to `principal`, non-revoked) AND seat that principal as a
/// CONFIRMED `workspace_member` at `role` — the ONE call that authorizes a device to read AND write in `ws`
/// under the workspace-credential model. Per-skill `roster` grants nothing now; the presented credential
/// (resolved by its sha256 to this registry row) plus the confirmed membership seat are the whole
/// authorization, on every lane. `role` ∈ {`owner`,`reviewer`,`member`}. Both seed shims UPSERT, so a
/// principal already seated (e.g. by a genesis seed) is simply refreshed.
pub(crate) async fn seed_member(
    authority: &Authority,
    ws: &WorkspaceId,
    dkid: &str,
    pubkey: &[u8; 32],
    principal: &str,
    role: &str,
    credential: &str,
) {
    let p = Principal::parse(principal).unwrap();
    authority
        .seed_device(ws, dkid, pubkey, &p, false, credential)
        .await
        .expect("seed member device");
    authority
        .seed_workspace_member(ws, &p, role, "confirmed")
        .await
        .expect("seat confirmed member");
}

/// The distribute-plane standup (what the HERO + contribute scenarios share): register the publishing
/// device WITH its workspace credential, seat its principal as a confirmed member, and publish a genesis at
/// `(1,1)`. The one `credential` authenticates BOTH the genesis WRITE and the follower's later READ (a
/// confirmed member reads every skill in the workspace — the follower presents this same credential).
pub(crate) struct GenesisSpec<'a> {
    pub(crate) dkid: &'a str,
    /// The device's registered 32-byte public key — a stable non-secret NAME; nothing verifies against it
    /// (git/GitHub-level trust). Authorization is the presented credential's registry-row lookup.
    pub(crate) device_pubkey: &'a [u8; 32],
    pub(crate) op_id: &'a str,
    pub(crate) files: Vec<UploadedFile>,
    pub(crate) principal: &'a str,
    pub(crate) author: &'a str,
    pub(crate) message: &'a str,
    pub(crate) created_at: &'a str,
    /// The workspace Bearer credential the publisher device holds — and the one a follower presents to read
    /// (it replaced the per-skill read token, which is gone).
    pub(crate) credential: &'a str,
}

/// Run a [`GenesisSpec`] against a fresh authority; returns the genesis version id.
pub(crate) async fn seed_genesis_plane(authority: &Authority, spec: GenesisSpec<'_>) -> CommitId {
    let ws = WorkspaceId::parse(WS).unwrap();
    let skill = SkillId::parse(SKILL).unwrap();

    // The publisher device + its credential + a confirmed-member seat — the whole authorization for the
    // genesis write and every subsequent read under this credential (per-skill roster grants nothing).
    seed_member(
        authority,
        &ws,
        spec.dkid,
        spec.device_pubkey,
        spec.principal,
        "member",
        spec.credential,
    )
    .await;
    let receipt = authority
        .seed_published_genesis(
            &ws,
            &skill,
            spec.credential,
            &OpId::parse(spec.op_id).unwrap(),
            spec.files,
            spec.author,
            spec.message,
            None,
            spec.created_at,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(Generation { epoch: 1, seq: 1 }));
    receipt.version_id.expect("genesis version id")
}

/// Mint an owner-driven `/i/` invite pre-offering `skill` to `email` at the `Member` role. The acting
/// `owner_credential` is the presented workspace Bearer credential the plane resolves to its registry row →
/// principal → OWNER role gate (nothing is signed — authority is the directory rows). See
/// [`mint_invite_with_role`] for the role-selectable form (the multi-workspace e2e mints owner-role invites
/// so the joiner can itself invite).
pub(crate) async fn mint_invite(
    authority: &Authority,
    ws: &WorkspaceId,
    owner_credential: &str,
    op_id: &str,
    email: &str,
    skill: &str,
    at: &str,
) -> String {
    mint_invite_with_role(
        authority,
        ws,
        owner_credential,
        op_id,
        email,
        skill,
        None,
        Role::Member,
        at,
    )
    .await
}

/// [`mint_invite`] with an explicit `role` and an optional OFFERED NAME for the skill. The request presents
/// the acting `owner_credential`; the plane resolves its non-revoked registry row → principal → OWNER role
/// gate (no signature — authority is the directory rows). The offered `name` is advisory (it becomes the
/// follower's local skill name, never part of the request identity — the deterministic link binds skill
/// ids only), so the multi-workspace e2e uses it to give a skill the SAME name in two workspaces and prove
/// `--workspace` disambiguation.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mint_invite_with_role(
    authority: &Authority,
    ws: &WorkspaceId,
    owner_credential: &str,
    op_id: &str,
    email: &str,
    skill: &str,
    name: Option<&str>,
    role: Role,
    at: &str,
) -> String {
    let request = GovernanceRequest {
        credential: owner_credential.to_owned(),
        op: GovernanceOp::Invite {
            role,
            expires_at: None,
            emails: vec![Principal::parse(email).unwrap()],
            skills: vec![(SkillId::parse(skill).unwrap(), name.map(str::to_owned))],
        },
    };
    match authority
        .create_invite(ws, op_id, request, at, NOW)
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
