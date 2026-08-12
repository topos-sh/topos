# Topos — the OSS repo (the `topos` CLI + the self-hostable plane + the web app)

Topos is a layer for AI agents to share **behaviors** within a team or organization. A behavior (a
"skill") is a bundle of files (`SKILL.md` + scripts + docs); the **whole bundle** is the unit of
trust. Two motions: **distribute** (publish → every subscribed agent updates silently) and
**contribute** (anyone proposes improvements back; review gates them).

**Three programs — two in one Apache-2.0 Cargo workspace, plus a TypeScript app:**

- **`topos`** (`bins/topos`) — the client CLI an agent drives non-interactively. Full verb
  reference: the generated `docs/cli.md`.
- **`topos-plane`** (`bins/topos-plane`) — the self-hostable vault (library + thin binary): pure
  byte custody, internal-network-only, ONE caller (the web app).
- **`@topos/web`** (`web/`) — the product web app (React Router 8, bun): the ONE public surface —
  identity, the whole directory, the `/api/v1` session lane, the signed-in pages, `/docs`. Its own
  toolchain and gates.

## Map — read the CLAUDE.md in the folder you're working in

- `crates/` — the five library crates: `topos-types` (wire DTOs), `topos-core` (the pure trust
  kernel), `topos-gitstore` (git object mechanics + the large-object store), `topos-harness` (the
  harness-adapter port + impls), `plane-store` (the vault's byte-custody boundary).
- `bins/` — the two Rust programs (each a lib + a thin bin).
- `web/` — the product web app.
- `xtask/` — codegen + the invariant gates (`ci`, `check-arch`, the drift gates).
- `contracts/` — the generated, committed cross-language contract (JSON-Schema, fixtures, OpenAPI).
- `tests/` — the workspace-level composed-stack e2e suites.
- `docs/` — the MDX documentation source the web app serves at `/docs`; `docs/cli.md` is the
  generated CLI reference. `skills/topos/` — the built-in `topos` meta-skill source (embedded in
  the binary). `scripts/` — installer, compose init-db/smoke, grants check, release tooling.
  `deny.toml`, `RELEASING.md`, `docker-compose.yml` +
  `Dockerfile` — supply-chain policy, the release/signing scheme, the self-host stack.

`AGENTS.md` in each folder symlinks to that folder's `CLAUDE.md`.

## Build / test / lint

```sh
cargo build
cargo test           # requires a Postgres via DATABASE_URL (fresh DB per test)
cargo xtask ci       # ALL the non-DB CI gates, in CI's order: fmt --check, clippy -D warnings,
                     # doc -D warnings, gen-schema --check, gen-fixtures --check, gen-cli-ref --check,
                     # check-arch
```

`cargo xtask ci` is the pre-push loop (the alias lives in the committed `.cargo/config.toml`).
Compilation is offline: `SQLX_OFFLINE=true` is defaulted there, so only running tests needs a
database. Toolchain pinned in `rust-toolchain.toml` (stable, edition 2024); `unsafe_code` is
forbidden workspace-wide.

## The crate graph (acyclic)

```
topos-types  ◄── the app libs + every fixture (the shared WIRE DTOs; NOT a dep of topos-core)
topos-core   the PURE trust kernel — no I/O, no traits, no clock/RNG. Owns digest, consent, the sync
   ▲   ▲     transition, diff3 policy, the content-addressed commit-id derivation. Tested in-crate.
   │   ├── topos-gitstore ──► topos-core   (gix object mechanics; the large-object store)
   │   └── topos-harness  ──► topos-core, topos-types   (the one client-side port; the harness impls)
   │
plane-store  ──► topos-core, topos-types, topos-gitstore   (the vault's byte-custody boundary)
topos-plane  ──► plane-store, topos-core, topos-types      (the OSS vault: lib + thin bin)
topos        ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx   ◄── architectural layering
```

Heavy-dependency placement is enforced by `cargo xtask check-arch`: `sqlx` in `plane-store` only,
`axum` in the vault, `ureq` as the client's blocking transport, mail in the web app alone.

## Principles that constrain this code

- **One trust implementation.** Every trust decision — digest, consent, the sync transition, diff3,
  the content-addressed identity — is written ONCE, in `topos-core` (the only crate with no I/O).
- **The client is never an authority.** `bins/topos` is a thin sync tool; the dependency graph
  enforces it.
- **The plane is a library, composed — not a framework with holes.** No extension/callback hooks;
  a downstream product imports and composes it.
- **Contracts are generated, never hand-written.** Change the Rust types, regenerate, review the
  diff; the drift gates stay green.
- **Disclosure + integrity, not a second permission system.** Nothing lands that wasn't disclosed
  and pinned; how much a human sits in the loop is the harness's job.
- **Simplicity-first.** No new primitives without a mainstream precedent.

## Conventions

- Match the surrounding code's idiom, comment density, and naming.
- Unit tests live inline (`#[cfg(test)] mod tests`); multi-file suites in `src/tests/`.
- Keep `topos-core` pure: no I/O, no ambient clock or RNG.
- `plane-store` keeps raw SQL + raw git reads private — that boundary keeps every object read
  behind verify-on-read.
- **Every `CLAUDE.md` here describes CURRENT state only** — architecture and what lives where.
  Update it in the same change that alters what it describes; never accrete history, status
  narrative, or "what changed" (git history carries that).

## License

Apache-2.0 — see `LICENSE`.
