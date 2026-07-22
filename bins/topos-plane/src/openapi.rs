//! The `utoipa`-generated OpenAPI document for the product's PUBLIC device lane.
//!
//! `xtask` serializes [`openapi()`] into `contracts/openapi/openapi.json` and a drift gate keeps it
//! in sync with the `routes::door` contract stubs + the `topos-types` wire DTOs — so the committed
//! contract can never silently diverge from the types. Every path here is SERVED BY THE COMPOSING
//! PRODUCT APP (the door-stub precedent): the vault's own internal custody lane stays deliberately
//! OUT of the committed contract (composition-internal, one caller).

use utoipa::OpenApi;

use topos_types::requests::{
    DeviceAuthHint, DeviceAuthPollRequest, DeviceAuthPollResponse, DeviceAuthPollStatus,
    DeviceAuthStartRequest, DeviceAuthStartResponse, DeviceAuthWorkspace, DeviceLinkData,
    DeviceLinkDescribe, DeviceLinkRequest, InvitationData, InvitationRequest, InviteAcceptData,
    InviteAcceptRequest, NoticeAckRequest, ProposeRequest, ProtectionSetRequest, PublishRequest,
    RevertRequest, ReviewRequest, WireAppliedReport, WireAppliedSkill, WireCandidate,
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireDelivery, WireDeliverySkill,
    WireFile, WireFileMode, WireLogProposal, WireLogVersion, WireMe, WireNotice, WireOpenProposal,
    WireProposalEntry, WireProposalIndex, WireProposalList, WireProtocolCard, WireReach,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog, WireVersionFile, WireVersionMeta, WireVia,
};
use topos_types::results::{ProposeData, PublishData, RevertData, ReviewData, ReviewDecision};
use topos_types::{
    ActionCode, Affected, CurrentRecord, JsonEnvelope, NextAction, PointerScope, Receipt,
    TerminalOutcome, WireCurrentRecord, WireError,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Topos product API (device lane)",
        description = "The device lane the product app serves: the gh-style device-auth enrollment (start/poll — approval promotes the device code to the device's ONE bearer credential), the publish/propose/revert/review writes, the current/version/object/catalog/proposals reads, the delivery + applied-state report, the describe reads, the row ops, the browser-free device-link lane (describe/create — an enrolled device joins a further workspace without a second ceremony), and the global device self-revoke. Every returned protocol outcome of an op_id-carrying write rides in a 200 body (the canonical JsonEnvelope + receipt); non-2xx is reserved for transport/auth/integrity faults.",
        version = "0.0.0",
        license(name = "Apache-2.0"),
    ),
    paths(
        // Writes.
        crate::routes::door::publish,
        crate::routes::door::propose,
        crate::routes::door::revert,
        crate::routes::door::review,
        // Reads.
        crate::routes::door::get_current,
        crate::routes::door::get_object,
        crate::routes::door::get_version,
        crate::routes::door::list_proposals,
        crate::routes::door::list_skills,
        crate::routes::door::get_delivery,
        crate::routes::door::put_report,
        crate::routes::door::get_me,
        crate::routes::door::get_channels,
        crate::routes::door::get_proposals,
        crate::routes::door::get_log,
        crate::routes::door::get_reach,
        // Row ops.
        crate::routes::door::follow_skill,
        crate::routes::door::unfollow_skill,
        crate::routes::door::exclude_device,
        crate::routes::door::channel_join,
        crate::routes::door::channel_leave,
        crate::routes::door::channel_place,
        crate::routes::door::channel_unplace,
        crate::routes::door::set_skill_protection,
        crate::routes::door::set_channel_protection,
        crate::routes::door::ack_notices,
        crate::routes::door::invite,
        // Enrollment: the device-auth flow, the enrolled device's invitation accept, the
        // browser-free device-link lane, and the global self-revoke (the CLI logout wire).
        crate::routes::door::device_auth_start,
        crate::routes::door::device_auth_poll,
        crate::routes::door::invite_accept,
        crate::routes::door::get_device_link,
        crate::routes::door::create_device_link,
        crate::routes::door::revoke_device,
    ),
    components(schemas(
        // Request DTOs (writes).
        PublishRequest,
        ProposeRequest,
        RevertRequest,
        ReviewRequest,
        WireCandidate,
        WireFile,
        WireFileMode,
        // Response / envelope DTOs.
        JsonEnvelope,
        Receipt,
        WireError,
        NextAction,
        ActionCode,
        TerminalOutcome,
        Affected,
        // The `current` pointer envelope.
        WireCurrentRecord,
        PointerScope,
        CurrentRecord,
        // Version metadata.
        WireVersionMeta,
        WireVersionFile,
        // The proposals-listing read.
        WireProposalList,
        WireOpenProposal,
        // The workspace catalog read.
        WireSkillIndex,
        WireSkillIndexEntry,
        // The per-device delivery read + the applied-state report.
        WireDelivery,
        WireDeliverySkill,
        WireVia,
        WireNotice,
        WireAppliedReport,
        WireAppliedSkill,
        // The member-lane DESCRIBE reads.
        WireMe,
        WireChannelIndex,
        WireChannelEntry,
        WireChannelSkill,
        WireProposalIndex,
        WireProposalEntry,
        WireSkillLog,
        WireLogVersion,
        WireLogProposal,
        WireReach,
        // The member-lane row-op write bodies.
        ProtectionSetRequest,
        NoticeAckRequest,
        InvitationRequest,
        InvitationData,
        // The constant protocol card (the unmatched-path fallback's machine face).
        WireProtocolCard,
        // Per-verb `data` shapes (the agent's typed payloads).
        PublishData,
        ProposeData,
        RevertData,
        ReviewData,
        ReviewDecision,
        // The device-auth flow + the invitation accept.
        DeviceAuthStartRequest,
        DeviceAuthStartResponse,
        DeviceAuthPollRequest,
        DeviceAuthPollResponse,
        DeviceAuthPollStatus,
        DeviceAuthWorkspace,
        DeviceAuthHint,
        InviteAcceptRequest,
        InviteAcceptData,
        // The device-link lane.
        DeviceLinkRequest,
        DeviceLinkDescribe,
        DeviceLinkData,
    )),
    tags(
        (name = "writes", description = "Device-credential writes (publish / propose / revert / review) and the member-lane row ops (follows / channels / exclusions / protection / notices)."),
        (name = "reads", description = "Device-credential reads (current / bundles / versions / proposals / catalog / delivery / me / channels / log / reach) plus the body-light applied-state report."),
        (name = "enrollment", description = "The gh-style device-auth flow (start / poll; approval promotes the device code to the ONE bearer credential and mints the FIRST device↔workspace link), the browser-free device-link lane, and the global device self-revoke."),
        (name = "governance", description = "The member-lane invitation (a roster write)."),
    ),
)]
struct ApiDoc;

/// The generated OpenAPI document (serialized to `contracts/openapi/openapi.json` by `xtask`).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
