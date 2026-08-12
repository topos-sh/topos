# `xtask` — codegen + the invariant gates

A `cargo-xtask` bin (not a production dependency). The committed `.cargo/config.toml` provides the
alias, so `cargo xtask <sub>` = `cargo run -p xtask -- <sub>`; every subcommand locates the
workspace relative to its own crate (cwd-independent).

```sh
cargo xtask gen-schema [--check]       # contracts/schemas/*.json + contracts/openapi/openapi.json
cargo xtask gen-fixtures [--check]     # the golden --json fixtures under contracts/fixtures/
cargo xtask gen-cli-ref [--check]      # docs/cli.md + skills/topos/reference.md from the real clap tree
cargo xtask gen-registry [--check]     # crates/topos-harness/registry.toml → web/public/harness-registry.toml
cargo xtask check-arch                 # layering + vocabulary + schema-boundary gates
cargo xtask check-registry-drift       # OPT-IN advisory: harness registry vs upstream (network; never a gate)
cargo xtask ci                         # ALL the non-DB gates, in CI's order, failing fast
```

- **`gen-schema`** — JSON-Schemas from `topos-types` + the OpenAPI from `topos_plane::openapi()`
  (the public session lane; the internal custody lane stays out of the committed contract).
  `--check` is the drift gate (stale / missing / orphan all fail). **Generated — never
  hand-edit.**
- **`gen-fixtures`** — golden `--json` envelopes built FROM the typed shapes.
- **`gen-cli-ref`** — TWO committed copies of the same bytes (`docs/cli.md` +
  `skills/topos/reference.md`), both drift-gated. The RENDERER lives in the client lib because the
  built-in skill places the same bytes at placement time — one implementation, no copy can drift;
  xtask keeps only the file-write/compare driver. It calls `topos::cli_ref_md_bundled()`, the
  render whose agent tables are spelled from the BUNDLED harness registry (the runtime
  `cli_ref_md()` spells this machine's): a committed file and its gate answer for the commit, not
  for the `~/.topos/harness-registry/` of whoever ran the command.
- **`check-arch`** — the trust claims as one gate: the client carries no
  `plane-store`/`sqlx`/async-runtime/HTTP/contract-derive edge; the kernel carries no wire DTOs
  or IO stacks; the vault cannot name identity-era stacks (`oauth2`/`openidconnect`/`reqwest`/
  `lettre`); `test-fixtures` stays off in production graphs; workspace lints + toolchain pins
  agree; the **custody-vocabulary gate** (the word `skill` appears nowhere in the vault dirs),
  the **identity-vocabulary gate** (no identity stems there — short explicit allowlist; prefer
  renaming code to allowlisting it), and the **schema-boundary gate** (no app-schema table in
  any `plane-store` SQL). All three scan gates are red-tested (`cargo test -p xtask`) and fail
  closed on a missing dir.
- **`gen-registry`** — the harness table vendored into the web app's static assets, so the plane
  serves the EXACT bytes the binary embeds (a client compares the served `version` against the one
  it was compiled with). A verbatim copy; `--check` is a byte compare.
- **`check-registry-drift`** — fetches upstream `agents.ts` (vercel-labs/skills) and diffs it
  against `crates/topos-harness/registry.toml` (through `bundled_harnesses()`, like every gate
  here); re-syncing is a deliberate human decision, which is why this never gates a push. It runs weekly on its own scheduled workflow
  (`.github/workflows/registry-drift.yml`) and by hand. It reads names/dirs only — detect-body
  changes are a known blind spot.
- **`ci`** — fmt, clippy, doc, the four drift gates, check-arch. Not covered:
  `cargo test --workspace` (needs `DATABASE_URL`), `cargo deny check`, the sqlx offline-metadata
  job.
