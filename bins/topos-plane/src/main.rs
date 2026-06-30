//! `topos-plane` — the OSS plane binary. A thin `axum` `main` that opens the storage authority, builds the
//! composed `router(state)`, and serves it. ZERO trust logic here: every decision is the library's (and the
//! authority's). A separate private product imports the LIBRARY and composes it; this bin is the reference
//! self-hostable server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use plane_store::Authority;
use topos_plane::{PlaneState, router};

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

    // Open the storage authority (the only trust surface) + load/generate the plane signing key.
    let authority = Authority::open_sqlite(&cfg.db, &cfg.git_root, &cfg.large_root)
        .await
        .context("opening the storage authority")?
        .with_plane_key(&cfg.plane_key)
        .context("loading the plane signing key")?;

    let state = PlaneState::new(Arc::new(authority));
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
