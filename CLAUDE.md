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

> **Status — early scaffold (contracts frozen; trust kernel complete).** The boundary contracts are frozen and
> schema-generated (the `--json` envelope, the outcome/receipt/error/action-code shapes, the closed
> signature-alg + signed pointer, all 12 per-verb `data` payloads, and the four load-bearing client documents
> — sync/lock/map/op), with golden `--json` fixtures (pull/list/diff/publish) validated positive **and**
> negative against the schemas. The pure trust kernel (`topos-core`) implements the **byte-exact digest**, the
> **consent truth-table**, and the frozen **signing/commit byte-encodings** — the `commit_id` construction, the
> **Ed25519** device-op signature frame, and the JCS `current`-pointer preimage, all with verify and all behind
> known-answer vectors. Still to come: the verb *logic*, the embedded-git sidecar + large-object store, the
> plane, and the harness adapters. The remaining heavy deps (`gix`/`sqlx`/`axum`) are declared but unreferenced,
> so the build stays light.
>
> **Keep this status honest (no stale docs).** This block — and the per-folder `CLAUDE.md` "Implemented /
> Planned" lists — are *living status*: update them in the **same change** that lands, removes, or alters what
> they describe. A `CLAUDE.md` that still calls landed work "planned" (or planned work "landed") is a bug, not
> just drift. The code is the source of truth; when this summary and the tree disagree, `cargo test` + the
> crate's own `CLAUDE.md` win — fix the prose to match.

## Progressive disclosure — read the CLAUDE.md in the folder you're working in

This file is the map; each folder carries its own `CLAUDE.md` with that unit's contract. Read it when you
enter the folder:

- `crates/` — the five library crates (the trust kernel + storage + the ports).
- `bins/` — the two programs (the CLI; the plane).
- `xtask/` — codegen + the schema drift gate.
- `contracts/` — the generated, committed cross-language contract (JSON-Schema + fixtures).
- `tests/` — the integration oracle stack.

`AGENTS.md` in each folder is a symlink to that folder's `CLAUDE.md` (for agents that read `AGENTS.md`).

## Build / test / lint

```sh
cargo build
cargo test
cargo run -p xtask -- gen-schema --check   # the schema drift gate (regenerate → assert no diff)
cargo fmt --all
cargo clippy --all-targets
```

Toolchain is pinned in `rust-toolchain.toml` (stable 1.96, edition 2024). `unsafe_code` is forbidden
workspace-wide; clippy `all` = warn.

## The crate graph (acyclic)

```
topos-types  ◄── the app libs + every fixture (the shared WIRE DTOs; NOT a dep of topos-core)
topos-core   the PURE trust kernel — no I/O, no traits, no clock/RNG. Owns digest, consent, the CAS
   ▲   ▲     decision, the sync transition, diff3, Ed25519 sign-preimage + verify. Tested in-crate.
   │   ├── topos-gitstore ──► topos-core, topos-types   (gix object mechanics; the large-object store)
   │   └── topos-harness  ──► topos-core, topos-types   (the one client-side port; 3 impls)
   │
plane-store  ──► topos-core, topos-types, topos-gitstore   (the server authority: private SQL + authz + txn)
topos-plane  ──► plane-store, topos-core, topos-types      (the OSS plane: lib + thin bin)
topos        ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx / libsqlite3-sys   ◄── architectural layering
```

## Principles that constrain this code

- **One trust implementation.** Every trust decision — digest, consent, the CAS decision, the sync
  transition, diff3, the signing-preimage — is written ONCE, in `topos-core`, the only crate with no I/O.
  The plane, the CLI, the fixtures, and the tests all link it, so no second implementation can drift.
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
- Keep `topos-core` pure: no I/O, no `tokio`/`sqlx`/`axum`/`gix`/`std::fs`, no ambient clock or RNG (time is
  a `now` parameter; keys/signatures are byte parameters).
- `plane-store` keeps raw SQL + raw git reads private (`pub(crate)`); only authorized authority operations
  are public — that privacy boundary is what makes every object read go through the access check.

## License

Apache-2.0 — see `LICENSE`.
