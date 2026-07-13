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
    Authority, BundleId, CommitId, ConfirmOutcome, DeploymentMode, EnrollmentConfig, FileMode,
    InviteOutcome, OpId, Principal, UploadedFile, WorkspaceId,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};
use topos::test_support::FollowHarness;
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

// ── the composed-stack provisioning (the door-cutover shape) ────────────────────────────────────────

/// The shared internal-lane bearer the composed stack arms on both sides (test-only value).
pub(crate) const INTERNAL_TOKEN: &str = "e2e-internal-token";

/// [`provision_pg`], but in the DOOR-CUTOVER shape the web app requires: the plane schema is a real
/// `plane` schema (the app's Drizzle mirror reads `plane.*` schema-qualified), the `topos_web` role
/// exists BEFORE the migrations run (0019's role-guarded grant block must execute, not skip), and
/// the role gets the same database-level shape the e2e bootstrap provisions (CONNECT is PUBLIC's
/// default; CREATE for the app's own `web` schema; the `web, plane` search_path). Returns the
/// authority-facing pool (search_path pinned to `plane`) plus the web-role URL the app dials.
pub(crate) async fn provision_pg_composed() -> (PgPool, String) {
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::var("DATABASE_URL")
        .expect("the e2e suite requires DATABASE_URL to point at a Postgres");
    let opts: PgConnectOptions = base
        .parse()
        .expect("DATABASE_URL must be a valid Postgres connection string");
    let name = format!(
        "topos_e2e_web_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );

    let mut admin = PgConnection::connect_with(&opts)
        .await
        .expect("connect to the base Postgres database");
    // The role is cluster-wide and race-shared across parallel test binaries. Create it if
    // absent, then ENFORCE the password unconditionally: on a shared dev cluster the role may
    // already exist from an earlier run (or another suite) with a different password, and a bare
    // `IF NOT EXISTS ... CREATE` would leave that stale password in place — the spawned app then
    // can't authenticate and its `/healthz` 503s until the harness times out. Setting the
    // password every time makes the app's login deterministic regardless of prior cluster state.
    //
    // A cluster-wide advisory lock serializes the role mutation: two binaries ALTERing the same
    // `pg_authid` tuple at once raise "tuple concurrently updated" (Postgres won't concurrently
    // update one catalog row). All writers target the SAME password, so serializing yields the
    // same end state without the race. The lock is session-scoped and released on `admin.close()`.
    admin
        .execute("SELECT pg_advisory_lock(hashtext('topos_web_role_setup'))")
        .await
        .expect("acquire role-setup advisory lock");
    admin
        .execute(
            r#"DO $$
            BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topos_web') THEN
                    CREATE ROLE topos_web LOGIN PASSWORD 'web';
                ELSE
                    ALTER ROLE topos_web LOGIN PASSWORD 'web';
                END IF;
            END $$"#,
        )
        .await
        .expect("ensure topos_web role");
    admin
        .execute("SELECT pg_advisory_unlock(hashtext('topos_web_role_setup'))")
        .await
        .expect("release role-setup advisory lock");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create the per-test database");
    admin
        .execute(format!(r#"GRANT CREATE ON DATABASE "{name}" TO topos_web"#).as_str())
        .await
        .expect("grant create to topos_web");
    admin
        .execute(
            format!(r#"ALTER ROLE topos_web IN DATABASE "{name}" SET search_path = web, plane"#)
                .as_str(),
        )
        .await
        .expect("set topos_web search_path");
    admin.close().await.ok();

    // Migrate INTO schema `plane` (production's layout — the app's read-only mirror is
    // schema-qualified), on a pool whose search_path stays pinned there so the authority's
    // unqualified SQL keeps resolving.
    let host = opts.get_host().to_owned();
    let port = opts.get_port();
    let pool = PgPoolOptions::new()
        .connect_with(opts.database(&name).options([("search_path", "plane")]))
        .await
        .expect("connect to the per-test database");
    pool.execute("CREATE SCHEMA IF NOT EXISTS plane")
        .await
        .expect("create the plane schema");
    plane_store::MIGRATOR
        .run(&pool)
        .await
        .expect("migrate the per-test database");

    let web_url = format!("postgres://topos_web:web@{host}:{port}/{name}");
    (pool, web_url)
}

/// The spawned web app (the door) — `bun run start` over the production build, killed on drop.
pub(crate) struct AppServer {
    child: std::process::Child,
    /// The app's public origin (`http://127.0.0.1:<port>`).
    pub(crate) origin: String,
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Repo root (this crate lives at `<repo>/tests`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/ has a parent")
        .to_path_buf()
}

/// Pick a free loopback port by binding :0 and dropping the socket (a small race with other
/// processes is acceptable in a test harness — the bind failure would fail loudly).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind a probe socket")
        .local_addr()
        .expect("probe local addr")
        .port()
}

/// Spawn the web app over its PRODUCTION build (`web/build/server/index.js` must exist — CI builds
/// it before `cargo test`; locally run `cd web && bun install && bun run build` once) and wait for
/// `/healthz`. The app connects to the composed database as `topos_web` and reaches the loopback
/// plane over the armed internal lane.
pub(crate) fn spawn_app(web_db_url: &str, plane_base: &str, app_port: u16) -> AppServer {
    let web_dir = repo_root().join("web");
    let build = web_dir.join("build").join("server").join("index.js");
    assert!(
        build.exists(),
        "the composed e2e needs the web app's production build — run `cd web && bun install && bun run build` first"
    );
    let origin = format!("http://127.0.0.1:{app_port}");
    // Spawn NODE directly (not `bun run start`): `bun run start` delegates to a node grandchild via
    // the `react-router-serve` shebang, and `child.kill()` on the bun wrapper would leave that node
    // process serving — the app would never actually die (leaking the listener, and defeating the
    // "unreachable plane" e2e that drops the stack to prove the freeze). Running node itself makes
    // the spawned child the real server, so Drop reaps it. `react-router-serve`'s entry is Node-native
    // (`@react-router/node` + `renderToPipeableStream`); bun's runtime cannot serve it, so node is
    // also the correct runtime — the same command the production image's CMD runs.
    let serve_bin = web_dir
        .join("node_modules")
        .join("@react-router")
        .join("serve")
        .join("bin.cjs");
    let child = std::process::Command::new("node")
        .arg(&serve_bin)
        .arg("./build/server/index.js")
        .current_dir(&web_dir)
        .env("PORT", app_port.to_string())
        .env("HOST", "127.0.0.1")
        .env("DATABASE_URL", web_db_url)
        .env("PLANE_INTERNAL_URL", plane_base)
        .env("PLANE_INTERNAL_TOKEN", INTERNAL_TOKEN)
        .env(
            "BETTER_AUTH_SECRET",
            "e2e-secret-0123456789abcdef0123456789abcdef",
        )
        .env("BETTER_AUTH_URL", &origin)
        .env("APP_ENV", "test")
        .env("TOPOS_WEB_RATELIMIT", "off")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn the web app (is `bun` on PATH?)");
    let mut app = AppServer { child, origin };
    let health = format!("{}/healthz", app.origin);
    for _ in 0..120 {
        if let Some(status) = app.child.try_wait().expect("poll the web app process") {
            panic!("the web app exited during startup: {status}");
        }
        if ureq::get(&health).call().is_ok() {
            return app;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    panic!("the web app never answered /healthz at {health}");
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

/// A running loopback plane — and, for the composed variants ([`start_stack`]), the web app in
/// front of it (the door): `base_url` is then the APP's `/api` base the CLI dials, and the plane
/// itself has no public face at all. Holds the runtime + authority handle alive for the test's
/// duration; `app` drops first (the door dies before the vault), `_dir` LAST so the served store
/// outlives the runtime/authority.
pub(crate) struct Plane {
    /// The spawned web app, when this is a composed stack (None for a bare loopback plane).
    app: Option<AppServer>,
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) authority: Arc<Authority>,
    /// The provisioned per-test database — for direct row-level witnesses only (e.g. the standup chain's
    /// "the admin_claim table stayed empty"), never a second write path.
    pub(crate) pool: PgPool,
    pub(crate) base_url: String,
    /// The base the minted `/i/` links ride — `base_url` unless the plane was started split
    /// ([`start_plane_split`]) or composed ([`start_stack`]: the app ORIGIN, address-shaped —
    /// resource addresses carry no `/api`).
    pub(crate) link_base_url: String,
    seeded: Seeded,
    _dir: Scratch,
}

impl Plane {
    pub(crate) fn ws(&self) -> WorkspaceId {
        WorkspaceId::parse(WS).unwrap()
    }

    pub(crate) fn skill(&self) -> BundleId {
        BundleId::parse(SKILL).unwrap()
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

/// Stand the COMPOSED stack up — the door-cutover shape every CLI flow now runs: the loopback
/// plane serves only the vault surface (byte/pointer + enrollment + the internal lane, its
/// internal token armed), the web app is spawned in FRONT of it, and the returned
/// [`Plane::base_url`] is the APP's `/api` base — exactly what the protocol card teaches a real
/// client to dial. Suites keep their seed closures and harness calls; only the door moved.
pub(crate) fn start_stack(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    start_stack_mode(scratch_prefix, tag, enrollment, DeploymentMode::Cloud, seed)
}

/// [`start_stack`] with an explicit deployment posture (the standup chain needs self-host).
pub(crate) fn start_stack_mode(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    mode: DeploymentMode,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    start_stack_impl(scratch_prefix, tag, enrollment, mode, seed)
}

fn start_stack_impl(
    scratch_prefix: &str,
    tag: &str,
    enrollment: bool,
    mode: DeploymentMode,
    seed: impl AsyncFnOnce(&Authority) -> Seeded,
) -> Plane {
    let dir = Scratch::new(scratch_prefix, tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // The APP's port is chosen FIRST: the vault's enrollment disclosure (the bootstrap plane
    // block, the card, every minted link) must name the DOOR, never the vault's own loopback
    // address — a client that re-rooted onto the vault would prove the wrong topology.
    let app_port = free_port();
    let app_origin = format!("http://127.0.0.1:{app_port}");
    let app_api_base = format!("{app_origin}/api");
    // Resource addresses (and the card + `/i/` + `/verify` links) live at the app ORIGIN root; the
    // device API the card declares is that origin's `/api` mount. The app derives `api_base_url`
    // from the request origin, so the two share a host by construction — there is no separate plane
    // host to point at (the app is the door, not a proxy to one).
    let link_origin = app_origin.clone();

    let listener = rt
        .block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await })
        .expect("bind loopback listener");
    let plane_addr = listener.local_addr().expect("local addr");
    let plane_base = format!("http://{plane_addr}");

    let (authority, seeded, pool, web_db_url) = rt.block_on(async {
        let (pool, web_db_url) = provision_pg_composed().await;
        let mut authority =
            Authority::from_pool(pool.clone(), &dir.0.join("git"), &dir.0.join("large"))
                .expect("open authority");
        if enrollment {
            authority = authority
                .with_enrollment_config(EnrollmentConfig {
                    secret_path: dir.0.join("enroll.key"),
                    base_url: app_api_base.clone(),
                    verify_base_url: Some(link_origin.clone()),
                    link_base_url: Some(link_origin.clone()),
                    deployment_mode: mode,
                    enrollment_method: "device_code".to_owned(),
                })
                .expect("load enrollment secret");
        }
        let seeded = seed(&authority).await;
        (authority, seeded, pool, web_db_url)
    });

    let authority = Arc::new(authority);
    let state = PlaneState::new(authority.clone()).with_internal_token(INTERNAL_TOKEN);
    rt.spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let app = spawn_app(&web_db_url, &plane_base, app_port);

    Plane {
        app: Some(app),
        rt,
        authority,
        pool,
        base_url: app_api_base,
        link_base_url: link_origin,
        seeded,
        _dir: dir,
    }
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
        app: None,
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
    let skill = BundleId::parse(SKILL).unwrap();

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

/// The ADDRESS name `seed_workspace` derives for the shared [`WS`] id (`w_acme` slugifies to `w-acme`) —
/// the workspace slug a member joins by. A member's join target is `<base_url>/<WS_NAME>` ([`ws_address`]).
pub(crate) const WS_NAME: &str = "w-acme";

/// The full workspace ADDRESS a `follow` targets against a loopback plane: `<base_url>/<name>`. The real
/// client fetches the constant protocol card here (`Accept: application/json` → `WireProtocolCard`),
/// re-roots onto the card's `api_base_url`, and device-authorizes toward the ADDRESS name (intent enroll).
pub(crate) fn ws_address(base_url: &str) -> String {
    format!("{base_url}/{WS_NAME}")
}

/// Seat `emails` as INVITED members through the REAL invitation op ([`Authority::invite`]) — the
/// member-lane roster write the reshaped `invite` verb (and `POST …/invitations`) drive; the tokened
/// `/i/` invite link is gone, an invitation is a roster row and nothing more. The acting
/// `owner_credential` is the presented workspace Bearer credential the plane resolves to its registry row
/// → principal → role gate (nothing is signed — authority is the directory rows). `channels` pre-places
/// the invitees (resolve-all-or-apply-none). Returns the invited principals in their canonical folded
/// form; panics on any denial (a test-precondition error).
pub(crate) async fn invite_member(
    authority: &Authority,
    ws: &WorkspaceId,
    owner_credential: &str,
    emails: &[&str],
    channels: &[&str],
    at: &str,
) -> Vec<String> {
    let emails: Vec<String> = emails.iter().map(|e| (*e).to_owned()).collect();
    let channels: Vec<String> = channels.iter().map(|c| (*c).to_owned()).collect();
    match authority
        .invite(ws, owner_credential, &emails, &channels, at)
        .await
        .expect("the invite op runs")
    {
        InviteOutcome::Invited { invited } => invited,
        other => panic!("invite denied: {other:?}"),
    }
}

/// Drive the REAL client `follow <address>` **call 1** (fetch the protocol card over the real socket →
/// re-root → device-authorize toward the ADDRESS name → the pending WAL) and complete the human identity
/// leg IN-PROCESS via the authority's external-confirm op (the same lever a web verification page calls,
/// so the flow is headless — the agent only ever polls). `email` is the identity proven on the
/// verification page (the principal the redeem seats). Leaves the caller to resume
/// (`resume_describe` / `resume_apply`), which polls (granted) → redeems → promotes → continues into the
/// follow intent.
pub(crate) fn begin_address_enroll(
    plane: &Plane,
    client: &FollowHarness,
    address: &str,
    email: &str,
) {
    let pending = client.follow(address).expect("follow call 1 (address)");
    assert!(!pending.enrolled, "call 1 only begins enrollment");
    let user_code = pending
        .pending
        .expect("the pending arm carries the verification handle")
        .user_code;
    let confirm = plane
        .rt
        .block_on(
            plane
                .authority
                .confirm_external_identity(&user_code, email, NOW),
        )
        .expect("confirm the session identity");
    assert!(matches!(confirm, ConfirmOutcome::Confirmed));
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
