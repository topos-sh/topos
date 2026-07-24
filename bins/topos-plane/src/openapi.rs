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
    DeviceAuthStartRequest, DeviceAuthStartResponse, DeviceAuthWorkspace, InvitationData,
    InvitationRequest, NoticeAckRequest, ProposeRequest, ProtectionSetRequest, PublishRequest,
    RevertRequest, ReviewRequest, WireAppliedReport, WireAppliedSkill, WireCandidate,
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireDelivery, WireDeliverySkill,
    WireFile, WireFileMode, WireLogProposal, WireLogVersion, WireMe, WireNotice, WireOpenProposal,
    WireProposalEntry, WireProposalIndex, WireProposalList, WireProtocolCard, WireReach,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog, WireUpstream, WireVersionFile,
    WireVersionMeta, WireVia,
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
        description = "The session lane the product app serves: the RFC-8628-shaped login flow (start/poll — approval promotes the flow code to the SESSION's workspace-scoped bearer credential), the session self-end, the publish/propose/revert/review writes, the current/version/object/catalog/proposals reads, the delivery + applied-state report, the describe reads, and the row ops (the server-stored profile / channel curation / protection / notices-ack / invitations). Every returned protocol outcome of an op_id-carrying write rides in a 200 body (the canonical JsonEnvelope + receipt); non-2xx is reserved for transport/auth/integrity faults.",
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
        crate::routes::door::profile_include_skill,
        crate::routes::door::profile_remove_skill,
        crate::routes::door::profile_include_channel,
        crate::routes::door::profile_remove_channel,
        crate::routes::door::channel_place,
        crate::routes::door::channel_unplace,
        crate::routes::door::set_skill_protection,
        crate::routes::door::set_channel_protection,
        crate::routes::door::ack_notices,
        crate::routes::door::invite,
        // Enrollment: the login flow (session minting) + the session self-end (the CLI logout
        // wire).
        crate::routes::door::login_authorize,
        crate::routes::door::login_token,
        crate::routes::door::end_session,
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
        // The login flow (session minting).
        DeviceAuthStartRequest,
        DeviceAuthStartResponse,
        DeviceAuthPollRequest,
        DeviceAuthPollResponse,
        DeviceAuthPollStatus,
        DeviceAuthWorkspace,
        DeviceAuthHint,
        // The publish provenance adjunct (a GitHub-imported bundle's origin).
        WireUpstream,
    )),
    tags(
        (name = "writes", description = "Session-credential writes (publish / propose / revert / review) and the member-lane row ops (channel curation / protection / notices-ack)."),
        (name = "reads", description = "Session-credential reads (current / bundles / versions / proposals / catalog / delivery / me / channels / log / reach) plus the body-light applied-state report."),
        (name = "rows", description = "The server-stored profile row ops (the person's `-g` manifest layer)."),
        (name = "enrollment", description = "The RFC-8628-shaped login flow (start / poll; approval promotes the flow code to the SESSION's workspace-scoped bearer credential) and the session self-end."),
        (name = "governance", description = "The member-lane invitation (a roster write)."),
    ),
)]
struct ApiDoc;

/// The generated OpenAPI document (serialized to `contracts/openapi/openapi.json` by `xtask`).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
