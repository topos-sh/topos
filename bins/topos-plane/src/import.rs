//! The `import-local` subcommand — the one-shot cutover from the pre-object-store on-disk layout
//! into the configured object store. Run it with the vault STOPPED; it is idempotent (a rerun
//! resumes) and ends with a per-workspace count table + a final PASS/FAIL on stdout.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use clap::Parser;

use crate::state::StoreBackend;

/// The cutover's configuration (flags or env — the same store env the serve command reads).
#[derive(Debug, Parser)]
#[command(
    name = "topos-plane import-local",
    about = "One-shot migration of a pre-object-store layout (bare repos + large-object files) \
             into the configured object store. Run with the vault STOPPED."
)]
struct ImportArgs {
    /// The Postgres connection URL. The schema is migrated first (the import's columns arrive
    /// with it).
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// The OLD layout's per-workspace git-store root (`TOPOS_PLANE_GIT_ROOT` — the same volume
    /// the pre-import vault served from).
    #[arg(long, env = "TOPOS_PLANE_GIT_ROOT")]
    git_root: PathBuf,
    /// The OLD layout's large-object root (`TOPOS_PLANE_LARGE_ROOT`).
    #[arg(long, env = "TOPOS_PLANE_LARGE_ROOT")]
    large_root: PathBuf,
    /// The TARGET store: `local` (default; the git root itself, making the byte copy a no-op) or
    /// `s3` (TOPOS_PLANE_S3_*).
    #[arg(long, env = "TOPOS_PLANE_STORE", default_value = "local")]
    store: ImportStoreKind,
    /// The local store root when `--store local` names a DIFFERENT directory than the old git
    /// root (an expand-to-a-new-volume cutover). Defaults to the old git root (in place).
    #[arg(long, env = "TOPOS_PLANE_LOCAL_STORE_ROOT")]
    local_store_root: Option<PathBuf>,
    /// The S3-compatible endpoint URL.
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
}

/// The closed store-backend vocabulary of the import's `--store`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ImportStoreKind {
    Local,
    S3,
}

/// Run the import; the process's exit code is the PASS/FAIL verdict.
///
/// # Errors
/// An [`anyhow::Error`] on a fault or a FAILED verification (main maps it to a nonzero exit).
pub async fn import_local_main(argv: Vec<OsString>) -> Result<()> {
    let args = ImportArgs::parse_from(argv);
    let backend = match args.store {
        ImportStoreKind::Local => StoreBackend::Local {
            root: args
                .local_store_root
                .clone()
                .unwrap_or_else(|| args.git_root.clone()),
        },
        ImportStoreKind::S3 => {
            let missing = |what: &str| format!("--store s3 requires {what} (TOPOS_PLANE_S3_*)");
            StoreBackend::S3 {
                endpoint: args
                    .s3_endpoint
                    .clone()
                    .with_context(|| missing("an endpoint"))?,
                bucket: args
                    .s3_bucket
                    .clone()
                    .with_context(|| missing("a bucket"))?,
                access_key_id: args
                    .s3_access_key_id
                    .clone()
                    .with_context(|| missing("an access key id"))?,
                secret_access_key: args
                    .s3_secret_access_key
                    .clone()
                    .with_context(|| missing("a secret access key"))?,
                region: args.s3_region.clone(),
            }
        }
    };

    // The staging root is unused by the import; it still rides the same secured resolution as
    // serve (0700, owned, never a squattable predictable name).
    let staging = crate::state::secure_staging_dir(&std::env::temp_dir().join(format!(
        "topos-plane-import-{}",
        rustix::process::geteuid().as_raw()
    )))?;
    let authority =
        plane_store::Authority::open(&args.database_url, &backend.to_store_config(), &staging)
            .await
            .context("opening the authority (database + store)")?;

    println!(
        "import-local: {} + {} -> {:?} store",
        args.git_root.display(),
        args.large_root.display(),
        args.store,
    );
    let report = authority
        .import_local(&args.git_root, &args.large_root)
        .await
        .context("running the import")?;

    for ws in &report.workspaces {
        println!(
            "  {}: {} loose copied, {} large converted ({} orphan skipped), {} versions backfilled",
            ws.workspace_id,
            ws.loose_copied,
            ws.large_converted,
            ws.large_orphans,
            ws.versions_backfilled,
        );
    }
    for failure in &report.failures {
        println!("  FAILURE: {failure}");
    }
    if report.passed() {
        println!(
            "import-local: PASS ({} workspace(s))",
            report.workspaces.len()
        );
        Ok(())
    } else {
        println!(
            "import-local: FAIL ({} failure(s) across {} workspace(s))",
            report.failures.len(),
            report.workspaces.len()
        );
        bail!("import-local verification FAILED — nothing to do but fix the named items and rerun")
    }
}
