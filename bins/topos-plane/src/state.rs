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
    /// The per-workspace git-object store root (created if absent).
    pub git_root: PathBuf,
    /// The per-workspace large-object store root (created if absent).
    pub large_root: PathBuf,
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
    /// [`Authority`] (the db + git + large stores) internally, so the caller never names a
    /// `plane_store` type.
    ///
    /// # Errors
    /// Returns an [`anyhow::Error`] if a store root cannot be created or the database cannot be
    /// opened or migrated.
    pub async fn open(cfg: PlaneConfig) -> anyhow::Result<PlaneState> {
        let authority = Authority::open_with_pool(
            &cfg.database_url,
            &cfg.git_root,
            &cfg.large_root,
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
