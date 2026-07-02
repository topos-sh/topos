# `xtask` — codegen + the invariant gates

A `cargo-xtask` bin (not a production dependency). The committed `.cargo/config.toml` provides the
standard alias, so `cargo xtask <subcommand>` and `cargo run -p xtask -- <subcommand>` are the same thing.
Every subcommand locates the workspace relative to its own crate, so it is independent of the current
working directory.

```sh
cargo xtask gen-schema             # (re)generate contracts/schemas/*.schema.json + contracts/openapi/openapi.json
cargo xtask gen-schema --check     # the contract drift gate — a stale / missing / orphan artifact fails
cargo xtask gen-fixtures           # (re)generate the golden --json fixtures under contracts/fixtures/
cargo xtask gen-fixtures --check   # the fixture drift gate (same stale/missing/orphan discipline)
cargo xtask check-arch             # the architectural-layering + lint-opt-in + toolchain-pin gate
cargo xtask ci                     # ALL the non-DB gates, in CI's order, failing fast
cargo xtask conformance            # the store matrices (not yet implemented — prints so and exits 0)
```

## The subcommands

- **`gen-schema [--check]`** — reads the wire types from `topos-types` and emits one JSON-Schema per
  top-level type, per-verb `data` payload, and persisted client document into `contracts/schemas/`; the
  SAME run also generates (or `--check`s) the plane OpenAPI `contracts/openapi/openapi.json` from
  `topos_plane::openapi()`, so one gate covers both contracts. **The artifacts are generated — never
  hand-edit them**; change the types/routes, regenerate, commit, review the diff. `--check` fails on a
  stale, missing, or orphan artifact (an orphan = a committed file no current generator produces).
- **`gen-fixtures [--check]`** — builds the golden `--json` envelopes FROM the typed shapes (so they cannot
  drift from the contract) and writes them under `contracts/fixtures/json/`; `--check` is the drift gate
  with the same stale/missing/orphan discipline.
- **`check-arch`** — the dependency-graph trust claims as a gate, via `cargo tree`:
  - the client (`topos`) carries no `plane-store` / `sqlx` / `libsqlite3-sys` / `tokio` / `reqwest` /
    `hyper` edge (it is a thin sync tool, never an authority), and none of the contract-generation
    machinery (`utoipa` / `utoipa-gen` / `schemars` / `schemars_derive` — `topos-types`' schema derives
    are behind its default-off `contract-derives` feature, enabled only by xtask + `topos-plane`);
  - the kernel (`topos-core`) carries no wire DTOs, async/IO/storage/HTTP crates, or diff engines;
  - the test-only `test-fixtures` features stay OFF in the production graphs of `topos-plane` and `topos`;
  - the leaf crates stay lean (`topos-types` / `topos-harness` / `topos-gitstore` ban
    `tokio`/`axum`/`sqlx`/`ureq`/`hyper`, the first two also `gix`) — an `--all-features` check;
  - the DEFAULT (production-features) `topos-plane` build resolves neither `oauth2` nor `openidconnect`
    nor `reqwest` (the OIDC connector is feature-gated default-off) — a production-tree check;
  - every workspace member opts into the shared `[workspace.lints]` (incl. `unsafe_code = forbid`);
  - the Dockerfile's builder image tag matches `rust-toolchain.toml`'s pinned channel.
- **`ci`** — the contributor's pre-push loop: runs the full NON-DB gate sequence in the same order as the
  CI `gate` job, failing fast with a per-gate banner — `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets --locked -- -D warnings`, `cargo doc --workspace --no-deps --locked` (with
  `RUSTDOCFLAGS="-D warnings"`), `gen-schema --check`, `gen-fixtures --check`, `check-arch`. Not covered
  (they need a database or an extra tool): `cargo test --workspace` (Postgres via `DATABASE_URL`),
  `cargo deny check`, and CI's sqlx offline-metadata drift job.
- **`conformance`** — a stub for the store conformance matrices; prints "not yet implemented".

There is no formal-model subcommand — the integration interleaving tests are the correctness net.
