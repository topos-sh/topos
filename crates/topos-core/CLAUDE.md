# `topos-core` — the pure trust kernel

Deterministic FUNCTIONS over EXPLICIT VALUES, over this crate's OWN validated domain newtypes (`SkillId`,
`Generation`, `Commit`, the state-transition types). A constructed `topos-core` value is, by construction,
well-formed — the kernel cannot represent an invalid state (parse-don't-validate). The app libs convert
wire DTOs → these types at the edge, so **`topos-core` does NOT depend on `topos-types`**.

## Owns (the single implementation of each)

Implemented (each behind a known-answer / truth-table test):
- ✅ the byte-exact sha256 **bundle digest** + the canonical-manifest **reject rules** (`digest`);
- ✅ the **consent-satisfier truth-table**, as a pure fn (`consent`);
- ✅ the frozen **signing/commit byte-encodings** (`sign`) — the canonical `commit_id` construction, the
  **Ed25519** device-op signature frame + verify, and the JCS `current`-pointer preimage + verify (the
  concrete `sign` lives in the caller, over the same dalek crate).
- ✅ the **client sync transition** (`sync`) — the four currency states from `work==base?`×`applied==observed?`,
  the anti-rollback floor + reused-tuple-ALARM evaluation (epoch-dominant generation order), and the
  post-fetch heal that distinguishes a crash-after-swap from a real divergence; all pure, behind a
  truth-table/matrix test.

Planned (land behind a golden vector as their wire encoding / mechanics freeze):
- the `(epoch, seq)` compare-and-set *decision*; diff3 hunk planning; first-parent + same-skill lineage
  assertions.

## Hard constraints

- **`#![cfg_attr(not(test), no_std)]` + `alloc`** — purity is enforced by the compiler: a `std::fs` /
  `SystemTime::now` / RNG call would fail to BUILD in a production build, not just fail review.
- **No I/O. No traits. No `tokio` / `sqlx` / `axum` / `gix` / `std::fs`.**
- **No ambient clock or RNG** — time is a `now` parameter; keys/signatures are byte parameters.
- **Every core invariant is a unit/proptest in this crate.**
- Depends on nothing in the workspace, and only on crypto primitives (`cargo xtask check-arch` enforces it).

Dependencies: `sha2` + `ed25519-dalek` (verify-only, `default-features = false`). Nothing else.
