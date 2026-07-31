# `topos-core` — the pure trust kernel

Deterministic FUNCTIONS over EXPLICIT VALUES, over this crate's OWN validated domain newtypes. A
constructed value is well-formed by construction (parse-don't-validate); the app libs convert wire
DTOs → these types at the edge, so **`topos-core` does NOT depend on `topos-types`**.

## Owns (the single implementation of each; each behind a known-answer / truth-table test)

- the byte-exact sha256 **bundle digest** + the canonical-manifest reject rules (`digest`);
- the **consent-satisfier truth table** (`consent`);
- the frozen **content-addressed identity derivation** (`identity`) — the canonical `commit_id`
  (the human-facing `version_id`, a length-prefixed binary frame). No keys, no signatures.
- the **client sync transition** (`sync`) — four states from `work==base?`×`applied==observed?` +
  the post-fetch heal. The served pointer is the sync target; integrity is the content-addressed
  id re-verified by digest on apply.
- the **author-merge policy** (`merge`) — the three-way file-set reconciliation over
  `(path, mode, content_sha256)` → a per-path `MergePlan`, the `MergeOutcome` decision, and the
  publish guard. Metadata only — the byte-level diff3 execution is `topos-gitstore`'s.

## Hard constraints

- **`#![cfg_attr(not(test), no_std)]` + `alloc`** — purity enforced by the compiler.
- **No I/O. No traits. No `tokio` / `sqlx` / `axum` / `gix` / `std::fs`.**
- **No ambient clock or RNG** — time is a `now` parameter; keys/signatures are byte parameters.
- Every core invariant is a unit or seeded generative test in this crate (deterministic in-repo
  xorshift generator — no proptest/RNG dependency).
- Depends on nothing in the workspace (`cargo xtask check-arch` enforces it).

Dependencies: `sha2` + `unicode-normalization` (the NFC path-collision fold in `digest`). Nothing
else — no crypto, no keys.
