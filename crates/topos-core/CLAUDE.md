# `topos-core` — the pure trust kernel

Deterministic FUNCTIONS over EXPLICIT VALUES, over this crate's OWN validated domain newtypes (`SkillId`,
`Generation`, `Commit`, the state-transition types). A constructed `topos-core` value is, by construction,
well-formed — the kernel cannot represent an invalid state (parse-don't-validate). The app libs convert
wire DTOs → these types at the edge, so **`topos-core` does NOT depend on `topos-types`**.

## Owns (the single implementation of each)

- the canonical bundle + commit construction;
- the byte-exact sha256 bundle digest + the canonical-manifest reject rules;
- the consent-satisfier truth-table, as a pure fn;
- the `(epoch, seq)` compare-and-set *decision* (current, expected → Promote | Conflict | …);
- the four-state sync *transition* fn (state, input → next);
- diff3 hunk planning;
- Ed25519 signing-PREIMAGE construction + verify (the one shared verify/preimage impl — the concrete
  `sign` lives in the caller, over the same dalek crate);
- first-parent + same-skill lineage assertions.

## Hard constraints

- **No I/O. No traits. No `tokio` / `sqlx` / `axum` / `gix` / `std::fs`.**
- **No ambient clock or RNG** — time is a `now` parameter; keys/signatures are byte parameters.
- **Every core invariant is a unit/proptest in this crate.**
- Depends on nothing else in the workspace. (It stays accidentally wasm-compilable as a free effect of
  purity, but that is not a requirement.)

Dependencies: `ed25519-dalek` (verify), `sha2`. Nothing else.
