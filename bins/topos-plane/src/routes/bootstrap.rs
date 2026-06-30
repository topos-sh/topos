//! `GET /i/{token}` — the UNAUTHENTICATED invite-bootstrap read (TOFU). Resolves the opaque invite token to
//! the workspace identity + the offered skills + the plane signing root to pin, with **no bytes and no role**.
//! A revoked / expired / unknown invite is the single indistinguishable 404.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};

use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError, map};

#[utoipa::path(
    get,
    path = "/i/{token}",
    tag = "enrollment",
    params(("token" = String, Path, description = "The opaque `/i/<token>` invite token.")),
    responses(
        (status = 200, description = "The invite bootstrap payload (workspace + plane signing root; no bytes, no role).", body = topos_types::bootstrap::BootstrapData),
        (status = 404, description = "No such invite, or it is revoked/expired.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn read_invite_bootstrap(
    State(state): State<PlaneState>,
    Path(token): Path<String>,
) -> Result<Response, PlaneHttpError> {
    // The server clock (epoch-ms) enforces the invite's expiry inside the authority.
    let now = wire::now_utc().1;
    let bootstrap = state.authority().read_invite_bootstrap(&token, now).await?;
    Ok(Json(map::bootstrap_to_wire(&token, bootstrap)).into_response())
}
