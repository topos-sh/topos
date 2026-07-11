//! `topos-plane` (lib) — the composable `plane-core`.
//!
//! The public API a downstream plane composes: a leak-free [`PlaneConfig`] + [`PlaneState::open`]
//! constructor, the authority operations (over `plane-store`, re-exported through the typed routes), a
//! [`router`] builder, the [`PlaneState::set_review_required`] policy toggle, and the generated
//! [`openapi()`] document. **The private cloud plane IMPORTS this lib and COMPOSES it** — it does NOT fork
//! it, and there is **no** extension hook in the OSS repo (no `PlaneExtension`/`NoOpExtension`, no callback
//! seam). `router(state)` is the entire composed surface the cloud mounts verbatim; its own entitlements /
//! billing / SSO middleware sit *in front* of it. The OSS repo is the sole, auditable implementation of the
//! trust algorithm; the hosted binary is a trusted composition of it.
//!
//! **Leak-free by construction.** Every composer-facing surface — [`PlaneConfig`], [`PlaneState::open`],
//! [`PlaneState::set_review_required`] — names only plain/owned or `topos-plane`-owned types, so a composing
//! plane builds + serves without ever naming a `plane_store` type. (The advanced [`PlaneState::new`] still
//! takes an `Arc<plane_store::Authority>` — the explicit test / by-hand path the bin no longer uses.)
//!
//! ## Shape
//!
//! - [`PlaneConfig`] + [`PlaneState::open`] — the one construction path: plain/owned config in, a
//!   serving [`PlaneState`] out, the authority + enrollment config built internally.
//! - [`PlaneState`] — the shared, cheap-to-clone handle (`Arc<Authority>` + the in-process rate limiter).
//! - [`router`] — wires the seven routes (4 device-credential writes + 3 token-scoped reads) with the
//!   rate-limit middleware, request-level tracing (method + matched route template + status + latency —
//!   never a raw, credential-bearing path), and a body-size limit; every handler is **thin** (parse → call
//!   the authority → serialize), never a trust decision.
//! - [`PlaneState::set_review_required`] — the `review_required` workspace-policy toggle, set via the public
//!   API (the off-by-default anti-poisoning gate; a composing admin route calls it).
//! - [`spawn_maintenance`] / [`run_maintenance_pass`] — the storage-maintenance scheduler (the recovery
//!   sweep + quarantine janitor + per-workspace GC the storage layer mandates but does not schedule). The
//!   composition root starts it once, right after construction — the OSS bin does; a downstream plane
//!   makes the same call (or drives the pass from its own scheduler).
//! - [`openapi()`] — the `utoipa`-generated OpenAPI document (emitted to `contracts/openapi/` by `xtask`).
//!
//! The enrollment connectors are the enrollment port's, not this layer's; they land behind their own seams.

mod enroll;
mod maintenance;
mod rate_limit;
mod restore_cmd;
mod roster_cmd;
mod router;
mod routes;
mod session_read_cmd;
mod session_review_cmd;
mod standup_cmd;
mod state;
mod wire;

/// The `utoipa`-generated OpenAPI document for the plane's HTTP surface.
pub mod openapi;

#[cfg(test)]
mod tests;

pub use enroll::mailer::SmtpConfig;
pub use maintenance::{MaintenancePass, run_maintenance_pass, spawn_maintenance};
pub use openapi::openapi;
pub use rate_limit::Limits;
pub use restore_cmd::EpochBumpSummary;
pub use roster_cmd::{
    InviteMembersSummary, RemoveMemberSummary, RosterSeatSummary, RosterSummary,
    RotateJoinLinkSummary,
};
pub use router::router;
pub use session_read_cmd::{
    SessionCurrentSummary, SessionObjectSummary, SessionProposalsSummary, SessionVersionSummary,
    SkillIndexEntrySummary, SkillsIndexSummary,
};
pub use session_review_cmd::{
    SessionProposalDetail, SessionProposalDetailSummary, SessionRevertSummary, SessionReviewSummary,
};
pub use standup_cmd::{ApproveSessionSummary, ApproveStandupSummary, CreateWorkspaceSummary};
pub use state::{PlaneConfig, PlaneState};

/// The OIDC enrollment connector's config (feature-gated — `enroll-oidc`, default-off). Re-exported so the
/// bin can read it from the environment; the verification routes that drive the connector land next.
#[cfg(feature = "enroll-oidc")]
pub use enroll::oidc::OidcConfig;

/// The plane's HTTP surface is built — `router(state)` composes it.
pub const PLANE_READY: bool = true;
