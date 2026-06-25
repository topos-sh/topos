//! `plane-store` — THE SERVER AUTHORITY BOUNDARY.
//!
//! A crate **so that raw access is private.** Owns ALL plane SQL (**raw `sqlx` 0.8.6 + thin
//! per-table repos, NO ORM**, concrete `mod sqlite`/`mod pg` behind ONE sealed authority-ops facade,
//! never `sqlx::Any`) + git object access (via `topos-gitstore`) + R1 skill-scoped authorization +
//! the **COMPLETE Step-E transaction** (one serializable txn spanning pointer · authz · proposals ·
//! receipts · `object_presence` · leases · the in-process Ed25519 signer, calling `topos-core` for
//! every pure decision) + lifecycle/GC + roster/authz/tombstones.
//!
//! **Raw SQL + raw git reads are `pub(crate)`-private; the only public surface is authorized
//! authority operations.** That privacy boundary is the auditable-encapsulation mechanism — no code
//! *outside* this crate can bypass R1 to read a bare object (misuse-prevention, not isolation of
//! malicious same-process code).
//!
//! Later work brings in `sqlx` (pin `0.8.6`) + `ed25519-dalek`.

/// Placeholder until the authority operations land.
pub const AUTHORITY_READY: bool = false;
