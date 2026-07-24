# Topos — the OSS repo (the `topos` CLI + the self-hostable plane + the web app)

Topos is a layer for AI agents to share their **behaviors** within a team or organization — so every agent
stays current with company processes and everyone gets a consistent experience. A *behavior* (a "skill")
is a bundle of files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

**This repository is three programs — two in one Apache-2.0 Cargo workspace, plus a TypeScript app:**

- **`topos`** (`bins/topos`) — the local CLI an agent drives non-interactively to add, publish, and
  update behaviors across harnesses (Claude Code, OpenClaw, Hermes).
- **`topos-plane`** (`bins/topos-plane`) — the self-hostable sharing server (a library + a thin binary).
- **`@topos/web`** (`web/`) — the product web app (React Router, bun): sign-in, device approval, the
  workspace dashboard, the rendered review UI. Its own toolchain and gates; see `web/CLAUDE.md`.

The two Rust programs share one trust kernel (`topos-core`) — the single, auditable implementation of the
byte-exact digest, consent, content-addressed identity, and sync algorithm. Nothing proprietary lives here.

## Status — real but early (living status)

Both loops work **end-to-end today**, proven by loopback-HTTP e2e tests: **distribute** (an author
publishes; a teammate's `topos login` by workspace address arms the harness auto-update trigger, the folder's
`topos.toml` manifest records the demand, and every subsequent `update` lands the team's `current`
byte-exact) and **contribute** (`publish --propose` → a four-eyes `review --approve` → receivers apply;
`revert --to` rolls the team forward to older bytes) — plus a
self-hostable compose stack, a checksummed installer, and a tag-triggered release pipeline. Deferred,
honestly: TLS terminates at a reverse proxy (the app serves plain HTTP behind it) and the large-object store's S3-compatible remote backend.
The per-area detail lives in the owning `CLAUDE.md`s:

| Area | Current state (one line) | Detail |
|---|---|---|
| Wire + persisted contracts | Frozen + generated: JSON-Schema per wire type / verb payload / persisted doc (incl. the unsigned `WireCurrentRecord` pointer body + the adopted describe/apply payloads + the member-lane wire bodies), golden `--json` fixtures (validated positive **and** negative), the plane OpenAPI, and the generated CLI reference `docs/cli.md` (rendered from the real clap tree) — all drift-gated. | `contracts/CLAUDE.md` |
| Trust kernel | Complete + pure (no_std, no I/O): byte-exact digest + reject rules, consent truth-table, the content-addressed `commit_id` derivation (a version's id; no keys, nothing signs), the four-state sync transition (no floor, no alarm — the served pointer is the sync target, re-verified by digest on apply), the author-merge policy. | `crates/topos-core/CLAUDE.md` |
| Git object layer + large objects | Built: verify-on-read object mechanics (bundle-generic — one bare repo per bundle for the client, one per workspace for the plane; it never asks what a bundle is), diff/diff3 execution (pinned engines), the lifecycle-fence byte primitives, per-version durability batches, the size-routed local large-object store. | `crates/topos-gitstore/CLAUDE.md` |
| Harness adapters | The `HarnessAdapter` port + its three impls: the **Claude Code reference** (discover, adopt-in-place, idempotent `settings.json` SessionStart hook — matcher-free + `async`, so it fires on every source without blocking; the quiet sweep answers `reloadSkills` so pulled skills go live same-session; clean uninstall) plus **Hermes** (session-boundary `on_session_start`/`on_session_reset` entries, probed against a real local build; consent-evidence-honest) and **OpenClaw** (native SKILL.md delivery into its default-watched skills root + a silent 1-minute cron trigger registered through its own CLI, declaration-key-idempotent, container-probed live against 2026.7.1; Active only on a successful gateway round-trip; the retired bootstrap-inject artifacts are scrubbed). One byte-stable sweep everywhere: `topos update --quiet`, self-throttled client-side. The crate also carries the baked ~73-harness registry (detection + skills-root resolution), the shared-dir **coverage** module (`~/.agents/skills` support per harness, with live-probed vs vendor-docs provenance; no evidence = not covered), and the one placement-naming discipline every target dir follows. | `crates/topos-harness/CLAUDE.md` |
| Server authority (`plane-store`) | Built, Postgres-only — **PURE BYTE CUSTODY** now (the directory left the vault for the app in the identity refactor). Content-addressed **bundles** (bundle-generic — a catalog `kind` names what a bundle is, app-side), their versions, the per-workspace object store, and the ONE movable `current` pointer per bundle: quarantine/lease/GC lifecycle-fence ingest (server rehash — no client id trusted) + recovery/janitor, the SERIALIZABLE single-`generation` CAS pointer-move (idempotent-`replayed` on a crash retry; a same-bundle first-parent lineage fence on publish), the forward-commit `revert`, `purge` (tombstone the version row — the hash stays — denylist its unique blobs, reclaim), and verified reads (NEVER by bare hash; corruption is a typed `Integrity` fault, never folded into the uniform 404). It holds NO identity, membership, or policy row, treats every request as PRE-AUTHORIZED (authorization/protection/entitlement decided app-side, once), and stores attribution as pass-through display text — pinned by TWO `check-arch` gates (the identity-vocabulary gate + the schema-boundary gate). ONE caller: the composing product app, over the internal lane. | `crates/plane-store/CLAUDE.md` |
| HTTP plane (lib + thin bin) | Built: a thin `axum` lib + bin. `router(state)` = `GET /healthz` (unauth liveness) + the bearer-gated **internal custody lane** `/internal/v1/…` (ingest a candidate; `publish`/`pointer`/`revert` with the `expected_generation` CAS; current/version/object/log reads; version `purge`; bundle/workspace drop) — lane-local snake_case DTOs, deliberately NOT in `topos-types` or the committed OpenAPI. `PlaneState::with_internal_token` arms the lane (sha256-only retention; unarmed, every route answers the uniform 404); `GET /internal/v1/storage` reports per-workspace stored bytes (present-object accounting) for a composing app's oversight surfaces. `routes/door.rs` carries contract-ONLY `#[utoipa::path]` stubs for the PUBLIC device lane the web app serves, so `openapi()` still describes that wire. `spawn_maintenance` runs recovery/janitor/per-workspace GC. The leak-free `PlaneConfig`/`PlaneState::open` composition seam. NO subcommands, no trust logic, NO published port — internal-network-only with ONE caller. | `bins/topos-plane/CLAUDE.md` |
| Client CLI | Built on the MANIFEST architecture: a scope IS a `topos.toml` manifest (nearest wins, walking up like git; server-stored per-workspace PROFILES + the local personal manifest complete the layers; `[exclude]` is the one negative state) and a SESSION (user × workspace × installation, `topos login`'s RFC-8628-shaped browser approval → ONE workspace-scoped bearer credential in `identity/sessions.json`, `0600`) is the standing acceptance — delivery is silent from login on, npm-style; `logout` is the self-end (`DELETE /v1/session`), an owner-ended session prints ONE typed line and freezes. `add` is source-polymorphic (workspace refs `@ws/name` / `@ws/channels/x` / bare catalog names edit the nearest manifest — or the `-g` profile route — and deliver in the same invocation; a path adopts in place; `owner/repo` imports at a pin; `add topos` restores the built-in) and `remove` is its inverse (exclude line when a broader layer still provides; two-phase only where loss is real). The bare `update` is the MANIFEST RECONCILE: one delivery call per live session, layers resolved per scope, per-scope placement (person = shared-dir-first home engine; project = in-checkout dirs + the idempotent `.git/info/exclude` line), undemanded items cleaned snapshot-first, the applied snapshot reported per session, and a delivery cache (`state/sync_status.json`) keeping `status`/`list` honest offline. The write verbs ride op-WAL idempotent retry through per-session transports; a landed `publish` of a path-ref item TRANSFERS GOVERNANCE by default (catalog entry per protection; the manifest line rewritten to the canonical workspace ref, disclosed); `review`/`protect`/`invite`/`revert`/`log` resolve over the session universe; `auth status` probes each session's health; the offline `status` verb is the trust rail (sessions · this folder's manifests · triggers). The BUILT-IN `topos` meta-skill (source `skills/topos/`, embedded, force-synced, `remove topos --yes` opt-out / `add topos` restore) and the `self-update` + passive version check maintenance surface complete it; the whole surface is documented by the generated `docs/cli.md`. | `bins/topos/CLAUDE.md` |
| End-to-end proof | COMPOSED-STACK suites green (the real web app spawned in front of an in-process vault with no public face; the CLI dials the app's `/api` base; every ceremony driven over HTTP — the claim page, the `/verify` login approval, the settings/sessions actions): the SESSION + MANIFEST hero (login → the governance-transferring genesis publish → a project-manifest ref delivering byte-exact into a checkout with its `.git/info/exclude` line → silent fast-forward → protect/downgrade/review-approve → the `-g` profile lane → the owner-side session end printing once and freezing), the deny + logout arms, and the NON-ORACLE suite (eight byte-identical uniform-404 misses across foreign/never-existed workspaces, wrong paths, garbage credentials; the `session_approval` knob's born-pending grant with exactly two typed answers until the owner approves). The manifest-layer resolution, reference grammar, and reconcile arms are unit-proven in `bins/topos` over fakes. | `tests/CLAUDE.md` |
| Web app | Built (real but early): the product app on React Router 8 (bun) is THE one public surface AND the authority for identity + the whole directory (schema `web`, keyed by `user.id`; email never authorizes). Sign-in (email+password; the magic-link rung when SMTP is armed), the gh-style login-approval flow (`/verify` — a plain signed-in accept minting the CLI SESSION: user × workspace × installation, workspace-scoped bearer credential), the workspace dashboard, the skill pages (+ the `skills/import` add-from-GitHub flow and the skill-settings Upstream panel), the `/profile` editor (the person's server-stored include/exclude rows over the default-channel baseline), the full rendered review UI (approve/reject/withdraw with four-eyes, comments, one-click revert), and the ADMIN surfaces (roster · lifecycle ceremonies · the tabbed channel pages · policy incl. the `registration` and `session_approval` knobs · the Sessions tab (live sessions + applied state, the pending approve/reject queue, owner remove) · the account Your-sessions list with self end · the first-run claim) — every ceremony confirms in proportion to its reach (an `admin_event` row per attempt). **THE SESSION LANE `/api/v1` is served here and TERMINATES here** — the reads and row ops are Drizzle queries on the app's own `web` schema behind the session guard (Bearer → live `cli_session` → person → seat, hash computed IN Postgres); the login flow at `/api/v1/login/*`, the self-end at `DELETE /api/v1/session`, the profile rows at `/api/v1/workspaces/{ws}/profile*`; only the byte/pointer ops of a publish forward to the internal-only vault through the ONE custody transport (`vaultFetch` → `/internal/v1`, a route allowlist); a miss answers the uniform wire 404 (a PENDING session gets exactly two typed answers: me + delivery), a belt wears the 429. First boot mints the workspace + prints the claim link (single-tenant only); registration is COMPOSITION-owned — the OSS default stays gated. ONE mail transport with the metadata-only `mail_event` send log. Four composition seams + the `tenancy` URL grammar for a downstream superset. Gated by biome/tsc + the boundary/email/token/contract checks + the built-bundle scan, the unit suite, and the Playwright e2e. | `web/CLAUDE.md` |
| Gates + packaging | `cargo xtask ci` = the non-DB CI gates in order; `check-arch` enforces the crate layering, the leaf-crate leanness, the vault's identity-vocabulary + schema-boundary gates (no identity word, no app-schema table in the vault), and the Dockerfile/toolchain pin pair; the compose stack ships the WHOLE product (the web app as the one public surface on :3000; the vault internal-network-only with NO published port; Postgres with the two per-app roles + schemas provisioned at initdb by `scripts/compose-init-db.sh`) and `scripts/compose-smoke.sh` PROVES the shape (no plane host port; the constant card carries the `/api` base; the FIRST document request renders 200 with no retry — the web schema migrates eagerly at boot; the first-boot claim seats an owner whose dashboard reads custody across the schema boundary); `scripts/check-db-grants.sh` proves the cross-lane grants by LOGGING IN as each role; the checksummed echo-then-match installer and the tag-triggered release pipeline (`xtask dist`) ship the rest — with ARMED minisign release signing (the compiled-in `RELEASE_PUBKEY` makes self-update signatures mandatory; CI signs via the `MINISIGN_SECRET_KEY` environment secret; the key ceremony was `scripts/mint-release-key.sh`, the scheme `docs/RELEASE.md`). CI runs the Rust gates + suites, the grants-shape check, the compose smoke, and the web job (checks, vitest, Playwright). | `xtask/CLAUDE.md`, `README.md` |

**Still to come:** the large-object store's **S3-compatible remote backend + online backfill** (additive, client-invisible); **SSO breadth** (managed multi-IdP / SAML / SCIM — a downstream composition supplies OIDC/social rungs; the OSS default rung is email+password); **magic-link** as a primary rung; **in-place credential rotation** (the device credential has NO expiry by decision — per-device revoke + re-enrollment IS the rotation); a **credential-authenticated governance route** over the policy the web app sets today; the **audit outbox**; the per-pilot **exact-build verification discipline** (all three adapters are probed — Claude Code against current docs, Hermes against a real local build, OpenClaw live in a container against 2026.7.1 — but each pilot's exact build stays a named MUST-VERIFY; a failed probe degrades reports, never rebuilds an adapter); and harness *selection* in the client's composition root (v0 constructs Claude Code only; the TTY receipt copy already branches on the report's `CurrencyKind`). TLS terminates at a reverse proxy (the app serves plain HTTP behind it) — there is no built-in ACME path.

**Keep this status honest (no stale docs).** This table — and the per-folder `CLAUDE.md` "Implemented /
Planned" lists — are *living status*: update them in the **same change** that lands, removes, or alters what
they describe. A `CLAUDE.md` that still calls landed work "planned" (or planned work "landed") is a bug, not
just drift. The code is the source of truth; when this summary and the tree disagree, `cargo test` + the
crate's own `CLAUDE.md` win — fix the prose to match. Shipped-increment *narrative* belongs in the commit
history, never re-accreted here.

## Progressive disclosure — read the CLAUDE.md in the folder you're working in

This file is the map; each folder carries its own `CLAUDE.md` with that unit's contract. Read it when you
enter the folder:

- `crates/` — the five library crates (the trust kernel + storage + the ports).
- `bins/` — the two Rust programs (the CLI; the plane).
- `web/` — the product web app (TypeScript / React Router on bun; its own gates + suites).
- `xtask/` — codegen + the invariant gates (`ci`, `check-arch`, the drift gates).
- `contracts/` — the generated, committed cross-language contract (JSON-Schema + fixtures + OpenAPI).
- `tests/` — the workspace-level loopback-HTTP e2e suites.

`AGENTS.md` in each folder is a symlink to that folder's `CLAUDE.md` (for agents that read `AGENTS.md`).

## Build / test / lint

```sh
cargo build
cargo test           # requires a Postgres via DATABASE_URL (see below)
cargo xtask ci       # ALL the non-DB CI gates, in CI's order: fmt --check, clippy -D warnings,
                     # doc -D warnings, gen-schema --check, gen-fixtures --check, gen-cli-ref --check,
                     # check-arch
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
   ▲   ▲     transition, diff3 policy, the content-addressed commit-id derivation. Tested in-crate.
   │   ├── topos-gitstore ──► topos-core   (gix object mechanics; the large-object store)
   │   └── topos-harness  ──► topos-core, topos-types   (the one client-side port; the three harness impls)
   │
plane-store  ──► topos-core, topos-types, topos-gitstore   (the vault's byte-custody boundary: private SQL + txn)
topos-plane  ──► plane-store, topos-core, topos-types      (the OSS vault: lib + thin bin)
topos        ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx   ◄── architectural layering
```

Heavy-dependency placement, enforced by `cargo xtask check-arch`: `sqlx` is referenced by `plane-store`
only (and kept out of the client build); `axum` powers the OSS vault's HTTP server, `ureq` the client's
blocking transport. Outbound MAIL is the web app's alone (nodemailer over bring-your-own SMTP — the vault holds no mail transport). Because the vault is pure custody, its graph cannot even name an OIDC/OAuth client, an HTTP client, or a mailer — a production-tree check asserts a default build resolves none of them.

## Principles that constrain this code

- **One trust implementation.** Every trust decision — digest, consent, the sync transition, diff3, the
  content-addressed identity derivations — is written ONCE, in `topos-core`, the only crate with no I/O. The plane, the CLI,
  the fixtures, and the tests all link it, so no second implementation can drift. (Named exception, for
  now: the `(epoch,seq)` compare-and-set *decision* lives in `plane-store`'s SQL — its kernel extraction
  is on `topos-core`'s own planned list.)
- **The client is never an authority.** `bins/topos` takes no dependency on `plane-store`, `sqlx`, or a SQL
  driver — it is a thin sync tool. The dependency graph enforces this.
- **The plane is a library, composed — not a framework with holes.** `topos-plane`'s lib exposes clean
  custody operations + a `router(state)` builder; it has **no** extension/callback hook. (A separate
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
- `plane-store` keeps raw SQL + raw git reads private (`pub(crate)`); only the custody operations
are public — that privacy boundary is what keeps every object read behind verify-on-read.

## License

Apache-2.0 — see `LICENSE`.
