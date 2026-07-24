//! Shared harness for the composed-stack e2e tests: a fresh per-test Postgres database provisioned by
//! the PRODUCTION recipe (two roles, two schemas, two migration lineages), an in-process vault (the
//! `topos_plane::router` custody lane, internal-token-armed, loopback-only), and the REAL web app
//! spawned from its production build in front of it. The app is the ONE public surface: the harness
//! drives every ceremony over HTTP exactly as a person (cookie sessions: claim, sign-in, the /verify
//! device approval) or a device (the Bearer lane under `/api/v1`) would.
//!
//! Each e2e runs a **blocking `ureq` client** on the test thread alongside the vault server on a
//! self-owned **multi-thread** runtime, so it cannot use `#[sqlx::test]` — that macro drives the test
//! on a current-thread runtime, where the blocking client would starve the server and deadlock.
//!
//! The provisioned databases are left behind on the target Postgres — the CI / local build Postgres is
//! disposable (a container), and dropping a database while its pool still holds connections is racy.
//!
//! Write-path discipline: everything the product has a surface for goes THROUGH that surface (claim,
//! sign-in, /verify, the device lane, the admin pages). The superuser pool is for row-level witnesses,
//! plus the few arrangement steps the OSS product deliberately has no mail-less surface for (seating an
//! extra account, flipping the registration knob) — each such helper says so.

// Each e2e binary compiles this module independently and drives a SUBSET of the harness — what one
// binary leaves unused is exercised by a sibling, so the module-level allow is deliberate.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plane_store::Authority;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool, Row};
use topos::test_support::SessionInstall;
use topos_plane::{PlaneState, router};

// ── the shared scenario constants ───────────────────────────────────────────────────────────────────

/// The boot-minted workspace's ADDRESS slug (`TOPOS_WORKSPACE_NAME`) — what a `follow` targets.
pub(crate) const WS_NAME: &str = "acme";
/// The one skill most scenarios distribute.
pub(crate) const SKILL: &str = "s-deploy";
/// The preset first-boot claim code (`TOPOS_SETUP_CODE` — stable across boots, like CI/IaC).
pub(crate) const SETUP_CODE: &str = "e2e-setup-code-0123456789abcdef";
/// The one password every harness account uses (better-auth minimum is 8).
pub(crate) const PASSWORD: &str = "e2e-password-1234";
/// The first owner (the claimant).
pub(crate) const OWNER_EMAIL: &str = "owner@acme.test";
/// The shared internal-lane bearer the composed stack arms on both sides (test-only value).
pub(crate) const INTERNAL_TOKEN: &str = "e2e-internal-token";

// ── per-test Postgres provisioning (the production recipe) ──────────────────────────────────────────

/// The two application roles' test passwords (mirroring the compose defaults).
const PLANE_PW: &str = "plane";
const WEB_PW: &str = "web";

/// Create a uniquely-named database on the `$DATABASE_URL` server and provision it by the PRODUCTION
/// first-boot recipe (two LOGIN roles, two schemas each owned by its role, role-level search_paths,
/// the ALTER DEFAULT PRIVILEGES chain that keeps the app's read-only custody mirror current), then run
/// both migration lineages: the vault's sqlx migrations AS `topos_plane`, the app's drizzle lineage AS
/// `topos_web` via the app's own `scripts/migrate.mjs`. Returns the superuser witness pool, the
/// vault-facing pool (connected as `topos_plane`, search_path pinned to `plane`), and the web-role URL
/// the spawned app dials.
async fn provision_pg() -> (PgPool, PgPool, String) {
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

    let mut admin = PgConnection::connect_with(&opts)
        .await
        .expect("connect to the base Postgres database");
    // The roles are cluster-wide and race-shared across parallel test binaries. Create-if-absent,
    // then ENFORCE the password unconditionally (a stale role from an earlier run may hold another
    // password, and the spawned app's login would 503 until the harness times out). A cluster-wide
    // advisory lock serializes the role mutation — two binaries ALTERing the same pg_authid tuple
    // at once raise "tuple concurrently updated"; all writers target the same end state.
    admin
        .execute("SELECT pg_advisory_lock(hashtext('topos_e2e_role_setup'))")
        .await
        .expect("acquire role-setup advisory lock");
    for (role, pw) in [("topos_plane", PLANE_PW), ("topos_web", WEB_PW)] {
        admin
            .execute(
                format!(
                    r#"DO $$
                    BEGIN
                        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN
                            CREATE ROLE {role} LOGIN PASSWORD '{pw}';
                        ELSE
                            ALTER ROLE {role} LOGIN PASSWORD '{pw}';
                        END IF;
                    END $$"#
                )
                .as_str(),
            )
            .await
            .expect("ensure app role");
    }
    admin
        .execute("SELECT pg_advisory_unlock(hashtext('topos_e2e_role_setup'))")
        .await
        .expect("release role-setup advisory lock");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create the per-test database");
    admin.close().await.ok();

    // Per-database provisioning, superuser-side — the same statements compose-init-db.sh runs.
    let host = opts.get_host().to_owned();
    let port = opts.get_port();
    let admin_pool = PgPoolOptions::new()
        .connect_with(opts.database(&name))
        .await
        .expect("connect to the per-test database");
    for stmt in [
        format!(r#"REVOKE ALL ON DATABASE "{name}" FROM PUBLIC"#),
        format!(r#"GRANT CONNECT ON DATABASE "{name}" TO topos_plane"#),
        format!(r#"GRANT CONNECT ON DATABASE "{name}" TO topos_web"#),
        // The app's migrator issues CREATE SCHEMA IF NOT EXISTS, and Postgres checks the CREATE
        // privilege before the existence short-circuit.
        format!(r#"GRANT CREATE ON DATABASE "{name}" TO topos_web"#),
        format!(r#"ALTER ROLE topos_web IN DATABASE "{name}" SET search_path = web, plane"#),
        format!(r#"ALTER ROLE topos_plane IN DATABASE "{name}" SET search_path = plane"#),
        "CREATE SCHEMA IF NOT EXISTS web AUTHORIZATION topos_web".to_owned(),
        "CREATE SCHEMA IF NOT EXISTS plane AUTHORIZATION topos_plane".to_owned(),
        "GRANT USAGE ON SCHEMA plane TO topos_web".to_owned(),
        // The app's read-only custody mirror: every table a plane migration adds arrives already
        // SELECT-granted — set BEFORE the plane lineage runs, exactly like first boot.
        "ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane GRANT SELECT ON TABLES TO topos_web"
            .to_owned(),
    ] {
        admin_pool
            .execute(stmt.as_str())
            .await
            .expect("provision the per-test database");
    }

    // The vault lineage, AS topos_plane (ownership + the default-privileges chain match production).
    let plane_opts: PgConnectOptions =
        format!("postgres://topos_plane:{PLANE_PW}@{host}:{port}/{name}")
            .parse()
            .expect("compose the plane role URL");
    let plane_pool = PgPoolOptions::new()
        .connect_with(plane_opts.options([("search_path", "plane")]))
        .await
        .expect("connect as topos_plane");
    plane_store::MIGRATOR
        .run(&plane_pool)
        .await
        .expect("migrate the vault schema");

    // The app lineage, AS topos_web, through the app's OWN migrator (records the drizzle ledger the
    // running app's first-request migration then finds and no-ops on).
    let web_url = format!("postgres://topos_web:{WEB_PW}@{host}:{port}/{name}");
    let status = std::process::Command::new("node")
        .arg(repo_root().join("web").join("scripts").join("migrate.mjs"))
        .current_dir(repo_root().join("web"))
        .env("DATABASE_URL", &web_url)
        .status()
        .expect("run the app's drizzle migrator (is `node` on PATH?)");
    assert!(status.success(), "the drizzle migrator failed");

    (admin_pool, plane_pool, web_url)
}

// ── the spawned web app ─────────────────────────────────────────────────────────────────────────────

/// The spawned web app (the door) — node over the production build, killed on drop.
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

/// Spawn the web app over its PRODUCTION build (`web/build/server/index.js` must exist — CI builds it
/// before `cargo test`; locally run `cd web && bun install && bun run build` once) and wait for
/// `/healthz`. The app connects to the composed database as `topos_web` and reaches the loopback
/// vault over the armed internal lane. `link_file` receives the printed first-boot claim line.
fn spawn_app(
    web_db_url: &str,
    plane_base: &str,
    app_port: u16,
    link_file: &std::path::Path,
    mail_armed: bool,
) -> AppServer {
    let web_dir = repo_root().join("web");
    let build = web_dir.join("build").join("server").join("index.js");
    assert!(
        build.exists(),
        "the composed e2e needs the web app's production build — run `cd web && bun install && bun run build` first"
    );
    let origin = format!("http://127.0.0.1:{app_port}");
    // Spawn NODE directly (not `bun run start`): `bun run start` delegates to a node grandchild via
    // the `react-router-serve` shebang, and `child.kill()` on the bun wrapper would leave that node
    // process serving — the app would never actually die. `react-router-serve`'s entry is Node-native
    // (`@react-router/node` + `renderToPipeableStream`), the same command the production image runs.
    let serve_bin = web_dir
        .join("node_modules")
        .join("@react-router")
        .join("serve")
        .join("bin.cjs");
    let mut cmd = std::process::Command::new("node");
    cmd.arg(&serve_bin)
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
        .env("TOPOS_WORKSPACE_NAME", WS_NAME)
        .env("TOPOS_SETUP_CODE", SETUP_CODE)
        .env("TOPOS_SETUP_LINK_FILE", link_file);
    if mail_armed {
        // Dummy relay coordinates: `APP_ENV=test` records every mail to the dev outbox files in
        // the web dir and never dials a socket — but an ARMED transport flips the mail-rung
        // gates on (inviting requires it, and the invitation page's account mint goes
        // passwordless through the magic-link rung the composition arms alongside it). The
        // default suites stay SMTP-UNSET: the whole enrolled loop must work with zero delivery.
        cmd.env("TOPOS_MAIL_SMTP_HOST", "127.0.0.1")
            .env("TOPOS_MAIL_SMTP_PORT", "2525")
            .env("TOPOS_MAIL_SMTP_USER", "e2e")
            .env("TOPOS_MAIL_SMTP_PASS", "e2e")
            .env("TOPOS_MAIL_SMTP_FROM", "Topos <no-reply@e2e.test>");
    }
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn the web app (is `node` on PATH?)");
    let mut app = AppServer { child, origin };
    let health = format!("{}/healthz", app.origin);
    for _ in 0..240 {
        if let Some(status) = app.child.try_wait().expect("poll the web app process") {
            panic!("the web app exited during startup: {status}");
        }
        if ureq::get(&health).call().is_ok() {
            return app;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("the web app never answered /healthz at {health}");
}

// ── scratch dirs ────────────────────────────────────────────────────────────────────────────────────

/// A self-cleaning temp dir (RAII).
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(prefix: &str, tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create stack scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ── the blocking HTTP session (a browser stand-in: cookie jar + form posts) ─────────────────────────

/// One signed-in browser session against the app: a `ureq` agent with redirects OFF (a 302 is an
/// asserted outcome, not something to chase) and a manual cookie jar (set-cookie absorbed from every
/// response, sent back on every request). Every POST carries the app's own `Origin` (better-auth's
/// CSRF check refuses a credential POST without one).
pub(crate) struct Session {
    agent: ureq::Agent,
    origin: String,
    cookies: Mutex<BTreeMap<String, String>>,
}

/// A response the session hands back: the status + the body text (empty when unreadable) + the
/// redirect target when the answer was one (redirects are OFF — a 302 is an asserted outcome).
pub(crate) struct HttpAnswer {
    pub(crate) status: u16,
    pub(crate) body: String,
    pub(crate) location: Option<String>,
}

fn blocking_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_recv_body(Some(Duration::from_secs(30)))
            .build(),
    )
}

/// Percent-encode one `application/x-www-form-urlencoded` value (everything but the unreserved set).
fn form_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl Session {
    pub(crate) fn new(origin: &str) -> Self {
        Self {
            agent: blocking_agent(),
            origin: origin.to_owned(),
            cookies: Mutex::new(BTreeMap::new()),
        }
    }

    fn cookie_header(&self) -> Option<String> {
        let jar = self.cookies.lock().expect("cookie jar");
        if jar.is_empty() {
            return None;
        }
        Some(
            jar.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn absorb_cookies(&self, resp: &ureq::http::Response<ureq::Body>) {
        let mut jar = self.cookies.lock().expect("cookie jar");
        for value in resp.headers().get_all(ureq::http::header::SET_COOKIE) {
            let Ok(raw) = value.to_str() else { continue };
            let pair = raw.split(';').next().unwrap_or("");
            if let Some((name, val)) = pair.split_once('=') {
                jar.insert(name.trim().to_owned(), val.trim().to_owned());
            }
        }
    }

    fn read(&self, mut resp: ureq::http::Response<ureq::Body>) -> HttpAnswer {
        self.absorb_cookies(&resp);
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = String::new();
        let _ = resp
            .body_mut()
            .as_reader()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut body);
        HttpAnswer {
            status,
            body,
            location,
        }
    }

    /// GET a path (an in-app path like `/login`, or `?`-suffixed) with the browser `Accept`.
    pub(crate) fn get(&self, path: &str) -> HttpAnswer {
        let mut req = self
            .agent
            .get(format!("{}{path}", self.origin))
            .header("Accept", "text/html,application/xhtml+xml");
        if let Some(cookie) = self.cookie_header() {
            req = req.header("Cookie", cookie);
        }
        self.read(req.call().expect("GET over loopback"))
    }

    /// POST an HTML form (`application/x-www-form-urlencoded`) — how every page ceremony submits.
    pub(crate) fn post_form(&self, path: &str, fields: &[(&str, &str)]) -> HttpAnswer {
        let body = fields
            .iter()
            .map(|(k, v)| format!("{}={}", form_escape(k), form_escape(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut req = self
            .agent
            .post(format!("{}{path}", self.origin))
            .header("Origin", &self.origin)
            // The browser Accept: a document POST without it would be answered by the constant
            // protocol card (the non-browser face) instead of the page this session emulates.
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Content-Type", "application/x-www-form-urlencoded");
        if let Some(cookie) = self.cookie_header() {
            req = req.header("Cookie", cookie);
        }
        self.read(req.send(body.as_bytes()).expect("POST form over loopback"))
    }

    /// POST a JSON body (the better-auth REST rungs).
    pub(crate) fn post_json(&self, path: &str, body: &serde_json::Value) -> HttpAnswer {
        let payload = serde_json::to_vec(body).expect("serialize JSON body");
        let mut req = self
            .agent
            .post(format!("{}{path}", self.origin))
            .header("Origin", &self.origin)
            .header("Content-Type", "application/json");
        if let Some(cookie) = self.cookie_header() {
            req = req.header("Cookie", cookie);
        }
        self.read(
            req.send(payload.as_slice())
                .expect("POST json over loopback"),
        )
    }

    /// Whether the jar holds a session cookie (a claim/sign-in landed one).
    pub(crate) fn signed_in(&self) -> bool {
        self.cookies
            .lock()
            .expect("cookie jar")
            .keys()
            .any(|k| k.contains("session_token"))
    }
}

// ── the composed stack ──────────────────────────────────────────────────────────────────────────────

/// A session-lane grant the harness minted for itself over the REAL login flow — a probe session
/// for wire-level assertions the CLI has no verb for (channel curation, the uniform-404 probes).
pub(crate) struct SessionGrant {
    /// The workspace-scoped bearer credential (the promoted flow code).
    pub(crate) credential: String,
    /// The minted session's id (`sn_…` — the non-secret handle).
    pub(crate) session_id: String,
    /// The session's born status (`active`, or `pending` under the workspace's session-approval
    /// knob). Absent on an older producer ⇒ active.
    pub(crate) session_status: Option<String>,
}

/// The whole composed stack, one per test: the per-test database, the in-process vault, the spawned
/// web app, and the boot-minted (unclaimed) workspace.
pub(crate) struct Stack {
    /// The spawned web app; dropped FIRST (the door dies before the vault).
    app: AppServer,
    pub(crate) rt: tokio::runtime::Runtime,
    /// The superuser witness pool over the per-test database (row-level witnesses + the named
    /// mail-less arrangement helpers — never a general write path).
    pub(crate) pool: PgPool,
    /// The app's public origin (`http://127.0.0.1:<port>`).
    pub(crate) origin: String,
    /// The device-lane base the protocol card declares (`<origin>/api`).
    pub(crate) api_base: String,
    /// The boot-minted workspace's row id (`w_…`).
    pub(crate) workspace_id: String,
    /// Where the printed claim link also lands (`TOPOS_SETUP_LINK_FILE`).
    pub(crate) setup_link_file: PathBuf,
    _dir: Scratch,
}

/// Stand the composed stack up: provision the database by the production recipe, serve the vault
/// in-process (internal token armed, loopback-only, no public face), spawn the real web app in front
/// of it, and poke ONE document request so first-boot setup mints the workspace (with the preset
/// claim code). The workspace is returned UNCLAIMED — `claim_owner` is the first ceremony.
pub(crate) fn start_stack(tag: &str) -> Stack {
    start_stack_with(tag, false)
}

/// [`start_stack`] with the app's mail transport ARMED (dummy coordinates; `APP_ENV=test` records
/// to the web dir's dev outbox files instead of dialing) — for the invitation-redemption suite,
/// whose invite ceremony and passwordless account mint both ride the mail rung.
pub(crate) fn start_stack_mailed(tag: &str) -> Stack {
    start_stack_with(tag, true)
}

fn start_stack_with(tag: &str, mail_armed: bool) -> Stack {
    let dir = Scratch::new("topos-e2e", tag);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let (admin_pool, plane_pool, web_url) = rt.block_on(provision_pg());

    // The in-process vault: the custody authority over the plane-role pool, served loopback-only.
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind the vault listener");
    let plane_addr = listener.local_addr().expect("vault local addr");
    let plane_base = format!("http://{plane_addr}");
    let authority = Authority::from_pool(plane_pool, &dir.0.join("git"), &dir.0.join("large"))
        .expect("open the custody authority");
    let state = PlaneState::new(Arc::new(authority)).with_internal_token(INTERNAL_TOKEN);
    rt.spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let app_port = free_port();
    let setup_link_file = dir.0.join("setup-link.txt");
    let app = spawn_app(
        &web_url,
        &plane_base,
        app_port,
        &setup_link_file,
        mail_armed,
    );
    let origin = app.origin.clone();
    let api_base = format!("{origin}/api");

    // ONE document request boots the app tier: the (no-op) migration pass + first-boot setup, which
    // mints the workspace and prints the claim link. /healthz is a resource route and skips both.
    let probe = Session::new(&origin);
    let boot = probe.get("/login");
    assert_eq!(
        boot.status, 200,
        "the login page boots the app: {}",
        boot.body
    );

    // The boot-minted workspace row (the single-tenant read the app itself makes).
    let workspace_id: String = rt
        .block_on(
            sqlx::query_scalar::<_, String>("SELECT id FROM web.workspace").fetch_one(&admin_pool),
        )
        .expect("the boot-minted workspace row exists");

    Stack {
        app,
        rt,
        pool: admin_pool,
        origin,
        api_base,
        workspace_id,
        setup_link_file,
        _dir: dir,
    }
}

impl Stack {
    /// The workspace ADDRESS a `follow` targets (`<origin>/<slug>` — the shareable resource address).
    pub(crate) fn address(&self) -> String {
        format!("{}/{WS_NAME}", self.origin)
    }

    // ── ceremonies over HTTP ────────────────────────────────────────────────────────────────────

    /// The first-boot CLAIM: create the first account + seat it as the workspace's first owner,
    /// through the real `/claim` ceremony with the preset code. Returns the signed-in owner session
    /// (the claim lands the session cookies).
    pub(crate) fn claim_owner(&self, email: &str) -> Session {
        let session = Session::new(&self.origin);
        let page = session.get(&format!("/claim?code={SETUP_CODE}"));
        assert_eq!(
            page.status, 200,
            "the live claim link renders: {}",
            page.body
        );
        let name = email.split('@').next().unwrap_or("owner");
        let claimed = session.post_form(
            &format!("/claim?code={SETUP_CODE}"),
            &[
                ("code", SETUP_CODE),
                ("name", name),
                ("email", email),
                ("password", PASSWORD),
            ],
        );
        assert_eq!(
            claimed.status, 302,
            "the claim consumes the code and redirects signed-in: {}",
            claimed.body
        );
        assert!(session.signed_in(), "the claimant lands with a session");
        session
    }

    /// Sign an EXISTING account in through better-auth's email+password rung.
    pub(crate) fn sign_in(&self, email: &str) -> Session {
        let session = Session::new(&self.origin);
        let answer = session.post_json(
            "/api/auth/sign-in/email",
            &serde_json::json!({ "email": email, "password": PASSWORD }),
        );
        assert_eq!(answer.status, 200, "sign-in for {email}: {}", answer.body);
        assert!(session.signed_in(), "sign-in lands a session cookie");
        session
    }

    /// Sign a NEW account up through the normal flow (requires the registration knob open — see
    /// [`open_registration`](Self::open_registration)). An account is NOT a seat.
    pub(crate) fn sign_up(&self, email: &str) -> Session {
        let session = Session::new(&self.origin);
        let name = email.split('@').next().unwrap_or("user");
        let answer = session.post_json(
            "/api/auth/sign-up/email",
            &serde_json::json!({ "email": email, "password": PASSWORD, "name": name }),
        );
        assert_eq!(answer.status, 200, "sign-up for {email}: {}", answer.body);
        assert!(session.signed_in(), "sign-up lands a session cookie");
        session
    }

    /// A sign-up that must be REFUSED (registration closed, no invitation): returns the answer so
    /// the caller asserts the one constant, non-enumerating refusal.
    pub(crate) fn sign_up_expect_refused(&self, email: &str) -> HttpAnswer {
        let session = Session::new(&self.origin);
        let name = email.split('@').next().unwrap_or("user");
        session.post_json(
            "/api/auth/sign-up/email",
            &serde_json::json!({ "email": email, "password": PASSWORD, "name": name }),
        )
    }

    /// Approve a pending device flow AS the sessioned person: the `/verify` ceremony's approve arm.
    /// This is a PLAIN signed-in accept — a live session plus the explicit approve click is the whole
    /// ceremony (no re-authentication; the admin ceremonies confirm in proportion to their reach, but
    /// none re-authenticate).
    pub(crate) fn approve_device(&self, session: &Session, user_code: &str) {
        let answer = session.post_form("/verify", &[("intent", "approve"), ("code", user_code)]);
        assert_eq!(answer.status, 200, "the approve lands: {}", answer.body);
        assert!(
            answer.body.to_lowercase().contains("logged in"),
            "the approve confirmation renders: {}",
            answer.body
        );
    }

    /// Deny a pending login flow (a plain signed-in deny — denying mints nothing).
    pub(crate) fn deny_device(&self, session: &Session, user_code: &str) {
        let answer = session.post_form("/verify", &[("intent", "deny"), ("code", user_code)]);
        assert_eq!(answer.status, 200, "the deny lands: {}", answer.body);
        assert!(
            answer.body.to_lowercase().contains("denied"),
            "the deny confirmation renders: {}",
            answer.body
        );
    }

    /// Begin a CLI login (`topos login <address>` call 1 — pends with the user code) and approve
    /// it as `approver`. The caller resumes (`client.login(None)` — re-invoking IS the resume).
    pub(crate) fn login_begin_and_approve(&self, client: &SessionInstall, approver: &Session) {
        let pending = client.login(Some(&self.address())).expect("login call 1");
        assert_eq!(
            pending.session_status, "awaiting-approval",
            "call 1 only begins the flow"
        );
        let user_code = pending
            .pending
            .expect("the pending arm carries the verification handle")
            .user_code;
        self.approve_device(approver, &user_code);
    }

    /// [`login_begin_and_approve`](Self::login_begin_and_approve) plus the resume: the granted
    /// second call persists the session row. Returns the resumed receipt's workspace id.
    pub(crate) fn login_complete(&self, client: &SessionInstall, approver: &Session) -> String {
        self.login_begin_and_approve(client, approver);
        let granted = client.login(None).expect("login resume");
        assert!(granted.pending.is_none(), "the resume settles the flow");
        granted.workspace_id
    }

    /// Mint a PROBE session grant for the harness itself over the real login flow — for wire-level
    /// lane calls the CLI has no verb for (curation, the uniform-404 probes). The `/verify`
    /// approval mints the session against `workspace` (default the boot workspace) — born per the
    /// one rule: active, or pending under the workspace's session-approval knob.
    pub(crate) fn mint_session(&self, approver: &Session, requested_name: &str) -> SessionGrant {
        self.mint_session_in(approver, requested_name, WS_NAME)
    }

    /// [`mint_session`](Self::mint_session) against a NAMED workspace slug.
    pub(crate) fn mint_session_in(
        &self,
        approver: &Session,
        requested_name: &str,
        workspace: &str,
    ) -> SessionGrant {
        let start = self.device_post_json(
            None,
            "/v1/login/authorize",
            &serde_json::json!({ "requested_name": requested_name, "workspace": workspace }),
        );
        assert_eq!(start.status, 200, "login authorize: {}", start.body);
        let start: serde_json::Value =
            serde_json::from_str(&start.body).expect("login authorize JSON");
        let device_code = start["device_code"]
            .as_str()
            .expect("device_code")
            .to_owned();
        let user_code = start["user_code"].as_str().expect("user_code").to_owned();
        self.approve_device(approver, &user_code);
        let poll = self.device_post_json(
            None,
            "/v1/login/token",
            &serde_json::json!({ "device_code": device_code }),
        );
        assert_eq!(poll.status, 200, "login token: {}", poll.body);
        let poll: serde_json::Value = serde_json::from_str(&poll.body).expect("login token JSON");
        assert_eq!(
            poll["status"], "granted",
            "the approved flow grants: {poll}"
        );
        SessionGrant {
            credential: poll["credential"].as_str().expect("credential").to_owned(),
            session_id: poll["session_id"].as_str().expect("session_id").to_owned(),
            session_status: poll["session_status"].as_str().map(str::to_owned),
        }
    }

    // ── the raw device lane (Bearer requests against `<origin>/api`) ────────────────────────────

    fn device_request(
        &self,
        method: &str,
        credential: Option<&str>,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> HttpAnswer {
        let agent = blocking_agent();
        let url = format!("{}{path}", self.api_base);
        // Every method rides the with-body builder shape (a bodyless op sends zero bytes) so one
        // arm type serves GET/PUT/POST/DELETE alike.
        let mut req = match method {
            "GET" => agent.get(&url).force_send_body(),
            "PUT" => agent.put(&url),
            "POST" => agent.post(&url),
            "DELETE" => agent.delete(&url).force_send_body(),
            other => panic!("unsupported method {other}"),
        }
        .header("Accept", "application/json");
        if let Some(cred) = credential {
            req = req.header("Authorization", format!("Bearer {cred}"));
        }
        let resp = match body {
            Some(value) => {
                let payload = serde_json::to_vec(value).expect("serialize lane body");
                req.header("Content-Type", "application/json")
                    .send(payload.as_slice())
            }
            None => req.send(&[][..]),
        }
        .expect("device-lane request over loopback");
        let mut resp = resp;
        let status = resp.status().as_u16();
        let mut text = String::new();
        let _ = resp
            .body_mut()
            .as_reader()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut text);
        HttpAnswer {
            status,
            body: text,
            location: None,
        }
    }

    /// GET a device-lane path (e.g. `/v1/workspaces/{ws}/delivery`) under `credential`.
    pub(crate) fn device_get(&self, credential: &str, path: &str) -> HttpAnswer {
        self.device_request("GET", Some(credential), path, None)
    }

    /// PUT a bodyless device-lane row op.
    pub(crate) fn device_put(&self, credential: &str, path: &str) -> HttpAnswer {
        self.device_request("PUT", Some(credential), path, None)
    }

    /// DELETE a device-lane row op (bodyless unless `body`).
    pub(crate) fn device_delete(
        &self,
        credential: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> HttpAnswer {
        self.device_request("DELETE", Some(credential), path, body)
    }

    /// POST a JSON body on the device lane (`credential` optional — the device-auth start is bare).
    pub(crate) fn device_post_json(
        &self,
        credential: Option<&str>,
        path: &str,
        body: &serde_json::Value,
    ) -> HttpAnswer {
        self.device_request("POST", credential, path, Some(body))
    }

    // ── the named mail-less arrangement helpers (superuser; the OSS surface for these is the
    //    invitation mailbox rung, which the SMTP-unset suites deliberately run without) ───────────

    /// Flip the registration knob open, with an audit note — the direct-row arrangement twin of the
    /// settings page's `set-registration` ceremony (which claim_e2e exercises for real).
    pub(crate) fn open_registration(&self, note: &str) {
        self.rt
            .block_on(async {
                sqlx::query("UPDATE web.workspace SET registration = 'open' WHERE id = $1")
                    .bind(&self.workspace_id)
                    .execute(&self.pool)
                    .await?;
                sqlx::query(
                    "INSERT INTO web.audit_event (workspace_id, actor_display, kind, outcome, details)
                     VALUES ($1, 'e2e-harness', 'policy_registration', 'ok', jsonb_build_object('note', $2::text))",
                )
                .bind(&self.workspace_id)
                .bind(note)
                .execute(&self.pool)
                .await
            })
            .expect("open the registration knob");
    }

    /// Flip the session-approval knob, with an audit note — the direct-row arrangement twin of
    /// the settings ceremony (member logins born pending until an owner approves).
    pub(crate) fn set_session_approval(&self, on: bool, note: &str) {
        let value = if on { "on" } else { "off" };
        self.rt
            .block_on(async {
                sqlx::query("UPDATE web.workspace SET session_approval = $1 WHERE id = $2")
                    .bind(value)
                    .bind(&self.workspace_id)
                    .execute(&self.pool)
                    .await?;
                sqlx::query(
                    "INSERT INTO web.audit_event (workspace_id, actor_display, kind, outcome, details)
                     VALUES ($1, 'e2e-harness', 'policy_session_approval', 'ok', jsonb_build_object('note', $2::text))",
                )
                .bind(&self.workspace_id)
                .bind(note)
                .execute(&self.pool)
                .await
            })
            .expect("flip the session-approval knob");
    }

    /// Seat an existing account (by email) at `role` — the arrangement step the invitation mailbox
    /// rung performs in a mail-armed deployment (the Playwright mail-sink spec drives that rung for
    /// real; these suites run SMTP-unset by design). Also marks the address verified.
    pub(crate) fn seat(&self, email: &str, role: &str) {
        let workspace_id = self.workspace_id.clone();
        self.seat_in(&workspace_id, email, role);
    }

    /// [`seat`](Self::seat) into a NAMED workspace — for the second-workspace arrangements the
    /// device-link suites drive (the OSS single-tenant product has no second-workspace surface).
    pub(crate) fn seat_in(&self, workspace_id: &str, email: &str, role: &str) {
        self.rt
            .block_on(async {
                sqlx::query("UPDATE web.\"user\" SET email_verified = true WHERE email = $1")
                    .bind(email)
                    .execute(&self.pool)
                    .await?;
                sqlx::query(
                    "INSERT INTO web.seat (workspace_id, user_id, role)
                     SELECT $1, id, $2 FROM web.\"user\" WHERE email = $3
                     ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role",
                )
                .bind(workspace_id)
                .bind(role)
                .bind(email)
                .execute(&self.pool)
                .await?;
                sqlx::query(
                    "INSERT INTO web.audit_event (workspace_id, actor_display, kind, subject, outcome)
                     VALUES ($1, 'e2e-harness', 'seat_arranged', $2, 'ok')",
                )
                .bind(workspace_id)
                .bind(email)
                .execute(&self.pool)
                .await
            })
            .expect("seat the account");
    }

    /// Insert a SECOND workspace directly — claimed (CHECK-valid), with its implicit default
    /// `everyone` channel — and return its row id. The OSS single-tenant product deliberately has
    /// no second-workspace surface; the device-link lane still resolves any workspace by NAME, so
    /// this is the named mail-less arrangement for the cross-workspace and second-link suites.
    pub(crate) fn add_workspace(&self, name: &str, display_name: &str) -> String {
        let id = format!("w_e2e{name:0<28}").replace(' ', "0");
        let channel_id = format!("c_e2e{name:0<28}").replace(' ', "0");
        self.rt
            .block_on(async {
                sqlx::query(
                    "INSERT INTO web.workspace (id, name, display_name, claimed_at)
                     VALUES ($1, $2, $3, now())",
                )
                .bind(&id)
                .bind(name)
                .bind(display_name)
                .execute(&self.pool)
                .await?;
                sqlx::query(
                    "INSERT INTO web.channel (id, workspace_id, name, is_default)
                     VALUES ($1, $2, 'everyone', true)",
                )
                .bind(&channel_id)
                .bind(&id)
                .execute(&self.pool)
                .await?;
                sqlx::query(
                    "INSERT INTO web.audit_event (workspace_id, actor_display, kind, subject, outcome)
                     VALUES ($1, 'e2e-harness', 'workspace_arranged', $2, 'ok')",
                )
                .bind(&id)
                .bind(name)
                .execute(&self.pool)
                .await
            })
            .expect("insert the second workspace");
        id
    }

    /// Sign a fresh member up AND seat them: the one-call arrangement most suites want.
    pub(crate) fn add_member(&self, email: &str, role: &str) -> Session {
        self.open_registration("harness arrangement: mint an extra identity");
        let session = self.sign_up(email);
        self.seat(email, role);
        session
    }

    // ── row-level witnesses ─────────────────────────────────────────────────────────────────────

    /// The `user.id` behind an email (panics if absent — a test-precondition error).
    pub(crate) fn user_id(&self, email: &str) -> String {
        self.rt
            .block_on(
                sqlx::query_scalar::<_, String>("SELECT id FROM web.\"user\" WHERE email = $1")
                    .bind(email)
                    .fetch_one(&self.pool),
            )
            .expect("the account row exists")
    }

    /// One COUNT(*) witness over an arbitrary condition (superuser; read-only).
    pub(crate) fn count(&self, sql: &str) -> i64 {
        self.rt
            .block_on(sqlx::query_scalar::<_, i64>(sql).fetch_one(&self.pool))
            .expect("count witness")
    }

    /// A single-row single-column optional TEXT witness.
    pub(crate) fn text_witness(&self, sql: &str) -> Option<String> {
        self.rt
            .block_on(sqlx::query(sql).fetch_optional(&self.pool))
            .expect("text witness")
            .map(|row| row.get::<String, _>(0))
    }
}

// ── shared bundle expectations ──────────────────────────────────────────────────────────────────────

/// The standard genesis bundle the distribute scenarios publish: a regular doc + an EXECUTABLE script
/// (the exec bit must survive end to end).
pub(crate) fn genesis_files() -> Vec<(&'static str, bool, &'static [u8])> {
    vec![
        (
            "SKILL.md",
            false,
            b"# deploy\nDeploy the service.\n" as &[u8],
        ),
        ("run.sh", true, b"#!/bin/sh\necho deploying\n" as &[u8]),
    ]
}

/// The placement-snapshot shape (`(path, mode & 0o777, bytes)`, sorted) a bundle must materialize to:
/// regular files at 0o644, executable files at 0o755.
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
