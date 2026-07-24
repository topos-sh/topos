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
cargo xtask gen-cli-ref            # (re)generate docs/cli.md + skills/topos/reference.md from the client's real clap tree
cargo xtask gen-cli-ref --check    # the cli-reference drift gate over BOTH copies (stale / missing fails)
cargo xtask check-arch             # the architectural-layering + vocabulary + schema-boundary gate
cargo xtask check-registry-drift   # OPT-IN + advisory: diff the baked harness registry vs upstream agents.ts (network; NEVER in ci/CI)
cargo xtask ci                     # ALL the non-DB gates, in CI's order, failing fast
cargo xtask conformance            # the store matrices (not yet implemented — prints so and exits 0)
```

## The subcommands

- **`gen-schema [--check]`** — reads the wire types from `topos-types` and emits one JSON-Schema per
  top-level wire type, per-verb `data` payload, and persisted client document into
  `contracts/schemas/`; the SAME run also generates (or `--check`s) the OpenAPI
  `contracts/openapi/openapi.json` from `topos_plane::openapi()` (the PUBLIC session lane the product
  app serves — contract stubs; the vault's internal custody lane stays out of the committed
  contract). **The artifacts are generated — never hand-edit them.**
- **`gen-fixtures [--check]`** — builds the golden `--json` envelopes FROM the typed shapes and
  writes them under `contracts/fixtures/json/`; `--check` is the drift gate.
- **`gen-cli-ref [--check]`** — writes (or `--check`s) TWO committed copies of the same bytes:
  `docs/cli.md` and the public skill's `skills/topos/reference.md` (skill installers download the
  committed file straight from the repo, so it is drift-gated like the doc). The RENDERER lives in
  the client lib (`topos::cli_ref_md()` — rendered from the real clap tree, `topos::cli_command()`),
  because it has a THIRD consumer: the built-in `topos` skill places the SAME bytes as its
  `reference.md` at placement time — one implementation, so no copy can drift from what the binary
  parses. xtask keeps only the file-write/byte-compare driver.
- **`check-arch`** — the dependency-graph + source-scan trust claims as one gate:
  - the client (`topos`) carries no `plane-store` / `sqlx` / async-runtime / HTTP / contract-derive
    edge; the kernel (`topos-core`) carries no wire DTOs or IO stacks; the leaf crates stay lean;
  - the vault (`topos-plane`) cannot even NAME the identity-era stacks (`oauth2` / `openidconnect` /
    `reqwest` / `lettre`), and `plane-store` carries no `uuid` / `ed25519-dalek` / `lettre` /
    OAuth edge;
  - the test-only `test-fixtures` features stay OFF in the production graphs;
  - every workspace member opts into the shared `[workspace.lints]`; the toolchain pins agree;
  - **the custody-vocabulary gate**: the word `skill` appears NOWHERE in the vault
    (`crates/plane-store/src` + `crates/plane-store/migrations` + `crates/topos-gitstore/src`;
    every file, any case);
  - **the identity-vocabulary gate**: none of the identity stems (`email`, `principal`, `invit…`,
    `claim`, `enroll`, `passcode`, `session`, `roster`, `seat`, `user`) appears in the same dirs —
    matched case-insensitively as word-ish identifier parts (`reclaimed` does not trip `claim`),
    with a SHORT explicit `(file, token)` allowlist for genuine non-identity uses (the gitstore's
    git-committer signature plumbing; the Postgres `idle_in_transaction_session_timeout` GUC).
    Prefer renaming code to allowlisting it (the GC's old `claim` vocabulary became `acquire`);
  - **the schema-boundary gate** (`check_seam`): no app-schema table name follows a
    FROM/JOIN/INTO/UPDATE token in any SQL string under `crates/plane-store/src` — the vault's own
    tables (`version`, `current_pointer`, `upload`, …) are the only ones its SQL may touch.
  All three scan gates are **red-tested** (`cargo test -p xtask` drives each scan over a violating
  temp tree and asserts it fires, plus a real-tree clean run) and fail closed on a missing dir.
- **`check-registry-drift`** — an OPT-IN, advisory check (NOT a `ci` gate, NEVER run in CI): it
  FETCHES the current upstream `agents.ts` from vercel-labs/skills over HTTPS at runtime and diffs it
  against `topos_harness::registry::known_harnesses()` — reporting rows missing locally, rows gone
  upstream, and per-row project/global-dir mismatches, and exiting nonzero on any drift so a human
  notices. Re-syncing the baked table is a deliberate human decision (an agent's dirs are load-bearing
  for discovery), which is why this stays out of the automated gates. It's a lightweight line parse of
  the TS, not a real parser: it reads each entry's `name`/`skillsDir`/`globalSkillsDir` and skips the
  `detectInstalled` bodies, so a detect-only upstream change is a known blind spot — skim `agents.ts`
  by eye on a real re-sync. (Uses `ureq`, the workspace's blocking transport.)
- **`ci`** — the contributor's pre-push loop: fmt, clippy, doc, the drift gates (schema / fixtures /
  cli-ref), check-arch. Not covered: `cargo test --workspace` (needs `DATABASE_URL`),
  `cargo deny check`, the sqlx offline-metadata drift job.
- **`conformance`** — a stub; prints "not yet implemented".
