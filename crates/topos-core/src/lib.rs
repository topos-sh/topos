//! `topos-core` — the pure trust kernel.
//!
//! Deterministic FUNCTIONS over EXPLICIT VALUES, over this crate's OWN validated domain newtypes
//! (`SkillId`, `Generation`, `Commit`, the state-transition types — parse-don't-validate, so a
//! constructed value is well-formed by construction and the kernel cannot represent an invalid
//! state). **No I/O, no traits, no `tokio`/`sqlx`/`axum`/`gix`/`std::fs`, and no ambient clock or
//! RNG** — time is a `now` parameter; keys/signatures are byte parameters. **Every L0 invariant is
//! a unit/proptest HERE.**
//!
//! The kernel does NOT depend on `topos-types`: the app libs convert wire DTOs → these domain types
//! at the edge, so an invalid deserialized value can never reach the kernel.
//!
//! Planned modules (each behind its golden vector):
//! - `digest`   — canonical bundle + commit construction; the byte-exact sha256 + reject rules.
//! - `consent`  — the consent satisfier truth-table as a pure fn.
//! - `cas`      — the `(epoch,seq)` CAS *decision* (current, expected → Promote | Conflict | …).
//! - `sync`     — the four-state sync *transition* fn (state, input → next).
//! - `sign`     — the device-op signing PREIMAGE (`TOPOS_DEVICE_OP_SIG_V1`) + verify.
//! - `diff3`    — author-only diff3 hunk planning.
//! - `lineage`  — first-parent + same-skill lineage asserts.

/// Placeholder until the domain newtypes land. Keeps the crate non-empty + linked.
pub const KERNEL_READY: bool = false;
