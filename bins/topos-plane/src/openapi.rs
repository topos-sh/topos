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
    InviteRequest, InviteSkill, PasscodeAck, PasscodeAckStatus, PasscodeConfirmRequest,
    PasscodeConfirmResponse, PasscodeConfirmStatus, PasscodeRequest, PolicyReviewRequiredRequest,
    ProposeRequest, PublishRequest, RedeemRequest, RedeemResponse, RedeemedSkillCred,
    RevertRequest, ReviewRequest, RosterRemoveRequest, RosterSetRequest, SessionIntent,
    VerificationContextResponse, WireCandidate, WireFile, WireFileMode, WireOpenProposal,
    WireProposalList, WireSkillIndex, WireSkillIndexEntry, WireVersionFile, WireVersionMeta,
    WorkspaceRole,
};
use topos_types::results::{
    InviteData, ProposeData, PublishData, RevertData, ReviewData, ReviewDecision,
};
use topos_types::{
    ActionCode, Affected, CurrentRecord, Generation, JsonEnvelope, NextAction, PointerScope,
    Receipt, TerminalOutcome, WireCurrentRecord, WireError,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Topos OSS plane",
        description = "The self-hostable Topos plane — device-credential writes + token-scoped reads, plus the enrollment + governance surface. Every returned protocol outcome of an op_id-carrying write rides in a 200 body (the canonical JsonEnvelope + receipt); non-2xx is reserved for transport/auth/integrity faults.",
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
        // The unauthenticated invite bootstrap.
        crate::routes::bootstrap::read_invite_bootstrap,
        // Enrollment flow.
        crate::routes::enroll::start_device_auth,
        crate::routes::enroll::poll_device_auth,
        crate::routes::enroll::read_verification_context,
        crate::routes::enroll::start_passcode,
        crate::routes::enroll::complete_passcode,
        crate::routes::enroll::redeem,
        crate::routes::enroll::admin_claim,
        // Governance mutations.
        crate::routes::governance::create_invite,
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
        RedeemedSkillCred,
        AdminClaimRequest,
        // Governance request DTOs (+ the invite `data` shape).
        InviteRequest,
        InviteSkill,
        RosterSetRequest,
        RosterRemoveRequest,
        DeviceRevokeRequest,
        WorkspaceRole,
        InviteData,
    )),
    tags(
        (name = "writes", description = "Device-credential writes (publish / propose / revert / review)."),
        (name = "reads", description = "Token-scoped reads (current / bundles / versions)."),
        (name = "enrollment", description = "Invite bootstrap + the device-auth / passcode / redeem / admin-claim enrollment flow."),
        (name = "governance", description = "Owner/admin device-credential mutations (invite / roster / revoke)."),
    ),
)]
struct ApiDoc;

/// The generated OpenAPI document (serialized to `contracts/openapi/openapi.json` by `xtask`).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
