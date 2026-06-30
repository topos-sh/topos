//! A minimal in-process token-bucket rate limiter + the axum middleware that enforces it.
//!
//! Deliberately NOT `tower-governor`: one `Arc<Mutex<HashMap<Key, Bucket>>>`, keyed by the (hashed)
//! `Authorization` header when present else the peer IP, with a generous default bucket and an env switch
//! (`TOPOS_PLANE_RATELIMIT=off`) to disable it. A composing server that wants distributed limiting puts its
//! own middleware *in front* of `router(state)`; this is the honest single-process floor.
//!
//! On exceed the response is the **frozen 429**: HTTP 429 + a `Retry-After` header + an
//! `application/json` [`JsonEnvelope`] whose flat [`WireError`] carries `code = "RATE_LIMITED"`,
//! `outcome = RETRYABLE_FAILURE`, `retryable = true`, a single `Retry` next-action, and a
//! `retry_after_seconds` context — so a curl/tooling client sees the same shape every other error uses.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use topos_types::{
    ActionCode, Affected, JsonEnvelope, NextAction, SCHEMA_VERSION, TerminalOutcome, WireError,
};

use crate::state::PlaneState;

/// The token-bucket parameters. A composing server (or a test) sets these via
/// [`PlaneState::with_rate_limit`](crate::PlaneState::with_rate_limit); the default reads the environment.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bucket capacity — the largest burst allowed before refill matters.
    pub burst: f64,
    /// Sustained refill rate, in tokens per second.
    pub refill_per_sec: f64,
    /// When `false`, every request is allowed (the `TOPOS_PLANE_RATELIMIT=off` switch).
    pub enabled: bool,
}

impl Limits {
    /// The default limits, honoring `TOPOS_PLANE_RATELIMIT`: `off` (case-insensitive) disables enforcement;
    /// anything else (or unset) uses a generous bucket (1000 burst, 50/s sustained) — fine for a single
    /// agent's session-start sweep, a brake only on a pathological loop.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = !std::env::var("TOPOS_PLANE_RATELIMIT")
            .is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"));
        Self {
            burst: 1000.0,
            refill_per_sec: 50.0,
            enabled,
        }
    }

    /// `Retry-After`, in whole seconds — the time to earn one token (≥ 1).
    fn retry_after_seconds(&self) -> u64 {
        if self.refill_per_sec <= 0.0 {
            return 1;
        }
        ((1.0 / self.refill_per_sec).ceil() as u64).max(1)
    }
}

/// A single bucket's mutable state.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// The rate-limit key: a presented credential (hashed, so no secret sits in the map), the peer IP, or a
/// single global bucket when neither is known (e.g. an in-process `oneshot` test).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Key {
    /// `sha256(Authorization header value)`.
    Credential([u8; 32]),
    /// The peer socket address's IP.
    Peer(IpAddr),
    /// Neither a credential nor a peer (a degenerate fallback).
    Global,
}

/// The in-process limiter — cheap to clone (`Arc`), shared across requests through [`PlaneState`].
#[derive(Clone, Debug)]
pub(crate) struct Limiter {
    limits: Limits,
    buckets: Arc<Mutex<HashMap<Key, Bucket>>>,
}

/// The limiter's verdict for one request.
enum Decision {
    Allow,
    Limited { retry_after_seconds: u64 },
}

impl Limiter {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Consume one token for `key`, refilling first based on elapsed time. `Allow` when a token was
    /// available (or enforcement is off); `Limited` otherwise.
    fn check(&self, key: Key) -> Decision {
        if !self.limits.enabled {
            return Decision::Allow;
        }
        let now = Instant::now();
        // A poisoned lock only means a prior holder panicked; recover the guard and keep serving (the bucket
        // state is plain numbers — there is no broken invariant to honor).
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            tokens: self.limits.burst,
            last_refill: now,
        });
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * self.limits.refill_per_sec).min(self.limits.burst);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Decision::Allow
        } else {
            Decision::Limited {
                retry_after_seconds: self.limits.retry_after_seconds(),
            }
        }
    }
}

/// The rate-limit middleware (`from_fn_with_state`): derive the key, consult the limiter, and either run
/// the inner stack or return the frozen 429. Reads `ConnectInfo` from the request extensions (present under
/// `into_make_service_with_connect_info`, absent in an in-process `oneshot` test → the global bucket).
pub(crate) async fn enforce(State(state): State<PlaneState>, req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let key = key_for(req.headers(), peer);
    match state.limiter().check(key) {
        Decision::Allow => next.run(req).await,
        Decision::Limited {
            retry_after_seconds,
        } => too_many_requests(retry_after_seconds),
    }
}

/// Prefer the credential (so two clients behind one NAT do not share a budget); fall back to the peer IP,
/// then a single global bucket.
fn key_for(headers: &HeaderMap, peer: Option<IpAddr>) -> Key {
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        return Key::Credential(topos_core::digest::sha256(auth.as_bytes()));
    }
    match peer {
        Some(ip) => Key::Peer(ip),
        None => Key::Global,
    }
}

/// Build the frozen 429 response: status 429 + `Retry-After` + the `application/json` envelope.
fn too_many_requests(retry_after_seconds: u64) -> Response {
    let error = WireError {
        code: "RATE_LIMITED".to_owned(),
        outcome: TerminalOutcome::RetryableFailure,
        retryable: true,
        affected: Affected::default(),
        expected_generation: None,
        current_generation: None,
        context: serde_json::json!({ "retry_after_seconds": retry_after_seconds }),
        next_actions: vec![retry_action()],
    };
    let envelope = JsonEnvelope {
        schema_version: SCHEMA_VERSION,
        command: "rate_limited".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions: vec![retry_action()],
        receipt: None,
        error: Some(error),
    };
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(envelope)).into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
    resp
}

fn retry_action() -> NextAction {
    NextAction {
        code: ActionCode::Retry,
        argv: vec![],
    }
}
