//! [`PlaneState`] — the shared handle every handler and the rate-limit middleware read.
//!
//! Cheap to clone (an `Arc<Authority>`, the `Arc`-backed limiter, and an `Arc<EnrollConfig>`), so axum can
//! hand a copy to each request. The fields are private: a handler reaches the authority through
//! [`PlaneState::authority`], the limiter through [`PlaneState::limiter`], and the enrollment config through
//! [`PlaneState::enroll`] — never by destructuring the struct.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use plane_store::{Authority, DeploymentMode, EnrollmentConfig, PoolConfig, WorkspaceId};

use topos_types::requests::WireSkillIndex;

use crate::rate_limit::{Limiter, Limits};
use crate::wire::error::PlaneHttpError;

/// The composed plane's shared state: the storage authority + the in-process rate limiter + the static
/// enrollment config. One value, cloned per request (every field is `Arc`-backed, so a clone is a handful
/// of pointer bumps).
#[derive(Clone, Debug)]
pub struct PlaneState {
    authority: Arc<Authority>,
    limiter: Limiter,
    enroll: Arc<EnrollConfig>,
    /// The OIDC connector config — only present under `enroll-oidc` (default-off), set by the bin from the
    /// environment via [`with_oidc_config`](Self::with_oidc_config); `None` until configured.
    #[cfg(feature = "enroll-oidc")]
    oidc: Option<Arc<crate::enroll::oidc::OidcConfig>>,
    /// The sha256 of the self-host operator's admin token, when one is configured
    /// ([`with_admin_token`](Self::with_admin_token)) — the raw token is never stored. `None` ⇒ the
    /// admin-authenticated policy route is disabled (it answers 404, so a composition that never sets a
    /// token can't accidentally expose an unauthenticated toggle).
    admin_token_sha256: Option<[u8; 32]>,
    /// The sha256 of the internal-session-lane token, when one is configured
    /// ([`with_internal_token`](Self::with_internal_token)) — the raw token is never stored. `None` ⇒ the
    /// internal session lane (`/internal/v1/*`) is disabled (every route answers 404, so a composition that
    /// never sets a token can't accidentally expose an unauthenticated internal lane).
    internal_token_sha256: Option<[u8; 32]>,
}

/// The static enrollment configuration the verification routes read: the public base URL, the deployment
/// posture, and the offered enrollment method. (Passcode DELIVERY is the composing surface's since the mail
/// unification: the internal lane mints the code once and the surface's mail seam carries it — the vault
/// holds no mail transport.)
///
/// **Crate-private** — it names a `plane_store` type (`deployment_mode`), so it is built **internally** by
/// [`PlaneState::open`] from the leak-free [`PlaneConfig`]; a downstream plane never constructs it.
#[derive(Debug, Clone)]
pub(crate) struct EnrollConfig {
    /// The plane's public API base URL (what a client dials; the bootstrap payload declares it). The
    /// **authority** holds the authoritative copy every disclosure serves (the `/i/` bootstrap + the
    /// standup plane block both read it there); this is the construction record, asserted by tests —
    /// mirroring `deployment_mode` below.
    #[cfg_attr(not(test), allow(dead_code))]
    pub base_url: String,
    /// The HUMAN-facing verification base (already resolved: `verify_base_url` else `base_url`). The
    /// **authority** builds the device-auth `verification_uri`(+`_complete`) from its own copy of the same
    /// value; this is the construction record, asserted by tests — mirroring `base_url` above.
    #[cfg_attr(not(test), allow(dead_code))]
    pub verify_base_url: String,
    /// The PUBLIC share-link base (already resolved: `link_base_url` else `base_url`). The
    /// **authority** holds the authoritative copy every consumer reads (the link composers + the
    /// agent-readable bootstrap document go through `Authority::enrollment_disclosure` / the domain
    /// bootstrap's `link_base`); this is the construction record, asserted by tests — mirroring
    /// `base_url` above.
    #[cfg_attr(not(test), allow(dead_code))]
    pub link_base_url: String,
    /// The deployment posture parsed STRICTLY (`None` ⇒ the configured mode string was unrecognized and
    /// `deployment_mode` below is the warn-fallback). The standup/create/mint wrappers REFUSE to run off a
    /// fallback — they fail closed on `None` instead of inheriting it.
    pub strict_deployment_mode: Option<DeploymentMode>,
    /// This plane's deployment posture. The **authority** holds the authoritative copy the bootstrap serves;
    /// this is the construction record (built by [`PlaneState::open`], asserted by tests). Production
    /// reads only `strict_deployment_mode` from here — hence `allow(dead_code)` off-test (mirrors the
    /// [`enroll`] accessor idiom), while parity with the original construction is preserved.
    #[cfg_attr(not(test), allow(dead_code))]
    pub deployment_mode: DeploymentMode,
    /// The enrollment method advertised to a bootstrapping device (e.g. `"device_code"`). The authority's copy
    /// is authoritative; see `deployment_mode`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub enrollment_method: String,
}

/// The leak-free construction config for [`PlaneState::open`] — the one a downstream plane (or the OSS
/// bin) fills in. Every field is plain/owned or `topos-plane`-owned: **no `plane_store` type crosses it** (the
/// deployment posture is a `String`, parsed internally), so a composer constructs a serving plane without ever
/// naming the authority crate.
#[derive(Debug, Clone)]
pub struct PlaneConfig {
    /// The Postgres connection URL (e.g. `postgres://user:pass@host:5432/db`; append `?sslmode=require`
    /// for a managed / BYO database reached over the network). The schema is migrated on open.
    pub database_url: String,
    /// The per-workspace git-object store root (created if absent).
    pub git_root: PathBuf,
    /// The per-workspace large-object store root (created if absent).
    pub large_root: PathBuf,
    /// The enrollment HMAC secret (a `0600` seed; generated on first run if absent) — the root every opaque
    /// credential is derived from.
    pub enroll_secret_path: PathBuf,
    /// The plane's public base URL (the invite + verification links are built on it). Already resolved — the
    /// OSS bin defaults it to `http://<bind>`.
    pub base_url: String,
    /// The HUMAN-facing verification base URL, when it differs from `base_url` (a hosted plane whose
    /// verification pages live on another host). `None` ⇒ `base_url`. Only the device-auth
    /// `verification_uri`(+`_complete`) and the passcode mail link are built on it.
    pub verify_base_url: Option<String>,
    /// The PUBLIC share-link base the minted `/i/<token>` links ride, when it differs from `base_url`
    /// (a hosted plane whose user-visible links live on the web origin, which must then serve or proxy
    /// `GET /i/{token}` back to this plane). `None` ⇒ `base_url`. Only the minted link STRING moves —
    /// the bootstrap payload keeps declaring the API `base_url` and clients re-root onto it.
    pub link_base_url: Option<String>,
    /// The deployment posture — `"self_host"` or `"cloud"`. Parsed internally (an unknown value warns and
    /// falls back to `self_host`), so no `plane_store::DeploymentMode` crosses the boundary.
    pub mode: String,
    /// The enrollment method advertised in the bootstrap. `None` ⇒ `device_code`. A deployment whose
    /// composing surface delivers the passcode mail sets `"passcode"` explicitly — the vault holds no
    /// mail transport, so it never infers the method from one.
    pub enrollment_method: Option<String>,
}

impl Default for EnrollConfig {
    /// The accountless self-host default: no base URL, self-host posture, the device-code method.
    /// The STRICT mode is deliberately `None` — a [`PlaneState::new`] composition that never set an enroll
    /// config has not configured a mode, so the genesis/standup wrappers must refuse typed (fail closed)
    /// rather than silently assume self_host against an Authority that may be configured cloud.
    /// [`PlaneState::open`] always sets it explicitly from the parsed config.
    fn default() -> Self {
        Self {
            base_url: String::new(),
            verify_base_url: String::new(),
            link_base_url: String::new(),
            strict_deployment_mode: None,
            deployment_mode: DeploymentMode::SelfHost,
            enrollment_method: "device_code".to_owned(),
        }
    }
}

/// The Postgres pool tuning, read from the environment (the one place the plane reads `TOPOS_PLANE_DB_*`,
/// mirroring how the rate limiter reads its env). Unset knobs keep the driver defaults (`max_connections =
/// 10`, `acquire_timeout = 30s`) — raise them for a plane serving concurrent HTTP. The statement/lock ceilings
/// stay off unless the operator opts in (so a long legitimate whole-bundle render is never capped); the
/// idle-in-transaction timeout defaults to a safe 30s (every write txn is pure-DB and short, so it only ever
/// trips an abandoned/stuck one that would otherwise pin row locks — set the env to `0` to disable it).
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

/// Resolve the enrollment method a plane ADVERTISES on its bootstraps: an explicit configured value wins,
/// else `device_code`. (`passcode` is never inferred: delivery belongs to the composing surface's mail
/// seam, which the vault cannot see — a deployment that mails passcodes says so explicitly.) The reserved
/// value `"admin_claim"` is refused as a typed startup error: it is the claim-only species marker a
/// one-time `/i/` claim link carries — a plane configured to advertise it would make every client treat
/// LIVE INVITES as one-shot claims (the wrong door: no device-auth session, a `--resume`-less flow that
/// wedges).
fn resolve_enrollment_method(configured: Option<String>) -> anyhow::Result<String> {
    let method = configured.unwrap_or_else(|| "device_code".to_owned());
    anyhow::ensure!(
        method != "admin_claim",
        "enrollment method \"admin_claim\" is reserved for one-time claim links and cannot be a \
         plane's configured enrollment method; use \"device_code\" or \"passcode\""
    );
    Ok(method)
}

impl PlaneState {
    /// Construct from an already-built [`Authority`] with the **default** rate limits (read from the
    /// environment — `TOPOS_PLANE_RATELIMIT=off` disables enforcement; otherwise a generous in-process token
    /// bucket) and the default enrollment config. Override the limits with
    /// [`with_rate_limit`](Self::with_rate_limit). This names the `plane_store` [`Authority`] in its signature
    /// — it is the explicit test / advanced construction path; a downstream plane builds through the leak-free
    /// [`open`](Self::open) ([`PlaneConfig`]) instead.
    #[must_use]
    pub fn new(authority: Arc<Authority>) -> Self {
        Self {
            authority,
            limiter: Limiter::new(Limits::from_env()),
            enroll: Arc::new(EnrollConfig::default()),
            #[cfg(feature = "enroll-oidc")]
            oidc: None,
            admin_token_sha256: None,
            internal_token_sha256: None,
        }
    }

    /// Open a serving [`PlaneState`] over Postgres from a leak-free [`PlaneConfig`] — the **single** construction
    /// path a downstream plane (and the OSS bin) use. Builds the storage [`Authority`] (the db + git + large
    /// stores, the enrollment secret) and the internal enrollment config from the config's plain/owned fields,
    /// so the caller never names a `plane_store` type. Rate limits default to
    /// [`Limits::from_env`] (chain [`with_rate_limit`](Self::with_rate_limit) to override); the OIDC connector
    /// stays a post-construction, feature-gated step.
    ///
    /// # Examples
    /// A downstream plane composes the leak-free surface — build, set the policy, and mount a private route
    /// beside the OSS [`router`](crate::router) — **without ever naming a `plane_store` type** (if any field
    /// or parameter regressed to one, these plain literals / the `&str` would stop compiling):
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// use topos_plane::{PlaneConfig, PlaneState, router};
    ///
    /// let state = PlaneState::open(PlaneConfig {
    ///     database_url: "postgres://plane:secret@db:5432/plane".to_owned(),
    ///     git_root: "git".into(),
    ///     large_root: "large".into(),
    ///     enroll_secret_path: "enroll.key".into(),
    ///     base_url: "https://plane.example".to_owned(),
    ///     verify_base_url: None,
    ///     link_base_url: None,
    ///     mode: "cloud".to_owned(),
    ///     enrollment_method: None,
    /// })
    /// .await?;
    ///
    /// // The workspace policy, set through the public API — a plain `&str`, no `plane_store` type.
    /// state.set_review_required("w_acme", true).await?;
    ///
    /// // Compose: the OSS routes + a private route, behind the caller's own gate (mounted by the cloud).
    /// let app = axum::Router::new()
    ///     .merge(router(state))
    ///     .route("/admin/health", axum::routing::get(|| async { "ok" }));
    /// let _ = app;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// Returns an [`anyhow::Error`] if a store root cannot be created, the database cannot be opened or
    /// migrated, or the enrollment secret cannot be loaded or generated.
    pub async fn open(cfg: PlaneConfig) -> anyhow::Result<PlaneState> {
        // The deployment posture crosses the boundary as a String; parse it here (an unknown value warns +
        // falls back to self_host, exactly as the bin did), so no `plane_store::DeploymentMode` is named by a
        // caller. Mirrors the previous main.rs construction verbatim (the one home, no drift). The STRICT
        // parse is retained beside the fallback: the standup/create/mint wrappers refuse to run off a
        // fallback (fail closed), while the pre-existing surfaces keep their lenient behavior.
        let strict_deployment_mode = DeploymentMode::parse(&cfg.mode);
        let deployment_mode = strict_deployment_mode.unwrap_or_else(|| {
            tracing::warn!(mode = %cfg.mode, "unknown plane mode; defaulting to self_host");
            DeploymentMode::SelfHost
        });
        // Resolve the enrollment method — refusing the reserved claim-only marker (fail closed at startup).
        let enrollment_method = resolve_enrollment_method(cfg.enrollment_method)?;
        let verify_base_url = cfg
            .verify_base_url
            .clone()
            .unwrap_or_else(|| cfg.base_url.clone());
        let link_base_url = cfg
            .link_base_url
            .clone()
            .unwrap_or_else(|| cfg.base_url.clone());
        let authority = Authority::open_with_pool(
            &cfg.database_url,
            &cfg.git_root,
            &cfg.large_root,
            pool_config_from_env(),
        )
        .await
        .context("opening the storage authority")?
        .with_enrollment_config(EnrollmentConfig {
            secret_path: cfg.enroll_secret_path,
            base_url: cfg.base_url.clone(),
            verify_base_url: cfg.verify_base_url,
            link_base_url: cfg.link_base_url,
            deployment_mode,
            enrollment_method: enrollment_method.clone(),
        })
        .context("loading the enrollment secret")?;

        Ok(
            PlaneState::new(Arc::new(authority)).with_enroll_config(EnrollConfig {
                base_url: cfg.base_url,
                verify_base_url,
                link_base_url,
                strict_deployment_mode,
                deployment_mode,
                enrollment_method,
            }),
        )
    }

    /// Replace the rate limits (a composing server wires these from its config; the tests force a tiny
    /// bucket to exercise the 429 path, or `off` to ignore limits entirely).
    #[must_use]
    pub fn with_rate_limit(mut self, limits: Limits) -> Self {
        self.limiter = Limiter::new(limits);
        self
    }

    /// Enable the self-host operator's admin-authenticated policy route by configuring its bearer token.
    /// Only the token's sha256 is retained (never the raw secret — it can't reach a `Debug`/log); with no
    /// token configured the route stays disabled and answers 404. The OSS bin wires this from
    /// `--admin-token` / `TOPOS_PLANE_ADMIN_TOKEN`; a composing plane may call it too.
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        self.admin_token_sha256 = Some(topos_core::digest::sha256(token.as_bytes()));
        self
    }

    /// Whether an admin token is configured (the policy route is 404-invisible otherwise).
    pub(crate) fn admin_token_configured(&self) -> bool {
        self.admin_token_sha256.is_some()
    }

    /// Whether `provided` is the configured admin token — a fixed 32-byte sha256 compare (timing-independent
    /// of any prefix match), the same token-as-sha256 idiom the enrollment credentials use. `false` when no
    /// token is configured.
    pub(crate) fn admin_token_matches(&self, provided: &str) -> bool {
        self.admin_token_sha256
            .is_some_and(|stored| topos_core::digest::sha256(provided.as_bytes()) == stored)
    }

    /// Enable the internal session lane (`/internal/v1/*`) by configuring its bearer token. Only the token's
    /// sha256 is retained (never the raw secret — it can't reach a `Debug`/log); with no token configured the
    /// lane stays disabled and every route answers 404. The OSS bin wires this from `--internal-token` /
    /// `TOPOS_PLANE_INTERNAL_TOKEN`; a composing plane that fronts the lane with its own session-authenticated
    /// surface calls it too.
    #[must_use]
    pub fn with_internal_token(mut self, token: &str) -> Self {
        self.internal_token_sha256 = Some(topos_core::digest::sha256(token.as_bytes()));
        self
    }

    /// Whether an internal-session-lane token is configured (every `/internal/v1/*` route is 404-invisible
    /// otherwise).
    pub(crate) fn internal_token_configured(&self) -> bool {
        self.internal_token_sha256.is_some()
    }

    /// Whether `provided` is the configured internal-session-lane token — a fixed 32-byte sha256 compare
    /// (timing-independent of any prefix match), the same token-as-sha256 idiom the admin token uses.
    /// `false` when no token is configured.
    pub(crate) fn internal_token_matches(&self, provided: &str) -> bool {
        self.internal_token_sha256
            .is_some_and(|stored| topos_core::digest::sha256(provided.as_bytes()) == stored)
    }

    /// Set the enrollment config. Mirrors [`with_rate_limit`](Self::with_rate_limit): a builder the
    /// construction path calls once. **Crate-private** (it takes the crate-private [`EnrollConfig`]) —
    /// [`open`](Self::open) calls it from a leak-free [`PlaneConfig`].
    #[must_use]
    pub(crate) fn with_enroll_config(mut self, config: EnrollConfig) -> Self {
        self.enroll = Arc::new(config);
        self
    }

    /// Set the OIDC connector config (the bin reads `TOPOS_PLANE_OIDC_*` and calls this). Feature-gated —
    /// `enroll-oidc` is default-off, so a default build never resolves the connector. Mirrors
    /// [`with_rate_limit`](Self::with_rate_limit): a builder that the composition root calls once, after
    /// [`open`](Self::open).
    #[cfg(feature = "enroll-oidc")]
    #[must_use]
    pub fn with_oidc_config(mut self, config: crate::enroll::oidc::OidcConfig) -> Self {
        self.oidc = Some(Arc::new(config));
        self
    }

    /// The configured OIDC connector, if any (the OIDC routes read it; `None` ⇒ the routes 404).
    #[cfg(feature = "enroll-oidc")]
    pub(crate) fn oidc(&self) -> Option<&crate::enroll::oidc::OidcConfig> {
        self.oidc.as_deref()
    }

    /// Set the workspace's `review_required` policy through the public authority op — the **leak-free** surface
    /// a downstream plane's admin route calls. The workspace id is a plain `&str` (parsed internally) and both
    /// failure modes are stringified, so **no `plane_store` type crosses the boundary** (a composer needs only
    /// `&str` + [`anyhow`](anyhow)). With the policy on, a direct publish returns `APPROVAL_REQUIRED` and an
    /// approval needs a second reviewer; genesis + revert bypass it.
    ///
    /// # Errors
    /// Returns an [`anyhow::Error`] if `workspace_id` is not a valid id, or the authority write fails.
    pub async fn set_review_required(
        &self,
        workspace_id: &str,
        review_required: bool,
    ) -> anyhow::Result<()> {
        let ws = WorkspaceId::parse(workspace_id)
            .map_err(|error| anyhow::anyhow!("invalid workspace id `{workspace_id}`: {error}"))?;
        self.authority()
            .set_review_required(&ws, review_required)
            .await
            .map_err(|error| {
                anyhow::anyhow!("setting review-required for `{workspace_id}`: {error}")
            })
    }

    /// The DEVICE-credential workspace CATALOG read (`GET /v1/workspaces/{ws}/skills`) — the thin wrapper the
    /// route handler calls: parse the workspace id (a malformed id is the uniform miss), stamp the server
    /// clock, run [`Authority::list_skills_device`](plane_store::Authority), and map each `SkillIndexRow`
    /// into the wire [`WireSkillIndex`] (metadata only, no bytes). A [`PlaneHttpError`] carries the outcome:
    /// [`AuthorityError::NotFound`](plane_store::AuthorityError) — unknown/revoked credential or non-member —
    /// becomes the indistinguishable 404; an Integrity/Internal fault becomes a 500.
    pub(crate) async fn list_skills_device(
        &self,
        workspace_id: &str,
        credential: &str,
    ) -> Result<WireSkillIndex, PlaneHttpError> {
        let ws =
            WorkspaceId::parse(workspace_id).map_err(|_| plane_store::AuthorityError::NotFound)?;
        let now = crate::wire::now_utc().1;
        let rows = self
            .authority()
            .list_skills_device(&ws, credential, now)
            .await?;
        Ok(crate::wire::map::skill_index_to_wire(rows))
    }

    /// The storage authority — the only trust surface; handlers call its authorized operations.
    pub(crate) fn authority(&self) -> &Authority {
        &self.authority
    }

    /// The in-process rate limiter (the middleware consults it before dispatch).
    pub(crate) fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// The static enrollment config (the standup wrappers read the strict posture; tests assert the
    /// construction record).
    pub(crate) fn enroll(&self) -> &EnrollConfig {
        &self.enroll
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_enrollment_method;

    #[test]
    fn enrollment_method_resolves_the_default_and_honors_an_explicit_value() {
        // `passcode` is never inferred (the vault holds no mail transport to infer it from) — the default
        // is `device_code`, and a mail-delivering deployment says `passcode` explicitly.
        assert_eq!(resolve_enrollment_method(None).unwrap(), "device_code");
        assert_eq!(
            resolve_enrollment_method(Some("passcode".to_owned())).unwrap(),
            "passcode"
        );
        assert_eq!(
            resolve_enrollment_method(Some("device_code".to_owned())).unwrap(),
            "device_code"
        );
    }

    #[test]
    fn the_reserved_admin_claim_method_is_a_typed_startup_error() {
        // "admin_claim" is a claim-only species marker: a plane ADVERTISING it would make clients treat
        // live invites as one-shot claims (the wrong door). Constructing a PlaneState with it must fail.
        let err = resolve_enrollment_method(Some("admin_claim".to_owned())).unwrap_err();
        assert!(err.to_string().contains("reserved"), "got {err}");
    }
}
