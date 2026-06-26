# `topos-core` — the pure trust kernel

Deterministic FUNCTIONS over EXPLICIT VALUES, over this crate's OWN validated domain newtypes (`SkillId`,
`Generation`, `Commit`, the state-transition types). A constructed `topos-core` value is, by construction,
well-formed — the kernel cannot represent an invalid state (parse-don't-validate). The app libs convert
wire DTOs → these types at the edge, so **`topos-core` does NOT depend on `topos-types`**.

## Owns (the single implementation of each)

Implemented (each behind a known-answer / truth-table test):
- ✅ the byte-exact sha256 **bundle digest** + the canonical-manifest **reject rules** (`digest`);
- ✅ the **consent-satisfier truth-table**, as a pure fn (`consent`).

Planned (land behind a golden vector as their wire encoding / mechanics freeze):
- the canonical **commit** construction (the byte layout is a pending design decision — see the spec);
- the **Ed25519 signing-PREIMAGE** construction + verify (the device-op frame + pointer canonicalization
  are the same pending decision; the concrete `sign` lives in the caller, over the same dalek crate);
- the `(epoch, seq)` compare-and-set *decision*; the four-state sync *transition* fn; diff3 hunk planning;
  first-parent + same-skill lineage assertions.

## Hard constraints

- **`#![cfg_attr(not(test), no_std)]` + `alloc`** — purity is enforced by the compiler: a `std::fs` /
  `SystemTime::now` / RNG call would fail to BUILD in a production build, not just fail review.
- **No I/O. No traits. No `tokio` / `sqlx` / `axum` / `gix` / `std::fs`.**
- **No ambient clock or RNG** — time is a `now` parameter; keys/signatures are byte parameters.
- **Every core invariant is a unit/proptest in this crate.**
- Depends on nothing in the workspace, and only on crypto primitives (`cargo xtask check-arch` enforces it).

Dependencies: `sha2` today; `ed25519-dalek` lands with the signing module. Nothing else.
