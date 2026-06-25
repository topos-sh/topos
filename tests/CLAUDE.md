# `tests/` — the integration oracle stack

Workspace-level integration tests, in tiers:

- **`invariants/`** — the core invariants wired as executable checks (the fast oracle).
- **`contracts/`** — the generated JSON-Schema + fixtures asserted against real serialization.
- **`interleavings/`** — concurrency/sync interleavings against a real SQL primary + git store (the
  correctness net in place of a formal model).
- **`hero/`** — the end-to-end hero flow on real harness adapters.

Per-crate unit + proptests live in their crates (every trust invariant is a unit/proptest in `topos-core`);
this directory is for cross-crate integration. The tiers fill in as the project matures — this is the
initial scaffold.
