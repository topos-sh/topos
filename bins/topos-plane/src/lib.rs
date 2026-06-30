//! `topos-plane` (lib) — the composable `plane-core`.
//!
//! The public API a downstream plane composes: the authority operations (over `plane-store`, re-exported
//! through the typed routes), a [`router`] builder, and the generated [`openapi()`] document. **The private
//! cloud plane IMPORTS this lib and COMPOSES it** — it does NOT fork it, and there is **no** extension hook
//! in the OSS repo (no `PlaneExtension`/`NoOpExtension`, no callback seam). `router(state)` is the entire
//! composed surface the cloud mounts verbatim; its own entitlements / billing / SSO middleware sit *in
//! front* of it. The OSS repo is the sole, auditable implementation of the trust algorithm; the hosted
//! binary is a trusted composition of it.
//!
//! ## Shape
//!
//! - [`PlaneState`] — the shared, cheap-to-clone handle (`Arc<Authority>` + the in-process rate limiter).
//! - [`router`] — wires the seven routes (4 device-signed writes + 3 token-scoped reads) with the
//!   rate-limit middleware and a body-size limit; every handler is **thin** (parse → call the authority →
//!   serialize), never a trust decision.
//! - [`openapi()`] — the `utoipa`-generated OpenAPI document (emitted to `contracts/openapi/` by `xtask`).
//!
//! The `review-required` workspace policy + the enrollment connectors are the authority's / the enrollment
//! port's, not this layer's; they land behind their own seams.

mod rate_limit;
mod router;
mod routes;
mod state;
mod wire;

/// The `utoipa`-generated OpenAPI document for the plane's HTTP surface.
pub mod openapi;

#[cfg(test)]
mod tests;

pub use openapi::openapi;
pub use rate_limit::Limits;
pub use router::router;
pub use state::PlaneState;

/// The plane's HTTP surface is built — `router(state)` composes it.
pub const PLANE_READY: bool = true;
