//! `topos-plane` — the OSS plane binary. A thin `axum` `main` that opens the storage authority, builds the
//! composed `router(state)`, and serves it. ZERO trust logic here: every decision is the library's (and the
//! authority's). A separate private product imports the LIBRARY and composes it; this bin is the reference
//! self-hostable server.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use topos_plane::{PlaneConfig, PlaneState, SmtpConfig, router, spawn_maintenance};

// The optional built-in ACME TLS serve path — bin-side composition only, behind the default-off `acme`
// feature (the library surface gains nothing; a default build resolves none of its dependencies).
#[cfg(feature = "acme")]
mod acme_serve;

/// The CLI surface: the runtime configuration plus an optional operator subcommand. A bare invocation
/// (`command = None` — the container ENTRYPOINT, every existing flag/env unchanged) serves the plane
/// exactly as before; a subcommand runs an operator task over the same configuration and exits.
#[derive(Debug, Parser)]
#[command(name = "topos-plane", about = "The self-hostable Topos plane (OSS).")]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Option<Command>,
}

/// The operator subcommands (the bare invocation — no subcommand — serves).
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Re-sign every selected skill's `current` pointer one epoch forward (same version, same seq) — the
    /// recovery step after restoring the database from a backup, so every follower's next pull is an
    /// ordinary forward move instead of a reused-generation alarm. Requires an explicit selection
    /// (`--workspace` or `--all-workspaces`); run it with the plane stopped.
    RestoreBumpEpoch {
        /// A workspace to bump (repeatable).
        #[arg(long = "workspace")]
        workspaces: Vec<String>,
        /// Bump every workspace on this plane.
        #[arg(long, conflicts_with = "workspaces")]
        all_workspaces: bool,
        /// A floor for the new epoch (max semantics: `new = max(old + 1, this)`) — for an operator who
        /// restored once before from an even older backup and must jump past every epoch ever served.
        #[arg(long)]
        epoch_at_least: Option<u64>,
    },
    /// Mint a ONE-TIME claim link that stands up a workspace and seats its first owner on redeem — the
    /// operator path for a fresh plane's first workspace (and the hosted break-glass). Prints the link
    /// EXACTLY ONCE to stdout; it is a bearer owner capability, so store it like a secret. On a cloud-mode
    /// plane --owner-email is required; on a self-host plane it is refused (the claiming device roots the
    /// owner).
    MintClaim {
        /// The workspace id to stand up (must not exist yet).
        #[arg(long)]
        workspace: String,
        /// The workspace display name (defaults to the workspace id at redeem).
        #[arg(long)]
        display_name: Option<String>,
        /// The owner email the redeem seats — required on a cloud-mode plane, REFUSED on self-host
        /// (there the claiming device roots the owner as its `dev.…` principal).
        #[arg(long)]
        owner_email: Option<String>,
        /// How long the UNREDEEMED claim stays valid — seconds, or a suffixed duration (`30m`, `72h`, `7d`).
        #[arg(long, default_value = "72h")]
        ttl: String,
    },
}

/// Parse a claim TTL: a bare integer is seconds; `s`/`m`/`h`/`d` suffixes scale it.
fn parse_ttl_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    let (digits, scale) = match s.as_bytes().last() {
        Some(b's') => (&s[..s.len() - 1], 1),
        Some(b'm') => (&s[..s.len() - 1], 60),
        Some(b'h') => (&s[..s.len() - 1], 3600),
        Some(b'd') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("invalid --ttl `{s}` (use seconds or e.g. 30m / 72h / 7d)"))?;
    Ok(n.saturating_mul(scale))
}

/// The self-hostable plane's runtime configuration (flags or env).
#[derive(Debug, clap::Args)]
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
    /// The HUMAN-facing verification base URL, when it differs from the base URL (a hosted plane whose
    /// verification pages live on another host). Defaults to the base URL. Only the device-auth
    /// verification links + the passcode mail link are built on it; `/i/` links stay on the base URL.
    #[arg(long, env = "TOPOS_PLANE_VERIFY_BASE_URL")]
    verify_base_url: Option<String>,
    /// The PUBLIC share-link base the minted `/i/<token>` links ride, when it differs from the base URL
    /// (a hosted plane whose user-visible links live on a web origin that serves or proxies
    /// `GET /i/{token}` back to this plane). Defaults to the base URL. Only the minted link string
    /// moves; the bootstrap payload keeps declaring the API base URL.
    #[arg(long, env = "TOPOS_PLANE_LINK_BASE_URL")]
    link_base_url: Option<String>,
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
    /// The operator admin token (a secret — never logged; only its sha256 is retained). Enables the
    /// `PUT /v1/workspaces/{ws}/policy/review-required` toggle; unset, that route answers 404.
    #[arg(long, env = "TOPOS_PLANE_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// The ACME TLS domain list (repeatable flag; comma-delimited in the env var). NON-EMPTY turns the
    /// EXPERIMENTAL built-in ACME TLS listener on; empty (the default) serves plain HTTP exactly like a
    /// build without the feature (terminate TLS at a reverse proxy — the recommended posture).
    #[cfg(feature = "acme")]
    #[arg(
        long = "acme-domain",
        env = "TOPOS_PLANE_ACME_DOMAINS",
        value_delimiter = ','
    )]
    acme_domain: Vec<String>,
    /// The ACME account contact (e.g. `mailto:ops@example.com`). Required when the ACME listener is on.
    #[cfg(feature = "acme")]
    #[arg(long, env = "TOPOS_PLANE_ACME_CONTACT")]
    acme_contact: Option<String>,
    /// The persistent ACME cache root (the account key + issued certificates; put it on the data volume,
    /// e.g. `/data/acme`, so both survive a container restart). Required when the ACME listener is on.
    #[cfg(feature = "acme")]
    #[arg(long, env = "TOPOS_PLANE_ACME_CACHE")]
    acme_cache: Option<PathBuf>,
    /// The ACME directory URL. Defaults to the Let's Encrypt PRODUCTION directory; point it at the
    /// staging directory (`https://acme-staging-v02.api.letsencrypt.org/directory`) or a local ACME test
    /// server while rehearsing — production imposes strict rate limits.
    #[cfg(feature = "acme")]
    #[arg(
        long,
        env = "TOPOS_PLANE_ACME_DIRECTORY",
        default_value = rustls_acme::acme::LETS_ENCRYPT_PRODUCTION_DIRECTORY
    )]
    acme_directory: String,
    /// The address the ACME TLS listener binds. tls-alpn-01 answers on this same port, so map your
    /// public 443 to it (e.g. the container operator publishes host 443 -> container 8443).
    #[cfg(feature = "acme")]
    #[arg(long, env = "TOPOS_PLANE_ACME_BIND", default_value = "0.0.0.0:8443")]
    acme_bind: SocketAddr,
    /// An extra PEM root added to the ACME CLIENT's trust store — for reaching a private/test ACME
    /// directory over TLS (test directories only; never needed for a public CA).
    #[cfg(feature = "acme")]
    #[arg(long, env = "TOPOS_PLANE_ACME_EXTRA_ROOT")]
    acme_extra_root: Option<PathBuf>,
    /// Seconds between storage-maintenance passes (the recovery sweep + quarantine janitor + a GC pass per
    /// workspace — the reclamation the storage layer mandates but does not schedule; without it, storage
    /// abandoned by rejected/stale proposals grows without bound). The first pass runs at startup. `0`
    /// disables the scheduler (an operator running the passes out-of-band).
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

    let cli = Cli::parse();
    match cli.command {
        None => serve(cli.config).await,
        Some(Command::RestoreBumpEpoch {
            workspaces,
            all_workspaces,
            epoch_at_least,
        }) => restore_bump_epoch(cli.config, workspaces, all_workspaces, epoch_at_least).await,
        Some(Command::MintClaim {
            workspace,
            display_name,
            owner_email,
            ttl,
        }) => mint_claim(cli.config, workspace, display_name, owner_email, &ttl).await,
    }
}

/// The `mint-claim` subcommand: open the SAME plane state serve does (never the listen socket), mint the
/// one-time claim, and print the `/i/` link as the ONLY stdout line (scripts capture stdout; the shown-once
/// warning goes to stderr). The token never enters tracing — the state, the authority, and this fn never
/// log it.
async fn mint_claim(
    cfg: Config,
    workspace: String,
    display_name: Option<String>,
    owner_email: Option<String>,
    ttl: &str,
) -> Result<()> {
    let ttl_secs = parse_ttl_secs(ttl)?;
    let state = open_state(cfg).await?;
    let link = state
        .mint_admin_claim(
            &workspace,
            display_name.as_deref(),
            owner_email.as_deref(),
            ttl_secs,
        )
        .await?;
    eprintln!(
        "This link stands up workspace {workspace} and makes whoever redeems it FIRST its owner.\n\
         It is shown ONCE and never logged — deliver it over a trusted channel and treat it like a secret."
    );
    println!("{link}");
    Ok(())
}

/// The bare invocation: open the plane state, bind, and serve — exactly the pre-subcommand behavior.
/// (With the `acme` feature compiled AND a non-empty `--acme-domain`, the ACME branch serves the same
/// plain listener PLUS a TLS one; off — or not compiled — the path below runs untouched.)
async fn serve(cfg: Config) -> Result<()> {
    #[cfg(feature = "acme")]
    if let Some(acme) = acme_serve::AcmeSettings::from_config(&cfg)? {
        let bind = cfg.bind;
        let state = open_state(cfg).await?;
        return acme_serve::serve_with_tls(bind, acme, state).await;
    }

    let bind = cfg.bind;
    let state = open_state(cfg).await?;

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(addr = %bind, "topos-plane listening");

    // `ConnectInfo<SocketAddr>` is wired so the rate limiter can key on the peer IP when no credential rides.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serving the plane")?;
    Ok(())
}

/// The `restore-bump-epoch` subcommand: open the SAME plane state serve does (the database + the plane key
/// — never the listen socket), run the bump, and print one line per re-signed pointer plus a summary. The
/// printed `key <key_id>` is the operator's tripwire that the restored data directory still holds the
/// pre-incident signing seed. An empty selection result is a success (`re-signed 0 pointer(s)`, exit 0).
async fn restore_bump_epoch(
    cfg: Config,
    workspaces: Vec<String>,
    all_workspaces: bool,
    epoch_at_least: Option<u64>,
) -> Result<()> {
    // The explicit-selection consent gate: an operator must SAY which workspaces get re-signed pointers —
    // there is no default and no prompt.
    if workspaces.is_empty() && !all_workspaces {
        anyhow::bail!(
            "restore-bump-epoch needs an explicit selection: pass --workspace <id> (repeatable) or --all-workspaces"
        );
    }
    let state = open_state(cfg).await?;
    let selection: Option<&[String]> = if all_workspaces {
        None
    } else {
        Some(&workspaces)
    };
    let bumps = state.restore_bump_epochs(selection, epoch_at_least).await?;
    for b in &bumps {
        println!(
            "{}/{}: ({},{}) -> ({},{})  commit {}  key {}",
            b.workspace_id,
            b.skill_id,
            b.old_epoch,
            b.old_seq,
            b.new_epoch,
            b.new_seq,
            &b.commit_hex[..8],
            b.key_id,
        );
    }
    match bumps.first() {
        Some(first) => println!(
            "re-signed {} pointer(s) with key {}",
            bumps.len(),
            first.key_id
        ),
        None => println!("re-signed 0 pointer(s)"),
    }
    Ok(())
}

/// The ONE construction home serve and the operator subcommands share: the two bin-local marshals
/// (default the public base URL to the bind address; assemble the SMTP relay from the five all-or-nothing
/// fields — any missing ⇒ no passcode email, the no-op mailer), then the library's leak-free constructor
/// (the same one a downstream plane uses), which opens the authority, loads/generates the `0600` plane key
/// + enrollment secret, and builds the enrollment config internally. Binds no socket.
async fn open_state(cfg: Config) -> Result<PlaneState> {
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

    let state = PlaneState::open(PlaneConfig {
        database_url: cfg.database_url,
        git_root: cfg.git_root,
        large_root: cfg.large_root,
        plane_key_path: cfg.plane_key,
        enroll_secret_path: cfg.enroll_secret,
        base_url,
        verify_base_url: cfg.verify_base_url,
        link_base_url: cfg.link_base_url,
        mode: cfg.mode,
        enrollment_method: cfg.enrollment_method,
        smtp,
    })
    .await?;
    // The operator admin token (post-construction, like the rate limits): only its sha256 is retained.
    let state = match cfg.admin_token.as_deref() {
        Some(token) if !token.trim().is_empty() => state.with_admin_token(token),
        _ => state,
    };

    // The OIDC connector is feature-gated + default-off; when built in, read its config from the environment
    // and load it onto the state so the `/v1/enroll/oidc/*` routes can drive it.
    #[cfg(feature = "enroll-oidc")]
    let state = match configure_oidc() {
        Some(oidc) => state.with_oidc_config(oidc),
        None => state,
    };

    // The storage-maintenance scheduler — recovery + janitor at startup (the first tick fires at once),
    // then recovery/janitor/per-workspace GC every interval. The LIBRARY owns the pass and the loop
    // (`spawn_maintenance` — the same call a downstream composition makes); the bin only decides to run it.
    // Errors are logged inside the task and never take the server down.
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

    Ok(state)
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
