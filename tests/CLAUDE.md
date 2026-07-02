# `tests/` — the workspace-level end-to-end suite

One workspace member (`topos-e2e`) holding the loopback-HTTP end-to-end tests: the GENUINE client engine
(the real `ureq` transport, the real pull/write verbs) against the GENUINE plane (`topos_plane::router`
over a real `plane-store::Authority`) on a real `127.0.0.1:0` socket. Per-crate unit + generative tests
live in their crates (every trust invariant is a unit/seeded-generative test in `topos-core`); this
directory is for what only a cross-crate loopback run can prove.

## Layout (what actually exists)

- **`src/lib.rs`** — an intentionally-empty anchor so the package is a real workspace member that
  `cargo test --workspace` discovers.
- **`tests/common/`** — the shared harness: per-test Postgres provisioning (`provision_pg` creates a
  uniquely-named database on `$DATABASE_URL` and runs the production migrations) plus the loopback-plane
  scaffold every suite stands on — `Scratch` / `Plane` / `start_plane` (bind-first, optional enrollment
  config, then serve `router(state)`), the shared seeding helpers (`seed_genesis_plane`, the
  governance-signed `mint_invite`), and the placement-expectation builders. Each suite keeps only its
  scenario-specific seeding (a seed closure handed to `start_plane`). Each e2e runs a blocking
  `ureq` client on a plain thread beside a live `axum` server on a self-owned **multi-thread** runtime —
  which is why these tests cannot use `#[sqlx::test]` (its current-thread runtime would deadlock).
- **`tests/hero.rs`** — the distribute HERO: the real pull engine over loopback HTTP. First pull
  fast-forwards byte-exact (incl. the executable bit); a second is a commit-sensitive 304 no-op; a
  tampered signed pointer is refused with last-known-good retained.
- **`tests/hero_claude.rs`** — the HERO on the REAL Claude Code adapter, on real client verbs: an author
  genesis-publishes over the wire; a follower's real two-call `follow` arms the actual `settings.json`
  SessionStart hook (asserted byte-exact) into a temp stand-in `$CLAUDE_CONFIG_DIR` and lands the bundle;
  update / `revert --to` land on subsequent bare `pull` sweeps; a drafting confirm-each follower is never
  clobbered. Table-driven so a sibling harness adapter is one case row + one test. Its module doc states
  the honest ceiling: hook-installed + bytes-materialized is asserted; that a live session's hook output
  reaches model context is a documented manual MUST-VERIFY.
- **`tests/follow_e2e.rs`** — the real `topos follow` enrollment loop: invite mint → bootstrap fetch +
  TOFU key pin → device authorize → confirm → resume signs the enroll possession proof → redeem over the
  wire → the first-received bundle lands byte-exact.
- **`tests/contribute_e2e.rs`** — the client device-signed write verbs (`publish` / `review` / `revert` /
  the plane-sourced `diff`) over loopback HTTP, with a separate follower receiving the shipped bytes
  byte-exact.

## Running it

The suite **requires a Postgres** reachable via `DATABASE_URL` (each test provisions its own fresh
database; provisioned databases are left behind — point it at a disposable server/container). Keep
`SQLX_OFFLINE=true` for compilation (the committed `.cargo/config.toml` defaults it).

```sh
export DATABASE_URL="postgres://topos:topos@localhost:5432/topos"
cargo test -p topos-e2e
```

Both sides are driven through their feature-gated `test-fixtures` surfaces (the client's `test_support`,
plane-store's seed shims) — dev-dependencies of this test-only member, never enabled in a production build
(`cargo xtask check-arch` asserts it).
