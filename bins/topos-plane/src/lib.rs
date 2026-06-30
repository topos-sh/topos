//! `topos-plane` (lib) — the composable `plane-core`.
//!
//! The public API a downstream plane composes: a leak-free [`PlaneConfig`] + [`PlaneState::open_sqlite`]
//! constructor, the authority operations (over `plane-store`, re-exported through the typed routes), a
//! [`router`] builder, the [`PlaneState::set_review_required`] policy toggle, and the generated
//! [`openapi()`] document. **The private cloud plane IMPORTS this lib and COMPOSES it** — it does NOT fork
//! it, and there is **no** extension hook in the OSS repo (no `PlaneExtension`/`NoOpExtension`, no callback
//! seam). `router(state)` is the entire composed surface the cloud mounts verbatim; its own entitlements /
//! billing / SSO middleware sit *in front* of it. The OSS repo is the sole, auditable implementation of the
//! trust algorithm; the hosted binary is a trusted composition of it.
//!
//! **Leak-free by construction.** Every composer-facing surface — [`PlaneConfig`], [`PlaneState::open_sqlite`],
//! [`PlaneState::set_review_required`] — names only plain/owned or `topos-plane`-owned types, so a composing
//! plane builds + serves without ever naming a `plane_store` type. (The advanced [`PlaneState::new`] still
//! takes an `Arc<plane_store::Authority>` — the explicit test / by-hand path the bin no longer uses.)
//!
//! ## Shape
//!
//! - [`PlaneConfig`] + [`PlaneState::open_sqlite`] — the one construction path: plain/owned config in, a
//!   serving [`PlaneState`] out, the authority + enrollment config built internally.
//! - [`PlaneState`] — the shared, cheap-to-clone handle (`Arc<Authority>` + the in-process rate limiter).
//! - [`router`] — wires the seven routes (4 device-signed writes + 3 token-scoped reads) with the
//!   rate-limit middleware and a body-size limit; every handler is **thin** (parse → call the authority →
//!   serialize), never a trust decision.
//! - [`PlaneState::set_review_required`] — the `review_required` workspace-policy toggle, set via the public
//!   API (the off-by-default anti-poisoning gate; a composing admin route calls it).
//! - [`openapi()`] — the `utoipa`-generated OpenAPI document (emitted to `contracts/openapi/` by `xtask`).
//!
//! The enrollment connectors are the enrollment port's, not this layer's; they land behind their own seams.

mod enroll;
mod rate_limit;
mod router;
mod routes;
mod state;
mod wire;

/// The `utoipa`-generated OpenAPI document for the plane's HTTP surface.
pub mod openapi;

#[cfg(test)]
mod tests;

pub use enroll::mailer::SmtpConfig;
pub use openapi::openapi;
pub use rate_limit::Limits;
pub use router::router;
pub use state::{PlaneConfig, PlaneState};

/// The OIDC enrollment connector's config (feature-gated — `enroll-oidc`, default-off). Re-exported so the
/// bin can read it from the environment; the verification routes that drive the connector land next.
#[cfg(feature = "enroll-oidc")]
pub use enroll::oidc::OidcConfig;

/// The plane's HTTP surface is built — `router(state)` composes it.
pub const PLANE_READY: bool = true;
