//! `GET /i/{token}` — the UNAUTHENTICATED bootstrap read (TOFU). An `/i/` link resolves to EITHER an invite
//! (the workspace identity + offered skills + the plane signing root to pin, **no bytes, no role**) OR a
//! one-time admin CLAIM (the workspace-to-be's name, no skills, `enrollment_method = "admin_claim"`). The
//! two live in disjoint tables and are probed in sequence — a claim token can never resolve as an invite
//! nor vice versa — and every dead/unknown token of either kind is the single indistinguishable 404.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use plane_store::AuthorityError;

use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError, map};

#[utoipa::path(
    get,
    path = "/i/{token}",
    tag = "enrollment",
    params(("token" = String, Path, description = "The opaque `/i/<token>` invite or admin-claim token.")),
    responses(
        (status = 200, description = "The bootstrap payload (workspace + plane signing root; no bytes, no role). A claim link carries enrollment_method \"admin_claim\" and no skills.", body = topos_types::bootstrap::BootstrapData),
        (status = 404, description = "No such invite or claim, or it is revoked/consumed/expired.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn read_invite_bootstrap(
    State(state): State<PlaneState>,
    Path(token): Path<String>,
) -> Result<Response, PlaneHttpError> {
    // The server clock (epoch-ms) enforces the invite/claim expiry inside the authority.
    let now = wire::now_utc().1;
    // Try the invite table first, then the claim table — sequential probes over DISJOINT stores (an
    // invite resolver never touches claims and vice versa), folding both misses into one uniform 404.
    let bootstrap = match state.authority().read_invite_bootstrap(&token, now).await {
        Ok(bootstrap) => bootstrap,
        Err(AuthorityError::NotFound) => {
            state.authority().read_claim_bootstrap(&token, now).await?
        }
        Err(other) => return Err(other.into()),
    };
    Ok(Json(map::bootstrap_to_wire(&token, bootstrap)).into_response())
}
