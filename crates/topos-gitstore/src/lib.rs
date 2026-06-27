//! `topos-gitstore` — the shared dumb `gix` object/byte layer.
//!
//! A path-**parameterized** embedded-git store (one bare repo per skill): import a bundle of files as
//! real content-addressed git objects, snapshot it as a commit, render it back, and walk its history.
//! It depends only on `topos-core` (the trust kernel) + `topos-types` + `gix`, holds **no `~/.topos/`
//! policy and no access control**, and is shared by the client today (the plane later).
//!
//! ## Identity is topos's, never gix's
//!
//! git object ids are SHA-1 — an **internal storage handle**. The real identity is always topos's own
//! sha256, computed by the kernel:
//! - `blob_id      = sha256(raw file bytes)`            (the byte-exact unit; no Git-LFS pointer files)
//! - `bundle_digest = sha256(canonical manifest)`        (the unit of consent — [`topos_core::digest`])
//! - `version_id    = sha256(canonical commit frame)`    (the user-facing `<skill>@<version_id>` — [`topos_core::sign`])
//!
//! The `version_id -> git commit` map **is** a ref name (`refs/topos/versions/<version_id_hex>`) — no
//! second index to keep crash-safe. [`Store::commit`] re-derives the `version_id` from its arguments
//! through the kernel and refuses to write a ref that would lie about its own identity.
//!
//! ## Verify-on-read (never trusts gix's object id)
//!
//! [`Store::render_verified`] re-hashes **every** stored blob's raw bytes through the kernel sha256,
//! reconstructs the canonical manifest, recomputes `bundle_digest`, and asserts it equals the digest the
//! caller is pinned to (its `lock.json`). A single flipped byte changes a blob hash, which changes the
//! recomputed digest, which fails **typed**. gix's own sha-1 verification is irrelevant — we authenticate
//! against topos's sha256, end to end.
//!
//! ## Placement-independent identity (the large-object offload is a drop-in)
//!
//! Every file — regardless of size, with no size cap this increment — is a real git blob addressed by
//! `blob_id = sha256(raw bytes)`. Because identity is recomputed over real bytes, *which* store physically
//! holds a blob never changes any id or digest. The [`largeobj`] seam is declared (unwired) so a later
//! size-routed local / S3-compatible backend slots behind one read chokepoint with zero identity impact.

mod diff;
mod error;
mod fence;
mod read;
mod store;

pub mod largeobj;

#[cfg(test)]
mod tests;

pub use diff::{DiffFile, unified_diff};
pub use error::{GitstoreError, VerifyError};
pub use fence::{GIT_OID_LEN, StagedBundle, StagedEntry};
pub use read::{RenderedBundle, RenderedFile, VersionNode};
pub use store::{ImportFile, Store, TreeHandle, WriteBatch};

/// Re-exported for callers that build [`ImportFile`]s — the same two regular-file modes the kernel allows.
pub use topos_core::digest::FileMode;

/// The git ref namespace under which each version's commit is recorded; the ref name carries the
/// `version_id` (lowercase hex), so the ref set **is** the persisted `version_id -> git OID` map.
pub(crate) const VERSION_REF_PREFIX: &str = "refs/topos/versions/";
