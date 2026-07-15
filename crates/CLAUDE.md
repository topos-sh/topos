# `crates/` — the libraries (the trust kernel + storage + ports)

Five library crates, one acyclic graph. Each has its own `CLAUDE.md`:

- **`topos-types/`** — the wire DTOs (the serde boundary; the JSON-Schema source). The shared leaf.
- **`topos-core/`** — the PURE trust kernel: digest, consent, the sync transition, diff3, and the
  content-addressed `commit_id` derivation (no keys, nothing signs). No I/O. Depends on nothing else
  in the workspace (not even `topos-types`).
- **`topos-gitstore/`** — the `gix` object mechanics + the content-addressed large-object store
  (verify-on-read). Path-parameterized and **bundle-generic** — one bare repo per bundle for the
  client, one per workspace for the vault; it never asks what a bundle is. Depends on `topos-core`
  only.
- **`topos-harness/`** — the `HarnessAdapter` port + its impls. The one client-side port. Depends on
  `topos-core` + `topos-types`.
- **`plane-store/`** — the vault's byte-custody boundary: private SQL + per-workspace object storage
  behind the ONE public `Authority` facade. PURE BYTE CUSTODY — content-addressed versions, the
  generation-fenced `current` pointer, verified reads, purge/reclaim, and the GC fence. It holds no
  identity, membership, or policy (the app owns those in its own schema); every request is
  pre-authorized by its one caller. Two automated `cargo xtask check-arch` gates pin the boundary:
  the identity-vocabulary gate (no identity words anywhere in the vault) and the schema-boundary
  gate (no app-schema table named in any SQL). Depends on `topos-core` + `topos-gitstore`.

**The rule that keeps the graph legible:** a trust decision is written once, in `topos-core`.
Everything links it; nothing re-implements digest / consent / sync. (The generation CAS *decision*
is the named exception for now — it lives in `plane-store`'s SQL; its kernel extraction is on
`topos-core`'s planned list.) `topos-core` has no I/O and no workspace deps, so an orchestration
edit never recompiles the kernel math.

Every split here is paid for by a real boundary — a testable-authority boundary, a
platform-dependency boundary, or `plane-store`'s privacy boundary — never a crate-per-noun.
