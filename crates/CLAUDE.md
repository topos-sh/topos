# `crates/` — the libraries (the trust kernel + storage + ports)

Five library crates, one acyclic graph. Each has its own `CLAUDE.md`:

- **`topos-types/`** — the wire DTOs (the serde boundary; the JSON-Schema source). The shared leaf.
- **`topos-core/`** — the PURE trust kernel: digest, consent, the sync transition, the author-merge
  policy (metadata only), and the content-addressed `commit_id` derivation. No I/O; depends on
  nothing else in the workspace (not even `topos-types`).
- **`topos-gitstore/`** — the `gix` object mechanics (the client's per-bundle bare repos), the
  pure in-memory loose-object codec (`codec` — what the vault encodes/decodes its store objects
  with), and the byte-level diff3 execution behind `topos-core`'s merge policy. Role rule: no
  network, no async, no SQL (`std::fs` stays — client repos live on). Depends on `topos-core` only.
- **`topos-harness/`** — the two client-side ports + their impls: `HarnessAdapter` (where a bundle's
  bytes go) and `triggers::TriggerAdapter` (when the update check fires). Depends on `topos-core` +
  `topos-types`.
- **`plane-store/`** — the vault's byte-custody boundary: private SQL + ONE object store
  (`object_store`: a local dir by default, any S3-compatible bucket optionally) behind the ONE
  public `Authority` facade. Holds no identity, membership, or policy; two
  `cargo xtask check-arch` gates (identity-vocabulary + schema-boundary) pin that. Depends on
  `topos-core` + `topos-gitstore`.

**The rule that keeps the graph legible:** a trust decision is written once, in `topos-core`;
everything links it, nothing re-implements digest / consent / sync. (Named exception: the
generation compare-and-set *decision* lives in `plane-store`'s SQL.) `topos-core` has no I/O and
no workspace deps, so an orchestration edit never recompiles the kernel math.

Every split here is paid for by a real boundary — testable authority, platform dependency, or
`plane-store`'s privacy boundary — never a crate-per-noun.
