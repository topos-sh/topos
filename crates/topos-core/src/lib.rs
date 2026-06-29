//! `topos-core` — the pure trust kernel.
//!
//! Deterministic FUNCTIONS over EXPLICIT VALUES. **No I/O, no traits, no `tokio`/`sqlx`/`axum`/
//! `gix`/`std::fs`, and no ambient clock or RNG** — time is a `now` parameter; keys/signatures are
//! byte parameters. The kernel is `#![no_std]` (+ `alloc`) in production builds, so a clock / RNG /
//! filesystem call would fail to COMPILE — purity is enforced by the type system, not convention.
//! Every L0 invariant is a unit test HERE, each behind a golden vector.
//!
//! The kernel does NOT depend on `topos-types`: the app libs convert wire DTOs → these domain types
//! at the edge, so an invalid deserialized value can never reach the kernel.
//!
//! Modules:
//! - [`digest`]  — canonical bundle manifest + the byte-exact sha256 digest + path reject rules.
//! - [`consent`] — the consent-satisfier truth-table as a pure decision function.
//! - [`sign`]    — the frozen signing/commit byte encodings: `commit_id`, the device-op signature
//!   frame + verify, and the JCS `current`-pointer preimage + verify. Construction + verify live
//!   here (the one shared encoder); the concrete `sign` is the caller's, over the same crate.
//! - [`sync`]    — the pure client sync transition: the four currency states, the anti-rollback floor
//!   plus the reused-tuple ALARM evaluation, and the post-fetch heal. Pure over explicit values,
//!   behind a truth-table test.
//!
//! Still to land (each behind its golden vector): the `(epoch,seq)` CAS decision, diff3, and the
//! first-parent / same-skill lineage asserts.
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
pub mod merge;
pub mod sign;
pub mod sync;
