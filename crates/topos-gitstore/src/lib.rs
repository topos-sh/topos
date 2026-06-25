//! `topos-gitstore` — the shared dumb `gix` object mechanics.
//!
//! Object read/write, dedup, **tree → empty-staging-dir render** (NO in-place reset), diff/diff3
//! execution, and the **topos-sha256-ID ↔ git-OID mapping** (a defined, tested invariant — git OIDs
//! are SHA-1, an internal detail; the version id is always our own sha256). **Re-verifies bytes →
//! expected sha256 on every read** (never trusts gix's object id). The untrusted tree renderer is
//! fuzzed. Holds NO access control. Shared by the plane (`plane-store`) and the client (`topos`).
//!
//! Later work brings in `gix` behind this small surface.

/// Placeholder until the gix mechanics land.
pub const GITSTORE_READY: bool = false;
