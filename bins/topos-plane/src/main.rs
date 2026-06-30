//! `topos-plane` — the OSS plane binary. A thin `axum` `main` that opens the storage authority, builds the
//! composed `router(state)`, and serves it. ZERO trust logic here: every decision is the library's (and the
//! authority's). A separate private product imports the LIBRARY and composes it; this bin is the reference
//! self-hostable server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use plane_store::{Authority, DeploymentMode, EnrollmentConfig};
use topos_plane::{EnrollConfig, PlaneState, SmtpConfig, router};

/// The self-hostable plane's runtime configuration (flags or env).
#[derive(Debug, Parser)]
#[command(name = "topos-plane", about = "The self-hostable Topos plane (OSS).")]
struct Config {
    /// The address to bind (host:port).
    #[arg(long, env = "TOPOS_PLANE_BIND", default_value = "127.0.0.1:8787")]
    bind: SocketAddr,
    /// The SQLite database file (created if absent).
    #[arg(long, env = "TOPOS_PLANE_DB")]
    db: PathBuf,
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

    // Resolve the shared enrollment values (posture, base URL, advertised method) once — both the authority's
    // `EnrollmentConfig` (it builds the bootstrap payloads) and the plane's `EnrollConfig` (the routes + the
    // mailer) read them.
    let deployment_mode = DeploymentMode::parse(&cfg.mode).unwrap_or_else(|| {
        tracing::warn!(mode = %cfg.mode, "unknown TOPOS_PLANE_MODE; defaulting to self_host");
        DeploymentMode::SelfHost
    });
    let base_url = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", cfg.bind));
    // All five SMTP fields together enable the relay; any missing ⇒ no passcode email (the no-op mailer).
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
    let enrollment_method = cfg.enrollment_method.clone().unwrap_or_else(|| {
        if smtp.is_some() {
            "passcode"
        } else {
            "device_code"
        }
        .to_owned()
    });

    // Open the storage authority (the only trust surface) + load/generate the plane signing key AND the
    // enrollment HMAC secret (both `0600`, generated on first run). The enrollment config is what mints real
    // credentials — wiring it here is what turns enrollment on for this bin.
    let authority = Authority::open_sqlite(&cfg.db, &cfg.git_root, &cfg.large_root)
        .await
        .context("opening the storage authority")?
        .with_plane_key(&cfg.plane_key)
        .context("loading the plane signing key")?
        .with_enrollment_config(EnrollmentConfig {
            secret_path: cfg.enroll_secret.clone(),
            base_url: base_url.clone(),
            deployment_mode,
            enrollment_method: enrollment_method.clone(),
        })
        .context("loading the enrollment secret")?;

    let state = PlaneState::new(Arc::new(authority)).with_enroll_config(EnrollConfig {
        base_url,
        deployment_mode,
        enrollment_method,
        smtp,
    });

    // The OIDC connector is feature-gated + default-off; when built in, read its config from the environment.
    #[cfg(feature = "enroll-oidc")]
    configure_oidc();

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

/// Read the OIDC connector config from `TOPOS_PLANE_OIDC_*` (a full set enables it) and log that it is wired.
/// The verification routes that DRIVE the connector land next; this only validates + surfaces the config.
#[cfg(feature = "enroll-oidc")]
fn configure_oidc() {
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
        tracing::info!(issuer = %oidc.issuer, "OIDC enrollment connector configured (routes land next)");
    }
}
