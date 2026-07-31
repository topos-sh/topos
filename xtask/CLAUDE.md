# `xtask` — codegen + the invariant gates

A `cargo-xtask` bin (not a production dependency). The committed `.cargo/config.toml` provides the
alias, so `cargo xtask <sub>` = `cargo run -p xtask -- <sub>`; every subcommand locates the
workspace relative to its own crate (cwd-independent).

```sh
cargo xtask gen-schema [--check]       # contracts/schemas/*.json + contracts/openapi/openapi.json
cargo xtask gen-fixtures [--check]     # the golden --json fixtures under contracts/fixtures/
cargo xtask gen-cli-ref [--check]      # docs/cli.md + skills/topos/reference.md from the real clap tree
cargo xtask check-arch                 # layering + vocabulary + schema-boundary gates
cargo xtask check-registry-drift       # OPT-IN advisory: baked harness registry vs upstream (network; NEVER in CI)
cargo xtask ci                         # ALL the non-DB gates, in CI's order, failing fast
```

- **`gen-schema`** — JSON-Schemas from `topos-types` + the OpenAPI from `topos_plane::openapi()`
  (the public session lane; the internal custody lane stays out of the committed contract).
  `--check` is the drift gate (stale / missing / orphan all fail). **Generated — never
  hand-edit.**
- **`gen-fixtures`** — golden `--json` envelopes built FROM the typed shapes.
- **`gen-cli-ref`** — TWO committed copies of the same bytes (`docs/cli.md` +
  `skills/topos/reference.md`), both drift-gated. The RENDERER lives in the client lib
  (`topos::cli_ref_md()`) because the built-in skill places the same bytes at placement time —
  one implementation, no copy can drift; xtask keeps only the file-write/compare driver.
- **`check-arch`** — the trust claims as one gate: the client carries no
  `plane-store`/`sqlx`/async-runtime/HTTP/contract-derive edge; the kernel carries no wire DTOs
  or IO stacks; the vault cannot name identity-era stacks (`oauth2`/`openidconnect`/`reqwest`/
  `lettre`); `test-fixtures` stays off in production graphs; workspace lints + toolchain pins
  agree; the **custody-vocabulary gate** (the word `skill` appears nowhere in the vault dirs),
  the **identity-vocabulary gate** (no identity stems there — short explicit allowlist; prefer
  renaming code to allowlisting it), and the **schema-boundary gate** (no app-schema table in
  any `plane-store` SQL). All three scan gates are red-tested (`cargo test -p xtask`) and fail
  closed on a missing dir.
- **`check-registry-drift`** — fetches upstream `agents.ts` (vercel-labs/skills) and diffs it
  against the baked registry; re-syncing is a deliberate human decision, which is why this stays
  out of the automated gates. It reads names/dirs only — detect-body changes are a known blind
  spot.
- **`ci`** — fmt, clippy, doc, the three drift gates, check-arch. Not covered:
  `cargo test --workspace` (needs `DATABASE_URL`), `cargo deny check`, the sqlx offline-metadata
  job.
