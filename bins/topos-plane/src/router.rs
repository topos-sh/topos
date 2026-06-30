//! [`router`] — the ONE composed surface. `router(state)` is the entire HTTP plane a downstream cloud mounts
//! verbatim (its own middleware sits in front; there is no extension hook here). Seven routes (axum 0.8
//! `{param}` syntax), the rate-limit middleware over all of them, and a body-size limit on the writes.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

use crate::rate_limit;
use crate::routes;
use crate::state::PlaneState;

/// ~160 MiB: the authority's ~100 MiB per-blob reject cap, ×4/3 for base64, plus headroom for a multi-file
/// candidate's JSON framing. The authority still enforces the real per-blob cap (typed, at ingest); this is
/// only the transport belt that rejects an absurd body before it is buffered into memory.
const WRITE_BODY_LIMIT: usize = 160 * 1024 * 1024;

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
        );

    writes
        .merge(reads)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::enforce,
        ))
        .with_state(state)
}
