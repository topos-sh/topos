//! `topos-plane` — the OSS plane binary. A thin `axum` `main` that opens the storage authority, builds the
//! composed `router(state)`, and serves it. ZERO trust logic here: every decision is the library's (and the
//! authority's). A separate private product imports the LIBRARY and composes it; this bin is the reference
//! self-hostable server.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use topos_plane::{PlaneConfig, PlaneState, SmtpConfig, router};

/// The self-hostable plane's runtime configuration (flags or env).
#[derive(Debug, Parser)]
#[command(name = "topos-plane", about = "The self-hostable Topos plane (OSS).")]
struct Config {
    /// The address to bind (host:port).
    #[arg(long, env = "TOPOS_PLANE_BIND", default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    /// The Postgres connection URL (e.g. `postgres://user:pass@host:5432/db`; append `?sslmode=require`
    /// for a managed / BYO database over the network). The schema is migrated on startup.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// The per-workspace git-object store root (created if absent).
    #[arg(long, env = "TOPOS_PLANE_GIT_ROOT")]
    git_root: PathBuf,
    /// The per-workspace large-object store root (created if absent).
    #[arg(long, env = "TOPOS_PLANE_LARGE_ROOT")]
    large_root: PathBuf,
    /// The plane signing key (a `0600` seed; generated on first run if absent).
    #[arg(long, env = "TOPOS_PLANE_KEY")]
    plane_key: PathBuf,
    /// The enrollment HMAC secret (a `0600` seed; generated on first run if absent) — the root every opaque
    /// invite / grant / read-token is derived from.
    #[arg(long, env = "TOPOS_PLANE_ENROLL_SECRET")]
    enroll_secret: PathBuf,
    /// The plane's PUBLIC base URL (the invite + verification links are built on it). Defaults to
    /// `http://<bind>` (fine for a single-box self-host; set it explicitly behind a reverse proxy).
    #[arg(long, env = "TOPOS_PLANE_BASE_URL")]
    base_url: Option<String>,
    /// The deployment posture — `cloud` or `self_host` (default `self_host`).
    #[arg(long, env = "TOPOS_PLANE_MODE", default_value = "self_host")]
    mode: String,
    /// The enrollment method advertised in the bootstrap. Defaults to `passcode` when SMTP is configured,
    /// else `device_code`.
    #[arg(long, env = "TOPOS_PLANE_ENROLLMENT_METHOD")]
    enrollment_method: Option<String>,
    /// The SMTP relay host (passcode email; all five `--smtp-*` must be set to enable it).
    #[arg(long, env = "TOPOS_PLANE_SMTP_HOST")]
    smtp_host: Option<String>,
    /// The SMTP relay port.
    #[arg(long, env = "TOPOS_PLANE_SMTP_PORT")]
    smtp_port: Option<u16>,
    /// The SMTP username (a credential — never logged).
    #[arg(long, env = "TOPOS_PLANE_SMTP_USER")]
    smtp_user: Option<String>,
    /// The SMTP password (a credential — never logged).
    #[arg(long, env = "TOPOS_PLANE_SMTP_PASS")]
    smtp_pass: Option<String>,
    /// The SMTP from-address.
    #[arg(long, env = "TOPOS_PLANE_SMTP_FROM")]
    smtp_from: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // JSON logs to stderr, filtered by `RUST_LOG` (defaulting to `info`). Diagnostics never touch stdout.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::parse();

    // The two bin-local marshals `open` does NOT do: default the public base URL to the bind address,
    // and assemble the SMTP relay from the five all-or-nothing fields (any missing ⇒ no passcode email, the
    // no-op mailer). Everything else — parsing the mode, defaulting the enrollment method, opening the
    // authority + enrollment config — is the constructor's, so there is one construction home and no drift.
    let base_url = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", cfg.bind));
    let smtp = match (
        cfg.smtp_host.clone(),
        cfg.smtp_port,
        cfg.smtp_user.clone(),
        cfg.smtp_pass.clone(),
        cfg.smtp_from.clone(),
    ) {
        (Some(host), Some(port), Some(user), Some(pass), Some(from)) => Some(SmtpConfig {
            host,
            port,
            user,
            pass,
            from,
        }),
        _ => None,
    };

    // The single construction path (dogfooding the library's leak-free constructor — the same one a downstream
    // plane uses): build the serving state from a `PlaneConfig`. It opens the authority, loads/generates the
    // `0600` plane key + enrollment secret, and builds the enrollment config internally.
    let state = PlaneState::open(PlaneConfig {
        database_url: cfg.database_url,
        git_root: cfg.git_root,
        large_root: cfg.large_root,
        plane_key_path: cfg.plane_key,
        enroll_secret_path: cfg.enroll_secret,
        base_url,
        mode: cfg.mode,
        enrollment_method: cfg.enrollment_method,
        smtp,
    })
    .await?;

    // The OIDC connector is feature-gated + default-off; when built in, read its config from the environment
    // and load it onto the state so the `/v1/enroll/oidc/*` routes can drive it.
    #[cfg(feature = "enroll-oidc")]
    let state = match configure_oidc() {
        Some(oidc) => state.with_oidc_config(oidc),
        None => state,
    };

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    tracing::info!(addr = %cfg.bind, "topos-plane listening");

    // `ConnectInfo<SocketAddr>` is wired so the rate limiter can key on the peer IP when no credential rides.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serving the plane")?;
    Ok(())
}

/// Read the OIDC connector config from `TOPOS_PLANE_OIDC_*` (a full set enables it). Returns the config to
/// load onto [`PlaneState`] (the `/v1/enroll/oidc/*` routes drive it); `None` when the env set is incomplete.
#[cfg(feature = "enroll-oidc")]
fn configure_oidc() -> Option<topos_plane::OidcConfig> {
    let vars = (
        std::env::var("TOPOS_PLANE_OIDC_ISSUER").ok(),
        std::env::var("TOPOS_PLANE_OIDC_CLIENT_ID").ok(),
        std::env::var("TOPOS_PLANE_OIDC_CLIENT_SECRET").ok(),
        std::env::var("TOPOS_PLANE_OIDC_REDIRECT_URI").ok(),
    );
    if let (Some(issuer), Some(client_id), Some(client_secret), Some(redirect_uri)) = vars {
        let oidc = topos_plane::OidcConfig {
            issuer,
            client_id,
            client_secret,
            redirect_uri,
        };
        tracing::info!(issuer = %oidc.issuer, "OIDC enrollment connector configured");
        Some(oidc)
    } else {
        None
    }
}
