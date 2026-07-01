//! [`router`] — the ONE composed surface. `router(state)` is the entire HTTP plane a downstream cloud mounts
//! verbatim (its own middleware sits in front; there is no extension hook here). The device-signed writes,
//! the token-scoped reads, the unauthenticated invite bootstrap, the enrollment flow, and the governance
//! mutations (axum 0.8 `{param}` syntax), all under the rate-limit middleware, with the body-size belts.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};

use crate::rate_limit;
use crate::routes;
use crate::state::PlaneState;

/// ~160 MiB: the authority's ~100 MiB per-blob reject cap, ×4/3 for base64, plus headroom for a multi-file
/// candidate's JSON framing. The authority still enforces the real per-blob cap (typed, at ingest); this is
/// only the transport belt that rejects an absurd body before it is buffered into memory.
const WRITE_BODY_LIMIT: usize = 160 * 1024 * 1024;

/// ~64 KiB: the enrollment + governance bodies are tiny (ids, an email list, a base64url key) — a small belt
/// rejects an absurd body for these non-candidate routes (the 160 MiB limit is only the byte-bearing writes).
const ENROLL_BODY_LIMIT: usize = 64 * 1024;

/// Build the composed plane router. ONE argument — the limiter lives inside [`PlaneState`].
pub fn router(state: PlaneState) -> Router {
    // The device-signed write routes carry a (large) JSON candidate body; the read routes carry none.
    let writes = Router::new()
        .route("/v1/publish", post(routes::publish::publish))
        .route("/v1/proposals", post(routes::proposals::propose))
        .route("/v1/reverts", post(routes::reverts::revert))
        .route("/v1/reviews", post(routes::reviews::review))
        .layer(DefaultBodyLimit::max(WRITE_BODY_LIMIT));

    let reads = Router::new()
        .route(
            "/v1/current/{read_token}",
            get(routes::current::get_current),
        )
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}",
            get(routes::bundles::get_bundle),
        )
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}",
            get(routes::versions::get_version),
        )
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/proposals",
            get(routes::proposals::list_proposals),
        );

    // The unauthenticated invite bootstrap (TOFU) — a GET, no body, no body-size belt.
    let public = Router::new().route("/i/{token}", get(routes::bootstrap::read_invite_bootstrap));

    // Enrollment + governance: small JSON bodies behind the 64 KiB belt. The `/v1/workspaces/{ws}/devices`
    // and `/v1/workspaces/{ws}/roster/{email}` paths method-dispatch (redeem vs revoke; set vs remove).
    let enroll_and_governance =
        enroll_and_governance_routes().layer(DefaultBodyLimit::max(ENROLL_BODY_LIMIT));

    writes
        .merge(reads)
        .merge(public)
        .merge(enroll_and_governance)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::enforce,
        ))
        .with_state(state)
}

/// The enrollment + governance route group (sharing the 64 KiB belt). Factored out so the feature-gated OIDC
/// routes fold in cleanly under `#[cfg(feature = "enroll-oidc")]`.
fn enroll_and_governance_routes() -> Router<PlaneState> {
    let router = Router::new()
        // Enrollment (grant/passcode-auth; NOT device-op-signed except redeem's possession header).
        .route(
            "/v1/device/authorize",
            post(routes::enroll::start_device_auth),
        )
        .route("/v1/device/token", post(routes::enroll::poll_device_auth))
        .route(
            "/v1/enroll/verify/{user_code}",
            get(routes::enroll::read_verification_context),
        )
        .route("/v1/enroll/passcode", post(routes::enroll::start_passcode))
        .route(
            "/v1/enroll/passcode/confirm",
            post(routes::enroll::complete_passcode),
        )
        .route("/v1/admin-claim", post(routes::enroll::admin_claim))
        // `/v1/workspaces/{ws}/devices`: POST redeems (enrollment), DELETE revokes (governance).
        .route(
            "/v1/workspaces/{ws}/devices",
            post(routes::enroll::redeem).delete(routes::governance::revoke_device),
        )
        // Governance (device-op-signed via the governance frame; owner/admin).
        .route("/v1/invites", post(routes::governance::create_invite))
        // `/v1/workspaces/{ws}/roster/{email}`: PUT sets a role, DELETE removes the principal.
        .route(
            "/v1/workspaces/{ws}/roster/{email}",
            put(routes::governance::roster_set).delete(routes::governance::roster_remove),
        )
        // The SELF-HOST operator policy toggle (admin bearer token; 404-invisible when unconfigured).
        .route(
            "/v1/workspaces/{ws}/policy/review-required",
            put(routes::policy::set_review_required),
        );

    // The OIDC connector routes — only present under the default-off feature.
    #[cfg(feature = "enroll-oidc")]
    let router = router
        .route("/v1/enroll/oidc/start", post(routes::oidc::start))
        .route("/v1/enroll/oidc/callback", post(routes::oidc::callback));

    router
}
