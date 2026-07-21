//! The PUBLIC device lane's wire contract — served by the composing product app, never by the
//! vault. The vault's own HTTP surface is the internal custody lane (out of the committed
//! OpenAPI); what lives HERE is the contract: these stubs carry the `#[utoipa::path]` annotations
//! the committed OpenAPI is generated from, so the product's frozen wire stays ONE generated
//! artifact whichever tier serves an operation. The stub bodies never run and nothing routes to
//! them.
//!
//! The lane: the gh-style device-auth start/poll (enrollment — on approval the device code is
//! promoted to the device's ONE bearer credential), the publish/propose/revert/review writes, the
//! current/version/object/catalog/proposals reads, the delivery + applied-state report, the
//! describe reads (me / channels / reach / review inbox / log), the row ops (follows / exclusions /
//! channel membership + curation / protection / notices-ack / invitations), and the device revoke.

#![allow(dead_code)] // contract-only: referenced by the OpenAPI derive, routed by the web app.

use topos_types::requests::{
    DeviceAuthPollRequest, DeviceAuthPollResponse, DeviceAuthStartRequest, DeviceAuthStartResponse,
    DeviceRevokeRequest, InvitationRequest, InviteAcceptRequest, NoticeAckRequest, ProposeRequest,
    ProtectionSetRequest, PublishRequest, RevertRequest, ReviewRequest, WireAppliedReport,
    WireChannelIndex, WireDelivery, WireMe, WireProposalIndex, WireProposalList, WireReach,
    WireSkillIndex, WireSkillLog, WireVersionMeta,
};
use topos_types::{JsonEnvelope, WireCurrentRecord};

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/me",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The caller's own membership (identity + address + role + inviter).", body = WireMe),
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
        (status = 200, description = "The invitation receipt — OK carries the InvitationData (address + invited + the honest mailed flag; the tokened invite link travels ONLY in the mail); a policy refusal is a 200 DENIED OWNER_ROLE_REQUIRED, an unresolvable hint a 200 DENIED UNKNOWN_SKILL / UNKNOWN_CHANNEL, an unarmed mail transport a 200 DENIED MAIL_NOT_CONFIGURED.", body = JsonEnvelope),
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

// ── the device-auth flow (enrollment — the app serves it; the vault never sees this lane) ───────

#[utoipa::path(
    post,
    path = "/v1/device/authorize",
    tag = "enrollment",
    request_body = DeviceAuthStartRequest,
    responses(
        (status = 200, description = "The device-authorization grant: the secret device_code to poll with (promoted to the device's ONE bearer credential on approval), the human-facing user_code, and the BARE approval URL (the code never rides a URL; the approval page's lookup is a POST). An invite_token in the body is recorded on the flow unvalidated — never a token oracle.", body = DeviceAuthStartResponse),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Internal fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn device_auth_start() {}

#[utoipa::path(
    post,
    path = "/v1/device/token",
    tag = "enrollment",
    request_body = DeviceAuthPollRequest,
    responses(
        (status = 200, description = "The poll status; `granted` carries the ONE bearer credential (the promoted device code), the device id, the joined workspace, and — when the flow carried an invitation naming one — the first-destination hint.", body = DeviceAuthPollResponse),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Internal fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn device_auth_poll() {}

#[utoipa::path(
    post,
    path = "/v1/invitations/accept",
    tag = "enrollment",
    request_body = InviteAcceptRequest,
    params(("Authorization" = String, Header, description = "`Bearer <device credential>` — PERSON-scoped: the caller has no seat in the invitation's workspace yet.")),
    responses(
        (status = 200, description = "OK carries the InviteAcceptData (the joined workspace + the optional first-destination hint); the ceremony fences answer 200 DENIED INVITE_OTHER_ACCOUNT / EMAIL_UNVERIFIED (no address is ever echoed).", body = JsonEnvelope),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, a revoked device, or a dead token — invalid, expired, revoked, or already consumed (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn invite_accept() {}

// ── the writes (publish / propose / revert / review) ─────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/v1/publish",
    tag = "writes",
    request_body = PublishRequest,
    params(("Authorization" = String, Header, description = "`Bearer <device credential>`.")),
    responses(
        (status = 200, description = "The all-outcome envelope + receipt (OK / NEEDS_REVIEW / CONFLICT / DENIED …). An op_id retry replays byte-identically.", body = JsonEnvelope),
        (status = 400, description = "Malformed body / id / candidate.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn publish() {}

#[utoipa::path(
    post,
    path = "/v1/proposals",
    tag = "writes",
    request_body = ProposeRequest,
    params(("Authorization" = String, Header, description = "`Bearer <device credential>`.")),
    responses(
        (status = 200, description = "The all-outcome envelope + receipt (NEEDS_REVIEW on success — the candidate is committed without moving `current`).", body = JsonEnvelope),
        (status = 400, description = "Malformed body / id / candidate.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn propose() {}

#[utoipa::path(
    post,
    path = "/v1/reverts",
    tag = "writes",
    request_body = RevertRequest,
    params(("Authorization" = String, Header, description = "`Bearer <device credential>`.")),
    responses(
        (status = 200, description = "The all-outcome envelope + receipt — a revert is a FORWARD commit restoring the good version's bytes (the pointer never moves backward).", body = JsonEnvelope),
        (status = 400, description = "Malformed body / id.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn revert() {}

#[utoipa::path(
    post,
    path = "/v1/reviews",
    tag = "writes",
    request_body = ReviewRequest,
    params(("Authorization" = String, Header, description = "`Bearer <device credential>`.")),
    responses(
        (status = 200, description = "The all-outcome envelope + receipt (approve promotes; reject requires its reason; withdraw is author-only).", body = JsonEnvelope),
        (status = 400, description = "Malformed body / id.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn review() {}

// ── the reads (current / catalog / version / object / proposals / inbox / log) ───────────────────

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/current",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The `current` pointer record (`ETag = \"<generation>\"`; a conditional GET answers 304).", body = WireCurrentRecord),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, non-member, or no pointer (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_current() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The workspace catalog (metadata only, no bytes; catalog visibility == membership).", body = WireSkillIndex),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn list_skills() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("object_id" = String, Path, description = "The object's content id (64-char lowercase hex sha256)."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The object's verified bytes (application/octet-stream). Served only through a skill that reaches it — never by bare hash."),
        (status = 404, description = "Missing/blank credential, non-member, unreachable/unknown object, or a malformed id (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_object() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("version_id" = String, Path, description = "The version's commit id (64-char lowercase hex)."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The version's metadata + file listing (no blob bytes; the per-object read serves those).", body = WireVersionMeta),
        (status = 404, description = "Missing/blank credential, non-member, or an unknown/purged version (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_version() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/proposals",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The skill's OPEN proposals — handles only (version id, base generation, opened-at); no bytes, no proposer.", body = WireProposalList),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn list_proposals() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/proposals",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The review inbox: every OPEN proposal in the workspace, author message first.", body = WireProposalIndex),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_proposals() {}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/log",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id (an archived skill stays addressable)."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The skill's version history (purge tombstones included) + its proposal events.", body = WireSkillLog),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn get_log() {}

// ── the device revoke (the CLI logout wire) ──────────────────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/devices",
    tag = "governance",
    request_body = DeviceRevokeRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <device credential>`."),
    ),
    responses(
        (status = 200, description = "The revoke receipt (instant: the target credential stops authorizing fresh work the moment it commits).", body = JsonEnvelope),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) fn revoke_device() {}
