//! `topos-plane` — the OSS vault binary. A thin `axum` `main` that opens the storage authority,
//! builds the composed `router(state)`, and serves it. ZERO trust logic here: every decision is the
//! library's (and the authority's). The vault is internal-network-only — never publish its port;
//! the product app is its one caller, authenticated by the internal bearer token.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use topos_plane::{PlaneConfig, PlaneState, router, spawn_maintenance};

/// The vault's runtime configuration (flags or env).
#[derive(Debug, Parser)]
#[command(
    name = "topos-plane",
    about = "The Topos vault (OSS) — pure byte custody."
)]
struct Config {
    /// The address to bind (host:port). Bind an INTERNAL interface — the vault must never be
    /// publicly reachable; the product app is its one caller.
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
    /// The internal-lane bearer token (a secret — never logged; only its sha256 is retained). Arms
    /// the `/internal/v1/*` custody lane; unset, every route on that lane answers 404.
    #[arg(long, env = "TOPOS_PLANE_INTERNAL_TOKEN", hide_env_values = true)]
    internal_token: Option<String>,
    /// Seconds between storage-maintenance passes (the recovery sweep + quarantine janitor + a GC
    /// pass per workspace — the reclamation the storage layer mandates but does not schedule). The
    /// first pass runs at startup. `0` disables the scheduler (an operator running the passes
    /// out-of-band).
    #[arg(long, env = "TOPOS_PLANE_GC_INTERVAL_SECS", default_value_t = 300)]
    gc_interval_secs: u64,
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
    let state = PlaneState::open(PlaneConfig {
        database_url: cfg.database_url,
        git_root: cfg.git_root,
        large_root: cfg.large_root,
    })
    .await?;
    // The internal-lane token (post-construction): only its sha256 is retained; unset (or blank),
    // the whole `/internal/v1/*` lane stays 404-invisible.
    let state = match cfg.internal_token.as_deref() {
        Some(token) if !token.trim().is_empty() => state.with_internal_token(token),
        _ => {
            tracing::warn!(
                "no TOPOS_PLANE_INTERNAL_TOKEN configured; the custody lane answers 404 until one is set"
            );
            state
        }
    };

    // The storage-maintenance scheduler — recovery + janitor at startup (the first tick fires at
    // once), then recovery/janitor/per-workspace GC every interval. The LIBRARY owns the pass and
    // the loop; the bin only decides to run it. Errors are logged inside the task and never take
    // the server down.
    if cfg.gc_interval_secs > 0 {
        spawn_maintenance(
            state.clone(),
            std::time::Duration::from_secs(cfg.gc_interval_secs),
        );
    } else {
        tracing::warn!(
            "storage maintenance disabled (TOPOS_PLANE_GC_INTERVAL_SECS=0); run the GC passes out-of-band"
        );
    }

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    tracing::info!(addr = %cfg.bind, "topos-plane listening");
    axum::serve(listener, router(state))
        .await
        .context("serving the vault")?;
    Ok(())
}
