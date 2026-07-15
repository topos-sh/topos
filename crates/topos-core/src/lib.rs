//! `topos-core` — the pure trust kernel.
//!
//! Deterministic FUNCTIONS over EXPLICIT VALUES. **No I/O, no traits, no `tokio`/`sqlx`/`axum`/
//! `gix`/`std::fs`, and no ambient clock or RNG** — time is a `now` parameter; there is no crypto and
//! no keys. The kernel is `#![no_std]` (+ `alloc`) in production builds, so a clock / RNG / filesystem
//! call would fail to COMPILE — purity is enforced by the type system, not convention. Every L0
//! invariant is a unit or seeded generative test HERE, the fixed points behind golden vectors.
//!
//! The kernel does NOT depend on `topos-types`: the app libs convert wire DTOs → these domain types
//! at the edge, so an invalid deserialized value can never reach the kernel.
//!
//! Modules:
//! - [`digest`]   — canonical bundle manifest + the byte-exact sha256 digest + path reject rules.
//! - [`consent`]  — the consent-satisfier truth-table as a pure decision function.
//! - [`identity`] — the content-addressed identity derivation: the frozen `commit_id` construction
//!   (the user-facing `version_id`). No keys, no crypto — pure content-addressing, written once so
//!   every component agrees.
//! - [`sync`]     — the pure client sync transition: the four currency states and the post-fetch heal.
//!   Pure over explicit values, behind a truth-table test. No floor, no rollback machinery.
//! - [`merge`]    — the pure author-merge policy: the three-way file-set reconciliation over
//!   `(path, mode, content_sha256)` → a per-path plan, the outcome decision, and the presence-based
//!   publish guard. Metadata only — the byte-level diff3 execution lives outside the kernel.
//!
//! Still to land (each behind its golden vector): the generation CAS decision and the
//! first-parent / same-bundle lineage asserts.
#![cfg_attr(not(test), no_std)]
// Purity AND panic-freedom are enforced by the compiler in production builds: the kernel may not
// reach `std`, nor `unwrap`/`expect`/`panic!`. Tests keep them (assertions, fixture construction).
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

extern crate alloc;

pub mod consent;
pub mod digest;
pub mod identity;
pub mod merge;
pub mod sync;

#[cfg(test)]
mod testgen;
