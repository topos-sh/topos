//! `POST /v1/workspaces/{ws}/invitations` — invitation as a ROSTER WRITE. Seat one or more emails as invited
//! members (optionally pre-placing them into channels) through the guarded roster write, and answer with the
//! workspace ADDRESS (the whole invitation besides the seats — there is no invite link; the roster is the
//! lock). Member-level unless the workspace restricts inviting to owners. Authenticated by the ONE Bearer
//! workspace credential and front-doored by the ONE membership predicate.
//!
//! After a successful seating, invitation mail is best-effort: fire-and-forget per invitee on
//! `spawn_blocking` (like the passcode send), so neither the response nor its latency depends on the relay.
//! The honest `mailed` flag reports whether the plane can actually deliver (a real SMTP relay); a self-host
//! plane with no relay reports `mailed: false` and the inviter pastes the address by hand.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::{InviteOutcome, WorkspaceId};
use topos_types::JsonEnvelope;
use topos_types::requests::InvitationRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/workspaces/{ws}/invitations",
    tag = "governance",
    request_body = InvitationRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The invitation receipt — OK carries the InvitationData (address + invited + the honest mailed flag); a policy refusal is a 200 DENIED OWNER_ROLE_REQUIRED, an unknown channel a 200 DENIED UNKNOWN_CHANNEL.", body = JsonEnvelope),
        (status = 400, description = "Malformed body or a malformed invitee email.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn invite(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: axum::http::HeaderMap,
    ApiJson(req): ApiJson<InvitationRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = WorkspaceId::parse(&ws)
        .map_err(|_| PlaneHttpError::from(plane_store::AuthorityError::NotFound))?;
    // The caller's own membership supplies the workspace ADDRESS + display name for the response and the
    // mail body — and is the same membership front door the invite op runs (a non-member is the uniform 404).
    let me = state
        .authority()
        .membership_describe(&ws, &credential)
        .await?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .invite(&ws, &credential, &req.emails, &req.channels, &created_at)
        .await?;
    let envelope = match outcome {
        InviteOutcome::Invited { invited } => {
            let mailed = mail_invitations(&state, &invited, &me.display_name, &me.address);
            map::invitation_envelope(me.address, invited, mailed)
        }
        InviteOutcome::OwnerRoleRequired => {
            map::denied_code_envelope("invite", "OWNER_ROLE_REQUIRED")
        }
        InviteOutcome::UnknownChannel => map::denied_code_envelope("invite", "UNKNOWN_CHANNEL"),
    };
    Ok((StatusCode::OK, Json(envelope)).into_response())
}

/// Fire-and-forget an invitation email per invitee (on `spawn_blocking`, like the passcode), returning the
/// honest `mailed` flag — `true` only when a real relay is configured. A send failure is intentionally
/// dropped (no oracle, no latency dependence); the seats are the invitation regardless.
fn mail_invitations(
    state: &PlaneState,
    invited: &[String],
    display_name: &str,
    address: &str,
) -> bool {
    let mailed = state.mailer().can_send();
    if mailed {
        for to in invited {
            let mailer = state.mailer().clone();
            let to = to.clone();
            let display_name = display_name.to_owned();
            let address = address.to_owned();
            tokio::task::spawn_blocking(move || {
                let _ = mailer.send_invitation(&to, &display_name, &address);
            });
        }
    }
    mailed
}
