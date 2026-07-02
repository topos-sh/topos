# `crates/` — the libraries (the trust kernel + storage + ports)

Five library crates, one acyclic graph. Each has its own `CLAUDE.md`:

- **`topos-types/`** — the wire DTOs (the serde boundary; the JSON-Schema source). The shared leaf.
- **`topos-core/`** — the PURE trust kernel: digest, consent, the CAS decision, the sync transition, diff3,
  Ed25519 sign-preimage + verify. No I/O. Depends on nothing else in the workspace (not even `topos-types`).
- **`topos-gitstore/`** — the `gix` object mechanics + the content-addressed large-object store
  (verify-on-read). Depends on `topos-core` only.
- **`topos-harness/`** — the `HarnessAdapter` port + its three impls, all built (Claude Code the
  reference; OpenClaw's concrete config bytes and Hermes's per-turn-injection claim stay provisional
  behind their pilot readiness probes). The one client-side port. Depends on `topos-core` + `topos-types`.
- **`plane-store/`** — the server authority boundary: private SQL + skill-scoped authorization + the atomic
  publish transaction. Depends on `topos-core` + `topos-types` + `topos-gitstore`.

**The rule that keeps the graph legible:** a trust decision is written once, in `topos-core`. Everything
links it; nothing re-implements digest / consent / CAS / sync. `topos-core` has no I/O and no workspace
deps, so an orchestration edit never recompiles the kernel math.

Every split here is paid for by a real boundary — a testable-authority boundary, a platform-dependency
boundary, or `plane-store`'s privacy boundary — never a crate-per-noun.
