//! The `utoipa`-generated OpenAPI document for the plane's HTTP surface.
//!
//! `xtask` serializes [`openapi()`] into `contracts/openapi/openapi.json` and a drift gate keeps it in sync
//! with the annotated routes + the `topos-types` wire DTOs — so the committed contract can never silently
//! diverge from the code (the same discipline the JSON-Schema artifacts use).

use utoipa::OpenApi;

use topos_types::requests::{
    ProposeRequest, PublishRequest, RevertRequest, ReviewRequest, WireCandidate, WireFile,
    WireFileMode, WireVersionFile, WireVersionMeta,
};
use topos_types::results::{ProposeData, PublishData, RevertData, ReviewData, ReviewDecision};
use topos_types::{
    ActionCode, Affected, CurrentRecord, Generation, JsonEnvelope, NextAction, PointerScope,
    Receipt, Signature, SignatureAlg, SignedCurrentRecord, TerminalOutcome, WireError,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Topos OSS plane",
        description = "The self-hostable Topos plane — device-signed writes + token-scoped reads. Every returned protocol outcome rides in a 200 body (the canonical JsonEnvelope + receipt); non-2xx is reserved for transport/auth/integrity faults.",
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
    ),
    components(schemas(
        // Request DTOs.
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
        Generation,
        Affected,
        // The signed `current` pointer envelope.
        SignedCurrentRecord,
        PointerScope,
        CurrentRecord,
        Signature,
        SignatureAlg,
        // Version metadata.
        WireVersionMeta,
        WireVersionFile,
        // Per-verb `data` shapes (the agent's typed payloads).
        PublishData,
        ProposeData,
        RevertData,
        ReviewData,
        ReviewDecision,
    )),
    tags(
        (name = "writes", description = "Device-signed writes (publish / propose / revert / review)."),
        (name = "reads", description = "Token-scoped reads (current / bundles / versions)."),
    ),
)]
struct ApiDoc;

/// The generated OpenAPI document (serialized to `contracts/openapi/openapi.json` by `xtask`).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
