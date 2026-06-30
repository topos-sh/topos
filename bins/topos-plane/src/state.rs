//! [`PlaneState`] — the shared handle every handler and the rate-limit middleware read.
//!
//! Cheap to clone (an `Arc<Authority>`, the `Arc`-backed limiter, an `Arc<dyn Mailer>`, and an
//! `Arc<EnrollConfig>`), so axum can hand a copy to each request. The fields are private: a handler reaches
//! the authority through [`PlaneState::authority`], the limiter through [`PlaneState::limiter`], the mailer
//! through [`PlaneState::mailer`], and the enrollment config through [`PlaneState::enroll`] — never by
//! destructuring the struct.

use std::sync::Arc;

use plane_store::{Authority, DeploymentMode};

use crate::enroll::mailer::{Mailer, NoopMailer, SmtpConfig, SmtpMailer};
use crate::rate_limit::{Limiter, Limits};

/// The composed plane's shared state: the storage authority + the in-process rate limiter + the passcode
/// mailer + the static enrollment config. One value, cloned per request (every field is `Arc`-backed, so a
/// clone is a handful of pointer bumps).
#[derive(Clone, Debug)]
pub struct PlaneState {
    authority: Arc<Authority>,
    limiter: Limiter,
    mailer: Arc<dyn Mailer>,
    enroll: Arc<EnrollConfig>,
}

/// The static enrollment configuration the verification routes (landing next) read: the public base URL, the
/// deployment posture, the offered enrollment method, and the optional SMTP relay (Some ⇒ a real
/// [`SmtpMailer`]; None ⇒ the silent [`NoopMailer`], and the bootstrap won't advertise the passcode method).
#[derive(Debug, Clone)]
pub struct EnrollConfig {
    /// The plane's public base URL (the verification + invite links are built on it).
    pub base_url: String,
    /// This plane's deployment posture (the default for a workspace it stands up).
    pub deployment_mode: DeploymentMode,
    /// The enrollment method advertised to a bootstrapping device (e.g. `"device_code"`).
    pub enrollment_method: String,
    /// The SMTP relay, if configured. `None` ⇒ no passcode email (the self-host default).
    pub smtp: Option<SmtpConfig>,
}

impl Default for EnrollConfig {
    /// The accountless self-host default: no base URL, self-host posture, the device-code method, no SMTP.
    fn default() -> Self {
        Self {
            base_url: String::new(),
            deployment_mode: DeploymentMode::SelfHost,
            enrollment_method: "device_code".to_owned(),
            smtp: None,
        }
    }
}

impl PlaneState {
    /// Construct with the **default** rate limits (read from the environment — `TOPOS_PLANE_RATELIMIT=off`
    /// disables enforcement; otherwise a generous in-process token bucket), a silent [`NoopMailer`], and a
    /// default [`EnrollConfig`]. Override the limits with [`with_rate_limit`](Self::with_rate_limit) and the
    /// mailer + enrollment config with [`with_enroll_config`](Self::with_enroll_config).
    #[must_use]
    pub fn new(authority: Arc<Authority>) -> Self {
        Self {
            authority,
            limiter: Limiter::new(Limits::from_env()),
            mailer: Arc::new(NoopMailer),
            enroll: Arc::new(EnrollConfig::default()),
        }
    }

    /// Replace the rate limits (a composing server wires these from its config; the tests force a tiny
    /// bucket to exercise the 429 path, or `off` to ignore limits entirely).
    #[must_use]
    pub fn with_rate_limit(mut self, limits: Limits) -> Self {
        self.limiter = Limiter::new(limits);
        self
    }

    /// Set the enrollment config, constructing the mailer **internally** — a real [`SmtpMailer`] when `smtp`
    /// is `Some`, else the silent [`NoopMailer`]. Mirrors [`with_rate_limit`](Self::with_rate_limit) +
    /// internal `Limiter`: the `Mailer` trait stays crate-private; only the config crosses the boundary. An
    /// invalid SMTP config falls back to the no-op mailer (passcode email disabled) rather than failing the
    /// build, so `new + with_enroll_config` stays infallible.
    #[must_use]
    pub fn with_enroll_config(mut self, config: EnrollConfig) -> Self {
        self.mailer = match &config.smtp {
            Some(smtp) => match SmtpMailer::from_smtp_config(smtp) {
                Ok(mailer) => Arc::new(mailer),
                Err(error) => {
                    // The error never contains the credentials (they are attached infallibly, never parsed).
                    tracing::warn!(%error, "invalid SMTP config; passcode email disabled (no-op mailer)");
                    Arc::new(NoopMailer)
                }
            },
            None => Arc::new(NoopMailer),
        };
        self.enroll = Arc::new(config);
        self
    }

    /// Inject a mailer directly — the route tests pass a `FakeMailer` to read the passcode without SMTP.
    /// Test-gated (`test` / `test-fixtures`), so production never carries it (a check-arch guard asserts the
    /// feature stays off). The `Mailer` trait is crate-private, so only in-crate code can call this.
    #[cfg(any(test, feature = "test-fixtures"))]
    #[must_use]
    pub(crate) fn with_mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.mailer = mailer;
        self
    }

    /// The storage authority — the only trust surface; handlers call its authorized operations.
    pub(crate) fn authority(&self) -> &Authority {
        &self.authority
    }

    /// The in-process rate limiter (the middleware consults it before dispatch).
    pub(crate) fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// The passcode mailer (cloned by the verification handler to run the blocking send on `spawn_blocking`).
    /// Consumed by the verification routes (landing next), so unreferenced in a production lib build today.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn mailer(&self) -> &Arc<dyn Mailer> {
        &self.mailer
    }

    /// The static enrollment config (the verification routes read the base URL / posture / method / SMTP).
    /// Consumed by the verification routes (landing next), so unreferenced in a production lib build today.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn enroll(&self) -> &EnrollConfig {
        &self.enroll
    }
}
