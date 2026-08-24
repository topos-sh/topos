//! `topos-plane` — the OSS vault binary. A thin `axum` `main` that opens the storage authority,
//! builds the composed `router(state)`, and serves it. ZERO trust logic here: every decision is the
//! library's (and the authority's). The vault is internal-network-only — never publish its port;
//! the product app is its one caller, authenticated by the internal bearer token.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use topos_plane::{PlaneConfig, PlaneState, StoreBackend, router, spawn_maintenance};

/// The serving configuration (flags or env) — the default command.
#[derive(Debug, Parser)]
#[command(
    name = "topos-plane",
    about = "The Topos vault (OSS) — pure byte custody. Bare invocation (or `serve`) serves; \
             `import-local` is the one-shot cutover from the pre-object-store on-disk layout."
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
    /// Which object store holds the bundle bytes: `local` (a directory — the self-host default)
    /// or `s3` (any S3-compatible endpoint: R2, MinIO, AWS — configured via TOPOS_PLANE_S3_*).
    #[arg(long, env = "TOPOS_PLANE_STORE", default_value = "local")]
    store: StoreKind,
    /// The local store root (created if absent) — required for `--store local`, ignored for `s3`.
    /// Reuses the pre-object-store git root: an existing volume's loose objects are already at
    /// their keys.
    #[arg(long, env = "TOPOS_PLANE_GIT_ROOT")]
    git_root: Option<PathBuf>,
    /// The S3-compatible endpoint URL (e.g. `https://<account>.r2.cloudflarestorage.com`).
    #[arg(long, env = "TOPOS_PLANE_S3_ENDPOINT")]
    s3_endpoint: Option<String>,
    /// The bucket name.
    #[arg(long, env = "TOPOS_PLANE_S3_BUCKET")]
    s3_bucket: Option<String>,
    /// The access key id.
    #[arg(long, env = "TOPOS_PLANE_S3_ACCESS_KEY_ID")]
    s3_access_key_id: Option<String>,
    /// The secret access key (a secret — never logged).
    #[arg(long, env = "TOPOS_PLANE_S3_SECRET_ACCESS_KEY", hide_env_values = true)]
    s3_secret_access_key: Option<String>,
    /// The region (`auto` for R2).
    #[arg(long, env = "TOPOS_PLANE_S3_REGION", default_value = "auto")]
    s3_region: String,
    /// The EPHEMERAL upload-staging directory (no volume required; losing it on a container
    /// replacement loses only in-flight uploads, which clients replay). Default: `.staging`
    /// under the local store root, or a service-owned per-uid temp dir on the s3 backend. The
    /// dir is created 0700; a pre-existing symlink or foreign-owned entry at the path refuses.
    #[arg(long, env = "TOPOS_PLANE_TMP")]
    tmp: Option<PathBuf>,
    /// The internal-lane bearer token (a secret — never logged; only its sha256 is retained). Arms
    /// the `/internal/v1/*` custody lane; unset, every route on that lane answers 404.
    #[arg(long, env = "TOPOS_PLANE_INTERNAL_TOKEN", hide_env_values = true)]
    internal_token: Option<String>,
    /// Seconds between storage-maintenance passes (the recovery sweep + staging janitor + a GC
    /// pass per workspace — the reclamation the storage layer mandates but does not schedule). The
    /// first pass runs at startup. `0` disables the scheduler (an operator running the passes
    /// out-of-band).
    #[arg(long, env = "TOPOS_PLANE_GC_INTERVAL_SECS", default_value_t = 300)]
    gc_interval_secs: u64,
}

/// The closed store-backend vocabulary of `--store`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum StoreKind {
    Local,
    S3,
}

/// Resolve the serving config's store backend, failing typed on a missing required field.
fn store_backend(cfg: &Config) -> Result<StoreBackend> {
    match cfg.store {
        StoreKind::Local => {
            let Some(root) = cfg.git_root.clone() else {
                bail!("--store local requires --git-root (or TOPOS_PLANE_GIT_ROOT)");
            };
            Ok(StoreBackend::Local { root })
        }
        StoreKind::S3 => {
            let missing = |what: &str| format!("--store s3 requires {what} (TOPOS_PLANE_S3_*)");
            Ok(StoreBackend::S3 {
                endpoint: cfg
                    .s3_endpoint
                    .clone()
                    .with_context(|| missing("an endpoint"))?,
                bucket: cfg.s3_bucket.clone().with_context(|| missing("a bucket"))?,
                access_key_id: cfg
                    .s3_access_key_id
                    .clone()
                    .with_context(|| missing("an access key id"))?,
                secret_access_key: cfg
                    .s3_secret_access_key
                    .clone()
                    .with_context(|| missing("a secret access key"))?,
                region: cfg.s3_region.clone(),
            })
        }
    }
}

/// The default ephemeral staging root when `TOPOS_PLANE_TMP` is unset: `.staging` under the
/// local store root when one exists (service-owned by construction; the dot prefix is reserved —
/// no workspace id may start with one), else a per-euid dir under the system temp dir (never a
/// bare predictable name a squatter could pre-create — and `secure_staging_dir` refuses one that
/// was).
fn default_tmp(backend: &StoreBackend) -> PathBuf {
    match backend {
        StoreBackend::Local { root } => root.join(".staging"),
        StoreBackend::S3 { .. } => std::env::temp_dir().join(format!(
            "topos-plane-{}",
            rustix::process::geteuid().as_raw()
        )),
    }
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

    // Two commands, dispatched by the first argument: `import-local` (the one-shot cutover) and
    // `serve` (the default — a bare `topos-plane`, flags and env exactly as before, keeps every
    // existing deployment's invocation working; an explicit leading `serve` is accepted too).
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match argv.get(1).and_then(|a| a.to_str()) {
        Some("import-local") => {
            argv.remove(1);
            return topos_plane::import_local_main(argv).await;
        }
        Some("serve") => {
            argv.remove(1);
        }
        _ => {}
    }

    let cfg = Config::parse_from(argv);
    let backend = store_backend(&cfg)?;
    let staging_root =
        topos_plane::secure_staging_dir(&cfg.tmp.clone().unwrap_or_else(|| default_tmp(&backend)))?;
    let state = PlaneState::open(PlaneConfig {
        database_url: cfg.database_url.clone(),
        store: backend,
        staging_root,
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
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving the vault")?;
    tracing::info!("topos-plane stopped");
    Ok(())
}

/// Resolves when the process is asked to stop, so `axum` can finish in-flight requests and return
/// instead of being killed mid-response.
///
/// This matters more than it looks: a container runtime stops a container by sending SIGTERM and
/// then waiting out a timeout before SIGKILL. The default disposition of SIGTERM terminates a
/// process — but only for a process that is NOT PID 1, and this binary IS PID 1 in its image
/// (`ENTRYPOINT ["topos-plane"]`, no init shim). PID 1 ignores every signal it has no handler
/// for, so without this function the vault sits deaf through the entire stop timeout and is
/// SIGKILLed — a fixed, pointless stall on every single restart, and an in-flight request dies
/// with it. Installing the handler is what makes the process actually stoppable.
async fn shutdown_signal() {
    // Ctrl-C for an operator running this in a terminal; SIGTERM for every orchestrator.
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installing the Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing the SIGTERM handler")
            .recv()
            .await;
    };
    // Non-unix hosts have no SIGTERM; Ctrl-C alone decides there.
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("interrupt received; draining in-flight requests"),
        () = terminate => tracing::info!("SIGTERM received; draining in-flight requests"),
    }
}
