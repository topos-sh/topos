//! [`PlaneState`] — the shared handle every handler and the rate-limit middleware read.
//!
//! Cheap to clone (an `Arc<Authority>` + the `Arc`-backed limiter), so axum can hand a copy to each
//! request. The fields are private: a handler reaches the authority through [`PlaneState::authority`] and
//! the limiter through [`PlaneState::limiter`], never by destructuring the struct.

use std::sync::Arc;

use plane_store::Authority;

use crate::rate_limit::{Limiter, Limits};

/// The composed plane's shared state: the storage authority + the in-process rate limiter. One value,
/// cloned per request (both fields are `Arc`-backed, so a clone is two pointer bumps).
#[derive(Clone, Debug)]
pub struct PlaneState {
    authority: Arc<Authority>,
    limiter: Limiter,
}

impl PlaneState {
    /// Construct with the **default** rate limits (read from the environment — `TOPOS_PLANE_RATELIMIT=off`
    /// disables enforcement; otherwise a generous in-process token bucket). Override with
    /// [`with_rate_limit`](Self::with_rate_limit).
    #[must_use]
    pub fn new(authority: Arc<Authority>) -> Self {
        Self {
            authority,
            limiter: Limiter::new(Limits::from_env()),
        }
    }

    /// Replace the rate limits (a composing server wires these from its config; the tests force a tiny
    /// bucket to exercise the 429 path, or `off` to ignore limits entirely).
    #[must_use]
    pub fn with_rate_limit(mut self, limits: Limits) -> Self {
        self.limiter = Limiter::new(limits);
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
}
