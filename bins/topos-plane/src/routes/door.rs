//! The DOOR's wire contract — the device-lane row ops the composing web app serves since the
//! cutover (the guarded `topos_*` SQL functions under its scoped role are the implementation;
//! `web/` in this repo is the serving tier). The vault does NOT mount these paths: its own surface
//! is bytes/pointers, enrollment, and governance. What stays HERE is the contract — these stubs
//! carry the `#[utoipa::path]` annotations the committed OpenAPI is generated from, so the wire
//! stays pinned in ONE generated artifact no matter which tier serves an operation. The stub
//! bodies never run and nothing routes to them. The enrollment passcode START rides here too
//! since the mail unification: the app mints the code over the internal lane and delivers it
//! through its own mail seam (the vault keeps only the confirm — no mail transport).
//!
//! Two describe reads that LOOK like row ops — the review inbox (`GET /v1/workspaces/{ws}/proposals`)
//! and the skill log — are deliberately absent: both decorate their rows with git commit
//! messages (byte custody), so the vault keeps serving them and their annotations stay on the
//! live handlers in [`super::describe`].

#![allow(dead_code)] // contract-only: referenced by the OpenAPI derive, routed by the web app.

use topos_types::JsonEnvelope;
use topos_types::requests::{
    InvitationRequest, NoticeAckRequest, PasscodeAck, PasscodeRequest, ProtectionSetRequest,
    WireAppliedReport, WireChannelIndex, WireDelivery, WireMe, WireReach,
};

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/me",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The caller's own membership (identity + address + role + inviter + invite policy).", body = WireMe),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_me() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/channels",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The workspace channels (the structural `everyone` included), with the caller's membership marked.", body = WireChannelIndex),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_channels() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/reach",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The skill's audience (entitled members + their non-revoked devices).", body = WireReach),
        (status = 404, description = "Missing/blank credential, non-member, or unknown skill (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_reach() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/follows/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id to direct-follow (the client resolves the address to it)."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The subscription outcome (followed, or a 200 DENIED SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn follow_skill() {}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/follows/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id to unfollow (person-scoped negative mask)."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The subscription outcome (unfollowed, or a 200 DENIED SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn unfollow_skill() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/exclusions/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The followed skill's immutable id to exclude from THIS device."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The subscription outcome (excluded, or a 200 DENIED SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn exclude_device() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/channels/{ch}/membership",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name to join."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The membership outcome (joined, or a 200 DENIED CHANNEL_BUILTIN for `everyone`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn channel_join() {}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/channels/{ch}/membership",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name to leave."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The membership outcome (left / not_member, or a 200 DENIED CHANNEL_BUILTIN for `everyone`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn channel_leave() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/channels/{ch}/skills/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name (created on first placement, member-level self-serve)."),
        ("skill" = String, Path, description = "The skill's immutable id to place into the channel."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The curation outcome (placed / created, or a 200 DENIED CURATED_ROLE_REQUIRED / BAD_NAME / SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn channel_place() {}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/channels/{ch}/skills/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name."),
        ("skill" = String, Path, description = "The skill's immutable id to remove from the channel."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The curation outcome (removed / not_placed, or a 200 DENIED CURATED_ROLE_REQUIRED).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn channel_unplace() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/skills/{skill}/protection",
    tag = "writes",
    request_body = ProtectionSetRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The protect outcome (set, or a 200 DENIED REVIEWER_ROLE_REQUIRED / OWNER_ROLE_REQUIRED).", body = JsonEnvelope),
        (status = 400, description = "A level not valid for a skill (must be `reviewed` or `open`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn set_skill_protection() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/channels/{ch}/protection",
    tag = "writes",
    request_body = ProtectionSetRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The protect outcome (set, or a 200 DENIED REVIEWER_ROLE_REQUIRED / OWNER_ROLE_REQUIRED).", body = JsonEnvelope),
        (status = 400, description = "A level not valid for a channel (must be `curated` or `open`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn set_channel_protection() {}

#[utoipa::path(
    post,
    path = "/v1/workspaces/{ws}/notices/ack",
    tag = "writes",
    request_body = NoticeAckRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The notices were acked (idempotent — only the caller's own unacked rows move).", body = JsonEnvelope),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn ack_notices() {}

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
pub(crate) fn invite() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/delivery",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "This device's delivery answer (entitled skills, detached, notices, open-proposal count).", body = WireDelivery),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_delivery() {}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/report",
    tag = "reads",
    request_body = WireAppliedReport,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 204, description = "The applied-state report was recorded (no body)."),
        (status = 400, description = "Malformed body or a bad skill / version id.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn put_report() {}

#[utoipa::path(
    post,
    path = "/v1/enroll/passcode",
    tag = "enrollment",
    request_body = PasscodeRequest,
    responses(
        (status = 200, description = "A constant-shaped ack (delivery is fire-and-forget through the serving tier's mail seam; no enumeration oracle).", body = PasscodeAck),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 404, description = "No live session for that user code.", body = JsonEnvelope),
        (status = 429, description = "Rate limited.", body = JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn start_passcode() {}
