//! The `utoipa`-generated OpenAPI document for the plane's HTTP surface.
//!
//! `xtask` serializes [`openapi()`] into `contracts/openapi/openapi.json` and a drift gate keeps it in sync
//! with the annotated routes + the `topos-types` wire DTOs — so the committed contract can never silently
//! diverge from the code (the same discipline the JSON-Schema artifacts use).
//!
//! The OIDC routes (behind the default-off `enroll-oidc` feature) are deliberately NOT registered here: the
//! drift gate generates this contract from the DEFAULT build, so the committed contract stays stable whether
//! or not the feature is enabled. The OIDC surface is an optional, feature-local extension.

use utoipa::OpenApi;

use topos_types::bootstrap::{
    BootstrapData, BootstrapInvite, BootstrapPlane, BootstrapSkill, BootstrapWorkspace,
    ConsentMode, DeploymentMode, VerifiedDomainStatus,
};
use topos_types::requests::{
    AdminClaimRequest, DeviceAuthorizeRequest, DeviceAuthorizeResponse, DeviceRevokeRequest,
    DeviceTokenRequest, DeviceTokenResponse, DeviceTokenStatus, DeviceTokenWorkspace,
    InvitationData, InvitationRequest, LoginData, LoginMembership, LoginRedeemRequest,
    NoticeAckRequest, PasscodeAck, PasscodeAckStatus, PasscodeConfirmRequest,
    PasscodeConfirmResponse, PasscodeConfirmStatus, PasscodeRequest, PolicyReviewRequiredRequest,
    ProposeRequest, ProtectionSetRequest, PublishRequest, RedeemRequest, RedeemResponse,
    RevertRequest, ReviewRequest, RosterRemoveRequest, RosterSetRequest, SessionIntent,
    VerificationContextResponse, WireAppliedReport, WireAppliedSkill, WireCandidate,
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireDelivery, WireDeliverySkill,
    WireFile, WireFileMode, WireLogProposal, WireLogVersion, WireMe, WireNotice, WireOpenProposal,
    WireProposalEntry, WireProposalIndex, WireProposalList, WireProtocolCard, WireReach,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog, WireVersionFile, WireVersionMeta, WireVia,
    WorkspaceRole,
};
use topos_types::results::{ProposeData, PublishData, RevertData, ReviewData, ReviewDecision};
use topos_types::{
    ActionCode, Affected, CurrentRecord, Generation, JsonEnvelope, NextAction, PointerScope,
    Receipt, TerminalOutcome, WireCurrentRecord, WireError,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Topos OSS plane",
        description = "The self-hostable Topos plane — workspace-credential writes AND reads (one Bearer credential per enrolled device; membership is the authorization), plus the enrollment + governance surface. Every returned protocol outcome of an op_id-carrying write rides in a 200 body (the canonical JsonEnvelope + receipt); non-2xx is reserved for transport/auth/integrity faults.",
        version = "0.0.0",
        license(name = "Apache-2.0"),
    ),
    paths(
        crate::routes::publish::publish,
        crate::routes::proposals::propose,
        crate::routes::reverts::revert,
        crate::routes::reviews::review,
        crate::routes::current::get_current,
        crate::routes::bundles::get_bundle,
        crate::routes::versions::get_version,
        crate::routes::proposals::list_proposals,
        // The device-credential workspace catalog read (catalog visibility == membership).
        crate::routes::skills_index::list_skills,
        // The per-device currency lane: the delivery read + the applied-state report.
        crate::routes::delivery::get_delivery,
        crate::routes::delivery::put_report,
        // The member-lane DESCRIBE reads (the two-phase verbs' "before").
        crate::routes::describe::get_me,
        crate::routes::describe::get_channels,
        crate::routes::describe::get_proposals,
        crate::routes::describe::get_log,
        crate::routes::describe::get_reach,
        // The member-lane row-op writes (subscriptions / channels / protection / notices / invitations).
        crate::routes::subscriptions::follow_skill,
        crate::routes::subscriptions::unfollow_skill,
        crate::routes::subscriptions::exclude_device,
        crate::routes::channels::channel_join,
        crate::routes::channels::channel_leave,
        crate::routes::channels::channel_place,
        crate::routes::channels::channel_unplace,
        crate::routes::protection::set_skill_protection,
        crate::routes::protection::set_channel_protection,
        crate::routes::notices::ack_notices,
        crate::routes::invitations::invite,
        // The unauthenticated claim bootstrap.
        crate::routes::bootstrap::read_bootstrap,
        // Enrollment flow (+ the login redeem).
        crate::routes::enroll::start_device_auth,
        crate::routes::enroll::poll_device_auth,
        crate::routes::enroll::read_verification_context,
        crate::routes::enroll::start_passcode,
        crate::routes::enroll::complete_passcode,
        crate::routes::enroll::redeem,
        crate::routes::enroll::admin_claim,
        crate::routes::login::login,
        // Governance mutations.
        crate::routes::governance::roster_set,
        crate::routes::governance::roster_remove,
        crate::routes::governance::revoke_device,
        // The self-host operator policy toggle (admin bearer token).
        crate::routes::policy::set_review_required,
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
        // The self-host operator policy toggle.
        PolicyReviewRequiredRequest,
        // Response / envelope DTOs.
        JsonEnvelope,
        Receipt,
        WireError,
        NextAction,
        ActionCode,
        TerminalOutcome,
        Generation,
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
        // The device-credential workspace catalog read.
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
        // The login redeem.
        LoginRedeemRequest,
        LoginData,
        LoginMembership,
        // The constant protocol card (the unmatched-path fallback's machine face).
        WireProtocolCard,
        // Per-verb `data` shapes (the agent's typed payloads).
        PublishData,
        ProposeData,
        RevertData,
        ReviewData,
        ReviewDecision,
        // The invite bootstrap payload.
        BootstrapData,
        BootstrapInvite,
        ConsentMode,
        BootstrapPlane,
        DeploymentMode,
        BootstrapWorkspace,
        VerifiedDomainStatus,
        BootstrapSkill,
        // Enrollment request/response DTOs.
        DeviceAuthorizeRequest,
        DeviceAuthorizeResponse,
        DeviceTokenRequest,
        DeviceTokenResponse,
        DeviceTokenStatus,
        DeviceTokenWorkspace,
        SessionIntent,
        VerificationContextResponse,
        PasscodeRequest,
        PasscodeAck,
        PasscodeAckStatus,
        PasscodeConfirmRequest,
        PasscodeConfirmResponse,
        PasscodeConfirmStatus,
        RedeemRequest,
        RedeemResponse,
        AdminClaimRequest,
        // Governance request DTOs.
        RosterSetRequest,
        RosterRemoveRequest,
        DeviceRevokeRequest,
        WorkspaceRole,
    )),
    tags(
        (name = "writes", description = "Device-credential writes (publish / propose / revert / review) and the member-lane row ops (follows / channels / exclusions / protection / notices)."),
        (name = "reads", description = "Workspace-credential device reads (current / bundles / versions / proposals / catalog / delivery / me / channels / proposals / log / reach) plus the body-light applied-state report."),
        (name = "enrollment", description = "Claim bootstrap + the device-auth / passcode / redeem / admin-claim / login enrollment flow."),
        (name = "governance", description = "Owner/admin device-credential mutations (roster / revoke) and the member-lane invitation (a roster write)."),
    ),
)]
struct ApiDoc;

/// The generated OpenAPI document (serialized to `contracts/openapi/openapi.json` by `xtask`).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
