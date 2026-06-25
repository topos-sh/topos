# `xtask` — codegen + the conformance entrypoint

A `cargo-xtask` bin (not a production dependency). Run from the workspace root:

```sh
cargo run -p xtask -- gen-schema           # (re)generate contracts/schemas/*.schema.json from topos-types
cargo run -p xtask -- gen-schema --check   # the CI drift gate — fails if a committed schema is stale
cargo run -p xtask -- conformance          # the store matrices
```

`gen-schema` reads the wire types from `topos-types` and emits one JSON-Schema per top-level type into
`contracts/schemas/`. It locates that directory relative to its own crate, so it is independent of the
current working directory. **The schemas are generated artifacts** — never hand-edit them; change the
types, regenerate, commit, and review the diff.

The plane OpenAPI generation wires in here once `topos-plane` exposes its annotated routes.
There is no formal-model subcommand — the integration interleaving tests are the correctness net.
