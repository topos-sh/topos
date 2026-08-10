# `crates/` — the libraries (the trust kernel + storage + ports)

Five library crates, one acyclic graph. Each has its own `CLAUDE.md`:

- **`topos-types/`** — the wire DTOs (the serde boundary; the JSON-Schema source). The shared leaf.
- **`topos-core/`** — the PURE trust kernel: digest, consent, the sync transition, diff3, and the
  content-addressed `commit_id` derivation. No I/O; depends on nothing else in the workspace (not
  even `topos-types`).
- **`topos-gitstore/`** — the `gix` object mechanics + the content-addressed large-object store
  (verify-on-read). Path-parameterized and bundle-generic. Depends on `topos-core` only.
- **`topos-harness/`** — the two client-side ports + their impls: `HarnessAdapter` (where a bundle's
  bytes go) and `triggers::TriggerAdapter` (when the update check fires). Depends on `topos-core` +
  `topos-types`.
- **`plane-store/`** — the vault's byte-custody boundary: private SQL + per-workspace object
  storage behind the ONE public `Authority` facade. Holds no identity, membership, or policy; two
  `cargo xtask check-arch` gates (identity-vocabulary + schema-boundary) pin that. Depends on
  `topos-core` + `topos-gitstore`.

**The rule that keeps the graph legible:** a trust decision is written once, in `topos-core`;
everything links it, nothing re-implements digest / consent / sync. (Named exception: the
generation compare-and-set *decision* lives in `plane-store`'s SQL.) `topos-core` has no I/O and
no workspace deps, so an orchestration edit never recompiles the kernel math.

Every split here is paid for by a real boundary — testable authority, platform dependency, or
`plane-store`'s privacy boundary — never a crate-per-noun.
