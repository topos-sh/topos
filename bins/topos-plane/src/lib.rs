//! `topos-plane` (lib) — the composable vault.
//!
//! The vault is PURE BYTE CUSTODY: content-addressed versions, the generation-fenced `current`
//! pointer, verified reads, and the GC fence — nothing else. It listens internal-network-only with
//! ONE caller (the composing product app), authenticated by the internal bearer token, and treats
//! every request as PRE-AUTHORIZED: authorization, protection, and entitlement are decided
//! app-side, once. Requests carry opaque `(workspace_id, bundle_id, …)` strings plus `attribution`
//! display strings stored verbatim; the vault validates SHAPE, never meaning.
//!
//! ## Shape
//!
//! - [`PlaneConfig`] + [`PlaneState::open`] — the one construction path: plain/owned config in, a
//!   serving [`PlaneState`] out (the storage authority built internally).
//! - [`router`] — the whole HTTP surface: `/healthz` + the bearer-gated `/internal/v1` custody
//!   lane; anything else is a uniform 404. Every handler is thin (parse → call the authority →
//!   serialize); no trust decision lives in a handler.
//! - [`spawn_maintenance`] / [`run_maintenance_pass`] — the storage-maintenance scheduler (the
//!   recovery sweep + quarantine janitor + per-workspace GC the storage layer mandates but does not
//!   schedule). The composition root starts it once, right after construction.
//! - [`openapi()`] — the generated OpenAPI document for the PUBLIC device lane the product app
//!   serves (the `routes::door` contract stubs); the internal custody lane stays out of the
//!   committed contract.

mod maintenance;
mod router;
mod routes;
mod state;
mod wire;

/// The `utoipa`-generated OpenAPI document for the product's public device lane.
pub mod openapi;

#[cfg(test)]
mod tests;

pub use maintenance::{MaintenancePass, run_maintenance_pass, spawn_maintenance};
pub use openapi::openapi;
pub use router::router;
pub use state::{PlaneConfig, PlaneState};
