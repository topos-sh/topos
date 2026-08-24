//! [`PlaneState`] — the shared handle every handler reads.
//!
//! Cheap to clone (an `Arc<Authority>` + a copied token hash), so axum can hand a copy to each
//! request. The fields are private: a handler reaches the authority through
//! [`PlaneState::authority`], never by destructuring the struct.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use plane_store::{Authority, PoolConfig};

/// The composed vault's shared state: the storage authority + the internal-lane bearer hash.
#[derive(Clone, Debug)]
pub struct PlaneState {
    authority: Arc<Authority>,
    /// The sha256 of the internal-lane bearer token, when one is configured
    /// ([`with_internal_token`](Self::with_internal_token)) — the raw token is never stored.
    /// `None` ⇒ the whole `/internal/v1/*` lane is disabled (every route answers the uniform 404,
    /// so a composition that never sets a token can't accidentally expose an unauthenticated
    /// custody lane).
    internal_token_sha256: Option<[u8; 32]>,
}

/// The leak-free construction config for [`PlaneState::open`]. Every field is plain/owned: **no
/// `plane_store` type crosses it**, so a composer constructs a serving vault without ever naming
/// the authority crate.
#[derive(Debug, Clone)]
pub struct PlaneConfig {
    /// The Postgres connection URL (e.g. `postgres://user:pass@host:5432/db`; append
    /// `?sslmode=require` for a managed / BYO database reached over the network). The schema is
    /// migrated on open.
    pub database_url: String,
    /// Which object store holds the bundle bytes.
    pub store: StoreBackend,
    /// The EPHEMERAL upload-staging root (created if absent; no volume required — losing it on a
    /// container replacement loses only in-flight uploads, which clients replay).
    pub staging_root: PathBuf,
}

/// The object-store backend, as plain config data (the leak-free twin of the authority crate's
/// store config).
#[derive(Debug, Clone)]
pub enum StoreBackend {
    /// A local directory (the self-host default). Reuses the pre-object-store git root: keys are
    /// the exact bare-repo loose shape, so an existing volume's objects are already in place.
    Local { root: PathBuf },
    /// Any S3-compatible endpoint (R2, MinIO, AWS). `region` is `"auto"` for R2.
    S3 {
        endpoint: String,
        bucket: String,
        access_key_id: String,
        secret_access_key: String,
        region: String,
    },
}

impl StoreBackend {
    /// The authority crate's config twin (crate-internal — the leak-free boundary holds).
    pub(crate) fn to_store_config(&self) -> plane_store::StoreConfig {
        match self {
            StoreBackend::Local { root } => plane_store::StoreConfig::Local { root: root.clone() },
            StoreBackend::S3 {
                endpoint,
                bucket,
                access_key_id,
                secret_access_key,
                region,
            } => plane_store::StoreConfig::S3 {
                endpoint: endpoint.clone(),
                bucket: bucket.clone(),
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                region: region.clone(),
            },
        }
    }
}

/// The Postgres pool tuning, read from the environment (the one place the vault reads
/// `TOPOS_PLANE_DB_*`). Unset knobs keep the driver defaults (`max_connections = 10`,
/// `acquire_timeout = 30s`). The statement/lock ceilings stay off unless the operator opts in (so a
/// long legitimate whole-bundle render is never capped); the idle-in-transaction timeout defaults
/// to a safe 30s (every write txn is pure-DB and short, so it only ever trips an abandoned/stuck
/// one that would otherwise pin row locks — set the env to `0` to disable it).
fn pool_config_from_env() -> PoolConfig {
    fn secs(var: &str) -> Option<Duration> {
        std::env::var(var)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    }
    PoolConfig {
        max_connections: std::env::var("TOPOS_PLANE_DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok()),
        acquire_timeout: secs("TOPOS_PLANE_DB_ACQUIRE_TIMEOUT_SECS"),
        statement_timeout: secs("TOPOS_PLANE_DB_STATEMENT_TIMEOUT_SECS"),
        lock_timeout: secs("TOPOS_PLANE_DB_LOCK_TIMEOUT_SECS"),
        idle_in_transaction_timeout: Some(
            secs("TOPOS_PLANE_DB_IDLE_IN_TX_TIMEOUT_SECS").unwrap_or(Duration::from_secs(30)),
        ),
    }
}

/// Resolve + SECURE the ephemeral staging root: the dir is created `0700` if absent; a
/// pre-existing one must be a real directory (never a symlink) owned by the serving euid, and is
/// re-tightened to `0700`. This is what lets the default live under a shared temp dir without a
/// predictable-path squat: a foreign-owned or linked entry at the path is a typed refusal naming
/// the cure, never adopted.
///
/// # Errors
/// An [`anyhow::Error`] naming the offending path and the property it failed.
pub fn secure_staging_dir(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .or_else(|e| {
                    // A racer (another instance booting) may have created it; re-verify below.
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .with_context(|| format!("creating the staging dir {}", path.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("probing {}", path.display())),
        Ok(_) => {}
    }
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("probing {}", path.display()))?;
    anyhow::ensure!(
        meta.file_type().is_dir(),
        "the staging path {} is not a plain directory (a symlink or file is squatting it) — remove it or point TOPOS_PLANE_TMP elsewhere",
        path.display()
    );
    let euid: u32 = rustix::process::geteuid().as_raw();
    anyhow::ensure!(
        meta.uid() == euid,
        "the staging dir {} is owned by uid {} but the vault runs as uid {euid} — remove it or point TOPOS_PLANE_TMP at a directory the service owns",
        path.display(),
        meta.uid()
    );
    // Tighten to owner-only; staged candidate bytes are nobody else's to read.
    let mut perms = meta.permissions();
    if perms.mode() & 0o077 != 0 {
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("tightening {}", path.display()))?;
    }
    Ok(path.to_path_buf())
}

impl PlaneState {
    /// Construct from an already-built [`Authority`]. This names the `plane_store` [`Authority`] in
    /// its signature — it is the explicit test / advanced construction path; a composer builds
    /// through the leak-free [`open`](Self::open) ([`PlaneConfig`]) instead.
    #[must_use]
    pub fn new(authority: Arc<Authority>) -> Self {
        Self {
            authority,
            internal_token_sha256: None,
        }
    }

    /// Open a serving [`PlaneState`] over Postgres from a leak-free [`PlaneConfig`] — the
    /// **single** construction path the OSS bin (and any composition) uses. Builds the storage
    /// [`Authority`] (the db + the object store + the ephemeral staging root) internally, so the
    /// caller never names a `plane_store` type.
    ///
    /// # Errors
    /// Returns an [`anyhow::Error`] if the store cannot be opened, the staging root cannot be
    /// created, or the database cannot be opened or migrated.
    pub async fn open(cfg: PlaneConfig) -> anyhow::Result<PlaneState> {
        let authority = Authority::open_with_pool(
            &cfg.database_url,
            &cfg.store.to_store_config(),
            &cfg.staging_root,
            pool_config_from_env(),
        )
        .await
        .context("opening the storage authority")?;
        Ok(PlaneState::new(Arc::new(authority)))
    }

    /// Arm the internal custody lane (`/internal/v1/*`) by configuring its bearer token. Only the
    /// token's sha256 is retained (never the raw secret — it can't reach a `Debug`/log); with no
    /// token configured the lane stays disabled and every route answers the uniform 404. The OSS
    /// bin wires this from `TOPOS_PLANE_INTERNAL_TOKEN`.
    #[must_use]
    pub fn with_internal_token(mut self, token: &str) -> Self {
        self.internal_token_sha256 = Some(topos_core::digest::sha256(token.as_bytes()));
        self
    }

    /// Whether an internal-lane token is configured (every `/internal/v1/*` route is 404-invisible
    /// otherwise).
    pub(crate) fn internal_token_configured(&self) -> bool {
        self.internal_token_sha256.is_some()
    }

    /// Whether `provided` is the configured internal-lane token — a fixed 32-byte sha256 compare
    /// (timing-independent of any prefix match). `false` when no token is configured.
    pub(crate) fn internal_token_matches(&self, provided: &str) -> bool {
        self.internal_token_sha256
            .is_some_and(|stored| topos_core::digest::sha256(provided.as_bytes()) == stored)
    }

    /// The storage authority — the only trust surface; handlers call its custody operations.
    pub(crate) fn authority(&self) -> &Authority {
        &self.authority
    }
}
