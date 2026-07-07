# Contributing to Topos

Thanks for your interest in Topos. This guide covers how to build, test, and propose changes. By
participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

- **Found a security issue?** Do **not** open a public issue — follow [`SECURITY.md`](SECURITY.md).
- **Want to understand the design first?** Read [`ARCHITECTURE.md`](ARCHITECTURE.md) and the `CLAUDE.md` in
  the folder you're working in (each one is that unit's contract).

## Ways to contribute

- **Report a bug** or **request a feature** via the issue templates.
- **Open a pull request** for a fix, a test, a doc improvement, or a feature.
- **Improve the docs** — the README, the per-folder `CLAUDE.md`s, or these guides.

For anything larger than a bug fix, please open an issue to discuss the approach first — it saves everyone a
round-trip.

## Development setup

You need a recent stable Rust and (only for the tests) a reachable Postgres.

- **Rust toolchain.** Pinned in `rust-toolchain.toml` (stable 1.96, edition 2024); `rustup` picks it up
  automatically. `unsafe` code is forbidden workspace-wide.
- **Postgres** — only the test suite needs it (compilation is offline). A throwaway one:

  ```sh
  docker run --rm -e POSTGRES_USER=topos -e POSTGRES_PASSWORD=topos \
    -e POSTGRES_DB=topos -p 5432:5432 postgres:18
  export DATABASE_URL="postgres://topos:topos@localhost:5432/topos"
  ```

## Build, test, lint

```sh
cargo build
cargo xtask ci    # the full non-DB gate sequence, in CI's order
cargo test        # requires DATABASE_URL + a reachable Postgres (above)
```

**`cargo xtask ci` is the pre-push loop** — one command that matches CI's gate job exactly: `fmt --check`,
`clippy -D warnings`, `doc -D warnings`, the schema / fixture / OpenAPI drift gates, and `check-arch`. Run
it before you push and your PR's gate job will pass. (The `xtask` alias lives in the committed
`.cargo/config.toml`.)

Compilation itself needs no database — the compile-time-checked queries read the committed
`crates/plane-store/.sqlx` metadata. Only running the tests hits Postgres, which the suite provisions per
test.

### If you change the wire types or SQL

Two things in this repo are **generated and drift-gated**, so a change there is a two-step:

- **Wire types (`topos-types`).** After editing them, regenerate the contract and commit the result:
  `cargo xtask gen-schema` and `cargo xtask gen-fixtures`. The drift gate fails if the committed
  `contracts/` output doesn't match the types.
- **SQL queries (`plane-store`).** If you add or change a compile-time-checked query, regenerate the offline
  metadata (`cargo sqlx prepare` against a live `DATABASE_URL`) and commit the `.sqlx/` change, or CI's
  offline-metadata drift gate will fail.

## Pull request process

1. Fork and branch from `main`.
2. Keep the change focused; a small PR is reviewed faster than a large one.
3. Add or update tests for behavior changes. Unit tests live inline (`#[cfg(test)] mod tests`); multi-file
   suites live in `src/tests/` or the workspace `tests/` crate.
4. Update the living docs **in the same change**: the per-folder `CLAUDE.md` status lists and, for a shipped
   increment, a `CHANGELOG.md` entry (newest first). A doc that describes the old behavior is a bug.
5. Make sure `cargo xtask ci` and `cargo test` are green locally.
6. Write commit messages that say **what changed and why**, in the imperative mood. Keep them self-contained.
7. Open the PR against `main` and fill in the template. CI must pass before merge.

Match the surrounding code's idiom, comment density, and naming — a change should read like the code around
it.

## Licensing of contributions

Topos is licensed under **Apache-2.0**. Contributions are **inbound = outbound**: when you submit a pull
request, you agree that your contribution is licensed to the project under the same Apache-2.0 license, and
you affirm you have the right to submit it under that license (this is the effect of Section 5 of the
Apache License). **There is no separate CLA to sign** — opening the PR is the agreement. Please only submit
work that is yours to contribute.

## Questions

Open a [discussion or issue](https://github.com/topos-sh/topos/issues). For anything sensitive or
security-related, use the private channel in [`SECURITY.md`](SECURITY.md).
