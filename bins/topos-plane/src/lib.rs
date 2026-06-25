//! `topos-plane` (lib) — the composable `plane-core`.
//!
//! The public API a downstream plane composes: the authority operations (over `plane-store`), a
//! `router(state)` builder, the **`review-required` workspace policy** (an authoritative
//! `workspace_policy` row read+locked inside the Step-E txn), and the **enrollment state
//! machine** (device-flow / passcode / magic-link / invite-chain / the single generic OSS OIDC
//! connector — concrete modules, behind a cargo feature + deferred).
//!
//! **The private cloud plane IMPORTS this lib and COMPOSES it** — it does NOT fork it, and
//! there is NO extension hook left in the OSS repo (no `PlaneExtension`/`NoOpExtension`). The OSS
//! repo is the sole, auditable implementation of the trust algorithm; the hosted binary is a
//! trusted composition of it.
//!
//! Later work brings in `axum`, then the enrollment connectors.

/// The `router(state)` builder + authority API land later.
pub const PLANE_READY: bool = false;
