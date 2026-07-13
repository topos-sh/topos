# `crates/` — the libraries (the trust kernel + storage + ports)

Five library crates, one acyclic graph. Each has its own `CLAUDE.md`:

- **`topos-types/`** — the wire DTOs (the serde boundary; the JSON-Schema source). The shared leaf.
- **`topos-core/`** — the PURE trust kernel: digest, consent, the sync transition, diff3,
  the content-addressed identity derivations (`commit_id` / `device_key_id` / `canonical_principal` — no
  keys, nothing signs). No I/O. Depends on nothing else in the workspace (not even `topos-types`).
- **`topos-gitstore/`** — the `gix` object mechanics + the content-addressed large-object store
  (verify-on-read). Path-parameterized and **bundle-generic** — one bare repo per bundle for the client,
  one per workspace for the plane; it never asks what a bundle is. Depends on `topos-core` only.
- **`topos-harness/`** — the `HarnessAdapter` port + its three impls, all built (Claude Code the
  reference; OpenClaw's concrete config bytes and Hermes's per-turn-injection claim stay provisional
  behind their pilot readiness probes). The one client-side port. Depends on `topos-core` + `topos-types`.
- **`plane-store/`** — the server authority boundary: private SQL + membership-scoped authorization + the
  atomic publish transaction, split into **custody** (bundle-generic byte custody:
  bytes/versions/pointers/GC — it speaks bundles, never skills) and **directory**
  (identity/policy: the catalog mapping names + kinds onto bundle ids, channels, person-scoped
  subscriptions, the entitlement predicate, the guarded `topos_*` policy functions) — custody consults access ONLY through the in-transaction
  **access-witness** trait the directory implements (a one-way seam `check-arch` enforces). Depends on
  `topos-core` + `topos-types` + `topos-gitstore`.

**The rule that keeps the graph legible:** a trust decision is written once, in `topos-core`. Everything
links it; nothing re-implements digest / consent / sync. (The `(epoch,seq)` CAS *decision* is the named
exception for now — it lives in `plane-store`'s SQL; its kernel extraction is on `topos-core`'s planned
list.) `topos-core` has no I/O and no workspace deps, so an orchestration edit never recompiles the
kernel math.

Every split here is paid for by a real boundary — a testable-authority boundary, a platform-dependency
boundary, or `plane-store`'s privacy boundary — never a crate-per-noun.
