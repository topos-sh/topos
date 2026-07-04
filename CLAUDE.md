# Topos — the OSS repo (the `topos` CLI + the self-hostable plane)

Topos is a layer for AI agents to share their **behaviors** within a team or organization — so every agent
stays current with company processes and everyone gets a consistent experience. A *behavior* (a "skill")
is a bundle of files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

**This repository is two programs in one Apache-2.0 Cargo workspace:**

- **`topos`** (`bins/topos`) — the local CLI an agent drives non-interactively to add, follow, publish, and
  update behaviors across harnesses (Claude Code, OpenClaw, Hermes).
- **`topos-plane`** (`bins/topos-plane`) — the self-hostable sharing server (a library + a thin binary).

They share one trust kernel (`topos-core`) — the single, auditable implementation of the byte-exact digest,
consent, signing, and sync algorithm. Nothing proprietary lives here.

## Status — real but early (living status)

Both loops work **end-to-end today**, proven by loopback-HTTP e2e tests: **distribute** (an author
publishes; a follower's real two-call `follow` arms the harness currency trigger and every subsequent
`pull` lands the team's `current` byte-exact) and **contribute** (`publish --propose` → a four-eyes
`review --approve` → followers apply; `revert --to` rolls the team forward to older bytes) — plus a
self-hostable compose stack, a checksummed installer, and a tag-triggered release pipeline. Deferred,
honestly: TLS terminates at a reverse proxy by default (an EXPERIMENTAL, default-off built-in ACME
listener exists but is unproven on a real box) and the large-object store's S3-compatible remote backend.
Delivery history lives in `CHANGELOG.md`; the per-area detail lives in the owning `CLAUDE.md`s:

| Area | Current state (one line) | Detail |
|---|---|---|
| Wire + persisted contracts | Frozen + generated: JSON-Schema per wire type / verb payload / persisted doc, golden `--json` fixtures (validated positive **and** negative), the plane OpenAPI — all drift-gated. | `contracts/CLAUDE.md` |
| Trust kernel | Complete + pure (no_std, no I/O): byte-exact digest, consent truth-table, the frozen commit/device-op/pointer/enroll/governance signing frames + the shared identity derivations, the four-state sync transition + anti-rollback floor/ALARM, the author-merge policy. | `crates/topos-core/CLAUDE.md` |
| Git object layer + large objects | Built: verify-on-read object mechanics, diff/diff3 execution (pinned engines), the lifecycle-fence byte primitives, per-version durability batches, the size-routed local large-object store. | `crates/topos-gitstore/CLAUDE.md` |
| Harness adapters | The `HarnessAdapter` port + its three impls: the **Claude Code reference** (discover, adopt-in-place, idempotent `settings.json` session-start hook, clean uninstall) plus **OpenClaw** and **Hermes** (their concrete config bytes / per-turn-injection claims stay provisional behind the pilot readiness probes). | `crates/topos-harness/CLAUDE.md` |
| Server authority (`plane-store`) | Built, Postgres-only: skill-scoped reads (404-not-403, never by bare hash), the quarantine/lease/GC lifecycle fence + recovery/janitor, the SERIALIZABLE `(epoch,seq)` CAS pointer-move with in-process Ed25519 signing + all-outcome receipts, propose → review (shared keep-set == read-surface predicate), enrollment issuance (deterministic HMAC credentials) + governance ops, and the workspace-standup genesis (the one-time claim mint/redeem with lost-200 replay, `create_workspace`, the standup session + `approve_standup` — one shared genesis seat, per-identity capped). | `crates/plane-store/CLAUDE.md` |
| HTTP plane (lib + thin bin) | Built: composable `router(state)`, the frozen read/write routes (200-for-all-outcomes writes, commit-sensitive 304 reads), enrollment + governance routes (+ default-off OIDC; `device/authorize` now intent-dispatches enroll vs standup and `/i/` serves claim links too), the admin-token policy route, in-process rate limiter, the maintenance scheduler (`spawn_maintenance`), request tracing, the backup-restore epoch bump (`restore-bump-epoch`) + the `mint-claim` subcommand, the leak-free `PlaneConfig`/`PlaneState::open` composition seam + the lib-only standup wrappers (`mint_admin_claim`/`create_workspace`/`approve_standup`/`approve_session`). | `bins/topos-plane/CLAUDE.md` |
| Client CLI (the 12 verbs) | Built: the accountless local core + crash-safe sidecar, the pull engine (anti-rollback, atomic dir-swap materialization, diff3 draft resolution, a fast-degrade circuit breaker), device key + two-call `follow`/`invite`, the device-signed write verbs with op-WAL idempotent retry, the workspace-standup client (an un-enrolled `publish` that stands the workspace up via the sign-in device flow + the one-shot `follow <claim-link>` bearer door), `INVALID_ARGUMENT`/io-kind-honest error codes, `TOPOS_DEBUG=1` + `~/.topos/log.jsonl` diagnostics. | `bins/topos/CLAUDE.md` |
| End-to-end proof | Loopback-HTTP suites green: the distribute HERO (table-driven across the Claude Code, OpenClaw, and Hermes adapters), the real `follow` enrollment, the contribute write verbs, and the backup-restore epoch-bump suite. | `tests/CLAUDE.md` |
| Gates + packaging | `cargo xtask ci` = the non-DB CI gates in order; `check-arch` enforces the layering, the leaf-crate leanness, the OIDC default-off claim, and the Dockerfile/toolchain pin pair; a stateless Docker image + compose + smoke script, the checksummed echo-then-match installer, and the tag-triggered release pipeline (`xtask dist`) ship the self-host. | `xtask/CLAUDE.md`, `README.md` |

**Still to come:** the large-object store's **S3-compatible remote backend + online backfill** (additive,
client-invisible); the **hosted verification-page HTML + cloud preview render** (the Rust completion API is
built; the page is a TS surface); **SSO breadth** (managed multi-IdP / HRD / SAML / SCIM — one generic OIDC
connector ships feature-gated); **magic-link** as a primary rung; **active read-token rotation** (per-device
revoke + expiry are built; rotation in the `current` path is deferred — v0 mints long-lived device-bound
tokens); the **device-signed `PUT /policy` variant** (the self-host admin-token route is built; a
device-op-signed governance route over the same policy needs a new kernel frame) + the client
**key-rotation-verify** (`KEY_REPIN_REQUIRED` beyond the first pin); the **workspace-standup web pages +
full-chain e2e** (the server AND client halves are built — the claim mint/redeem, the standup device flow
+ `approve_standup`, `create_workspace`, the `/i/` claim bootstrap, the client's un-enrolled `publish`
standup resume and `follow <claim-link>`; what remains is the hosted web's verify/create pages that call
the lib-only wrappers, plus the loopback full-chain standup e2e); the built-in ACME TLS path's
**real-estate rehearsal** (public DNS · Let's Encrypt staging→prod · rate limits · renewal timing — the
experimental label stands and no one-command auto-TLS claim is made until it passes; a reverse proxy
remains the documented default); the **audit outbox**; at-rest key encryption (the plane signing key +
the enrollment secret are plaintext `0600` seeds for now); the two **pilot-build readiness probes**
(both sibling adapters are built — see above; OpenClaw's concrete config bytes and Hermes's per-turn
injection + consent flow stay provisional until each pilot's exact build is probed); and harness
*selection* in the client's composition root (v0 constructs Claude Code only; the TTY receipt copy
already branches on the report's `CurrencyKind`).

**Keep this status honest (no stale docs).** This table — and the per-folder `CLAUDE.md` "Implemented /
Planned" lists — are *living status*: update them in the **same change** that lands, removes, or alters what
they describe. A `CLAUDE.md` that still calls landed work "planned" (or planned work "landed") is a bug, not
just drift. The code is the source of truth; when this summary and the tree disagree, `cargo test` + the
crate's own `CLAUDE.md` win — fix the prose to match. Shipped-increment *narrative* belongs in
`CHANGELOG.md` (newest first), never re-accreted here.

## Progressive disclosure — read the CLAUDE.md in the folder you're working in

This file is the map; each folder carries its own `CLAUDE.md` with that unit's contract. Read it when you
enter the folder:

- `crates/` — the five library crates (the trust kernel + storage + the ports).
- `bins/` — the two programs (the CLI; the plane).
- `xtask/` — codegen + the invariant gates (`ci`, `check-arch`, the drift gates).
- `contracts/` — the generated, committed cross-language contract (JSON-Schema + fixtures + OpenAPI).
- `tests/` — the workspace-level loopback-HTTP e2e suites.

`AGENTS.md` in each folder is a symlink to that folder's `CLAUDE.md` (for agents that read `AGENTS.md`).
`CHANGELOG.md` at the root is the delivery history (newest first).

## Build / test / lint

```sh
cargo build
cargo test           # requires a Postgres via DATABASE_URL (see below)
cargo xtask ci       # ALL the non-DB CI gates, in CI's order: fmt --check, clippy -D warnings,
                     # doc -D warnings, gen-schema --check, gen-fixtures --check, check-arch
```

`cargo xtask ci` is the pre-push loop — one command that matches the CI `gate` job exactly (the `xtask`
alias lives in the committed `.cargo/config.toml`). The individual gates remain runnable one at a time —
see `xtask/CLAUDE.md`.

`cargo test` requires a Postgres reachable via `DATABASE_URL` — the suite provisions a fresh database per
test (`#[sqlx::test]`). Compilation itself is offline: the committed `.cargo/config.toml` defaults
`SQLX_OFFLINE=true` (non-forced — your own environment wins), so the compile-time-checked queries read the
committed `crates/plane-store/.sqlx` and `cargo build`, `clippy`, and `doc` need no database — only running
the tests does.

Toolchain is pinned in `rust-toolchain.toml` (stable 1.96, edition 2024). `unsafe_code` is forbidden
workspace-wide; clippy `all` = warn.

## The crate graph (acyclic)

```
topos-types  ◄── the app libs + every fixture (the shared WIRE DTOs; NOT a dep of topos-core)
topos-core   the PURE trust kernel — no I/O, no traits, no clock/RNG. Owns digest, consent, the sync
   ▲   ▲     transition, diff3 policy, Ed25519 sign-preimage + verify. Tested in-crate.
   │   ├── topos-gitstore ──► topos-core   (gix object mechanics; the large-object store)
   │   └── topos-harness  ──► topos-core, topos-types   (the one client-side port; the three harness impls)
   │
plane-store  ──► topos-core, topos-types, topos-gitstore   (the server authority: private SQL + authz + txn)
topos-plane  ──► plane-store, topos-core, topos-types      (the OSS plane: lib + thin bin)
topos        ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx   ◄── architectural layering
```

Heavy-dependency placement, enforced by `cargo xtask check-arch`: `sqlx` is referenced by `plane-store`
only (and kept out of the client build); `axum` powers the OSS plane's HTTP server, `ureq` the client's
blocking transport, and `lettre` the plane's passcode mailer. The OIDC stack (`oauth2`/`openidconnect`,
with their `reqwest`) is feature-gated **default-off** in `topos-plane` — a production-tree check asserts
a default build resolves none of it.

## Principles that constrain this code

- **One trust implementation.** Every trust decision — digest, consent, the sync transition, diff3, the
  signing-preimage — is written ONCE, in `topos-core`, the only crate with no I/O. The plane, the CLI,
  the fixtures, and the tests all link it, so no second implementation can drift. (Named exception, for
  now: the `(epoch,seq)` compare-and-set *decision* lives in `plane-store`'s SQL — its kernel extraction
  is on `topos-core`'s own planned list.)
- **The client is never an authority.** `bins/topos` takes no dependency on `plane-store`, `sqlx`, or a SQL
  driver — it is a thin sync tool. The dependency graph enforces this.
- **The plane is a library, composed — not a framework with holes.** `topos-plane`'s lib exposes clean
  authority operations + a `router(state)` builder; it has **no** extension/callback hook. (A separate
  private product imports and composes this library; this repo never imports it.)
- **Contracts are generated, never hand-written.** `contracts/schemas/*.json` are generated from
  `topos-types` by `xtask`. Change the Rust types, regenerate, review the diff. The drift gate must stay
  green.
- **Disclosure + integrity, not a second permission system.** The tool guarantees nothing lands that wasn't
  disclosed and pinned (the byte-exact bundle digest is what a human approves). How much a human sits in the
  loop is the agent/harness's job — never this tool's.
- **Simplicity-first.** No new primitives without a mainstream precedent (git, npm, signed links); reuse
  existing mechanisms.

## Conventions

- Match the surrounding code's idiom, comment density, and naming.
- Unit tests live inline (`#[cfg(test)] mod tests`); multi-file suites live in `src/tests/`.
- Keep `topos-core` pure: no I/O, no `tokio`/`sqlx`/`axum`/`gix`/`std::fs`, no ambient clock or RNG (time is
  a `now` parameter; keys/signatures are byte parameters).
- `plane-store` keeps raw SQL + raw git reads private (`pub(crate)`); only authorized authority operations
  are public — that privacy boundary is what makes every object read go through the access check.

## License

Apache-2.0 — see `LICENSE`.
