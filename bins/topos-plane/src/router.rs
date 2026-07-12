//! [`router`] — the ONE composed surface. `router(state)` is the entire HTTP plane a downstream cloud mounts
//! verbatim (its own middleware sits in front; there is no extension hook here). The workspace-credential
//! writes and reads (one Bearer credential per enrolled device), the unauthenticated invite bootstrap, the
//! enrollment flow, and the governance
//! mutations (axum 0.8 `{param}` syntax), all under the rate-limit middleware, with the body-size belts.

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post, put};
use tracing::Instrument as _;

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
    // The device-credential write routes carry a (large) JSON candidate body; the read routes carry none.
    let writes = Router::new()
        .route("/v1/publish", post(routes::publish::publish))
        .route("/v1/proposals", post(routes::proposals::propose))
        .route("/v1/reverts", post(routes::reverts::revert))
        .route("/v1/reviews", post(routes::reviews::review))
        .layer(DefaultBodyLimit::max(WRITE_BODY_LIMIT));

    let reads = Router::new()
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/current",
            get(routes::current::get_current),
        )
        // The device-credential workspace CATALOG read (metadata only; catalog visibility == membership).
        .route(
            "/v1/workspaces/{ws}/skills",
            get(routes::skills_index::list_skills),
        )
        // The per-device currency read (delivery) + the fleet's applied-state report. The report is a
        // body-light device WRITE grouped with the reads (the currency lane); it carries the small 64 KiB
        // belt as a per-route layer (the other reads carry no body, so the group has no shared belt).
        .route(
            "/v1/workspaces/{ws}/delivery",
            get(routes::delivery::get_delivery),
        )
        .route(
            "/v1/workspaces/{ws}/report",
            put(routes::delivery::put_report).layer(DefaultBodyLimit::max(ENROLL_BODY_LIMIT)),
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

    // The member-lane VERB surface: the describe reads (no body) + the small guarded row-op writes. Shared
    // the 64 KiB belt (harmless on the bodyless reads/PUTs) — every op the ONE Bearer workspace credential.
    let member_verbs = member_verb_routes().layer(DefaultBodyLimit::max(ENROLL_BODY_LIMIT));

    // The unauthenticated claim bootstrap — a GET, no body, no body-size belt.
    let public = Router::new().route("/i/{token}", get(routes::bootstrap::read_bootstrap));

    // Enrollment + governance: small JSON bodies behind the 64 KiB belt. The `/v1/workspaces/{ws}/devices`
    // and `/v1/workspaces/{ws}/roster/{email}` paths method-dispatch (redeem vs revoke; set vs remove).
    let enroll_and_governance =
        enroll_and_governance_routes().layer(DefaultBodyLimit::max(ENROLL_BODY_LIMIT));

    // The INTERNAL session lane (`/internal/v1/*`): HTTP over the lib-only session wrappers for a downstream
    // session-authenticated composing surface. Small JSON bodies behind the same 64 KiB belt; the whole lane
    // is 404-invisible until an internal token is configured (`with_internal_token`).
    let internal = internal_session_routes().layer(DefaultBodyLimit::max(ENROLL_BODY_LIMIT));

    writes
        .merge(reads)
        .merge(member_verbs)
        .merge(public)
        .merge(enroll_and_governance)
        .merge(internal)
        // Any UNMATCHED path is the constant protocol card (a GET) or the uniform JSON 404 (any other
        // method) — a resource address a client can re-root from, with no path echo and no existence signal.
        .fallback(routes::card::protocol_card)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::enforce,
        ))
        // Outermost (the last layer added), so the rate limiter's 429s are recorded too.
        .layer(axum::middleware::from_fn(trace_requests))
        .with_state(state)
}

/// The member-lane verb-surface route group (the describe reads + the guarded row-op writes), all
/// authenticated by the ONE Bearer workspace credential and front-doored by the ONE membership predicate.
/// Factored out so the body-size belt wraps the whole group in one place.
fn member_verb_routes() -> Router<PlaneState> {
    Router::new()
        // The describe reads (the two-phase verbs' "before").
        .route("/v1/workspaces/{ws}/me", get(routes::describe::get_me))
        .route(
            "/v1/workspaces/{ws}/channels",
            get(routes::describe::get_channels),
        )
        .route(
            "/v1/workspaces/{ws}/proposals",
            get(routes::describe::get_proposals),
        )
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/log",
            get(routes::describe::get_log),
        )
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/reach",
            get(routes::describe::get_reach),
        )
        // Person-scoped subscriptions + this-device exclusion.
        .route(
            "/v1/workspaces/{ws}/follows/{skill}",
            put(routes::subscriptions::follow_skill).delete(routes::subscriptions::unfollow_skill),
        )
        .route(
            "/v1/workspaces/{ws}/exclusions/{skill}",
            put(routes::subscriptions::exclude_device),
        )
        // Channel membership (join/leave) + curation (place/unplace).
        .route(
            "/v1/workspaces/{ws}/channels/{ch}/membership",
            put(routes::channels::channel_join).delete(routes::channels::channel_leave),
        )
        .route(
            "/v1/workspaces/{ws}/channels/{ch}/skills/{skill}",
            put(routes::channels::channel_place).delete(routes::channels::channel_unplace),
        )
        // The `protect` setter (kind-polymorphic: a skill bundle, or a channel mode).
        .route(
            "/v1/workspaces/{ws}/skills/{skill}/protection",
            put(routes::protection::set_skill_protection),
        )
        .route(
            "/v1/workspaces/{ws}/channels/{ch}/protection",
            put(routes::protection::set_channel_protection),
        )
        // Notices ack (person-scoped read-state) + invitation (a roster WRITE, member-level).
        .route(
            "/v1/workspaces/{ws}/notices/ack",
            post(routes::notices::ack_notices),
        )
        .route(
            "/v1/workspaces/{ws}/invitations",
            post(routes::invitations::invite),
        )
}

/// The INTERNAL session-lane route group — HTTP over the lib-only session wrappers for a downstream
/// session-authenticated composing surface. Every route is gated by the ONE internal bearer token (the whole
/// lane is 404-invisible until [`PlaneState::with_internal_token`](crate::PlaneState::with_internal_token) is
/// called) and reads the acting principal from the `x-topos-acting-email` header; the wrappers' own
/// in-transaction gates re-verify the roster rows. Factored out so the body-size belt wraps the group in one
/// place. These handlers carry NO `#[utoipa::path]` — the lane is composition-internal, out of the committed
/// OpenAPI.
fn internal_session_routes() -> Router<PlaneState> {
    Router::new()
        // Reads (member-scoped; `no-store`).
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/current",
            get(routes::internal::read_current),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}",
            get(routes::internal::read_version),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}",
            get(routes::internal::read_object),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/proposals",
            get(routes::internal::list_proposals),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}",
            get(routes::internal::read_proposal),
        )
        // Genesis / session-approval writes.
        .route(
            "/internal/v1/workspaces",
            post(routes::internal::create_workspace),
        )
        .route(
            "/internal/v1/device-sessions/{user_code}/approve",
            post(routes::internal::approve_session),
        )
        .route(
            "/internal/v1/device-sessions/{user_code}/approve-standup",
            post(routes::internal::approve_standup),
        )
        // Roster / policy / review writes.
        .route(
            "/internal/v1/workspaces/{ws}/roster/remove",
            post(routes::internal::remove_member),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/approve",
            post(routes::internal::approve_proposal),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/reject",
            post(routes::internal::reject_proposal),
        )
        .route(
            "/internal/v1/workspaces/{ws}/skills/{skill}/reverts",
            post(routes::internal::revert),
        )
}

/// Request-level tracing, wired into [`router`] so every composition gets it (no new dependency): ONE
/// `request` span per request carrying the method + the matched ROUTE TEMPLATE, and one completion `info`
/// event recording the status + latency. Handlers — and the error mapper's `tracing::error!` authority-fault
/// chains ([`crate::wire::error`]) — run inside the span, so a 500's server-side diagnostics correlate with
/// exactly one request line in the JSON logs.
///
/// The span records the route TEMPLATE, never the raw path: a raw path carries
/// the invite token on `/i/{token}`, and a credential
/// never reaches the logs (the same posture as storing only credential sha256s; the workspace
/// credential itself rides the Authorization header, which is never logged). A request that matched no
/// route has no template and logs the constant `(unmatched)` — same reasoning: a mistyped
/// token-bearing URL must not land in the logs either.
async fn trace_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("(unmatched)", MatchedPath::as_str)
        .to_owned();
    let span = tracing::info_span!("request", %method, %route);
    let started = std::time::Instant::now();
    let response = next.run(req).instrument(span.clone()).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let status = response.status().as_u16();
    span.in_scope(|| tracing::info!(status, latency_ms, "request served"));
    response
}

/// The enrollment + governance route group (sharing the 64 KiB belt). Factored out so the feature-gated OIDC
/// routes fold in cleanly under `#[cfg(feature = "enroll-oidc")]`.
fn enroll_and_governance_routes() -> Router<PlaneState> {
    let router = Router::new()
        // Enrollment (grant/passcode-auth; the redeem presents the grant + device key, no signature).
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
        // The LOGIN redeem (a login-intent grant → one credential per confirmed seat); grant-in-body, no
        // Authorization header, like the redeem.
        .route("/v1/login", post(routes::login::login))
        // `/v1/workspaces/{ws}/devices`: POST redeems (enrollment), DELETE revokes (governance).
        .route(
            "/v1/workspaces/{ws}/devices",
            post(routes::enroll::redeem).delete(routes::governance::revoke_device),
        )
        // Governance (device-credential authenticated; owner/admin).
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
