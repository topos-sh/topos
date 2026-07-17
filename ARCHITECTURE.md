# Architecture

This is the public design doc for the Topos OSS core: the shape of the code, the trust boundaries it
enforces, and the model behind sharing a behavior byte-exactly. For how to build and contribute, see
[`CONTRIBUTING.md`](CONTRIBUTING.md); to report a vulnerability, [`SECURITY.md`](SECURITY.md).

## What this repository is

Topos is a layer for AI agents to **share their behaviors** within a team — so every agent stays current
with the same company processes. A *behavior* (a "skill") is a bundle of files (`SKILL.md` + scripts +
reference docs); the **whole bundle** is the unit of trust.

The repository holds three programs — two in an Apache-2.0 Cargo workspace, plus a TypeScript app:

- **`topos`** — the local CLI an agent drives to add, follow, publish, and update behaviors across harnesses.
- **`topos-plane`** — the self-hostable **vault**: pure byte custody (a library plus a thin binary),
  internal-network-only.
- **the web app** (`web/`, React Router on bun) — the one public surface: sign-in, the workspace dashboard,
  the review UI, the device API, and the whole identity + directory authority.

The two Rust programs share one trust kernel, `topos-core`: the single, auditable implementation of the
byte-exact digest, the consent rule, and the sync algorithm. Nothing proprietary lives here.

## The two motions

**Distribute** — an author `publish`es a bundle, a teammate `follow`s it, and every `pull` lands the team's
`current` byte-exact (digest re-verified in), driven by a session-start hook so updates arrive per session,
never over local edits. **Contribute** — anyone `publish --propose`s a candidate, a reviewer
`review --approve`s it to `current` (PR-like), and `revert --to` rolls the team forward to older bytes.

## The crate graph (acyclic)

Every split is paid for by a real boundary — a testable-authority, platform-dependency, or storage-privacy
boundary — never a crate-per-noun.

```
topos-types   the shared wire DTOs (the --json envelope, receipts, the current-record). No logic.
topos-core    the PURE trust kernel — no I/O, no traits, no clock/RNG, no crypto. Owns the byte-exact
   ▲   ▲      digest, the consent truth-table, the sync-state transition, the diff3 merge policy, and the
   │   │      content-addressed commit-id derivation (a version's id; no keys, nothing signs).
   │   │      Every trust invariant is a unit/property test here.
   │   ├── topos-gitstore ──► topos-core     (git object mechanics; diff/diff3 execution; large objects)
   │   └── topos-harness  ──► topos-core, topos-types   (the client-side harness port + its impls)
   │
plane-store   ──► topos-core, topos-types, topos-gitstore    (the vault's byte-custody boundary: private SQL + txn)
topos-plane   ──► plane-store, topos-core, topos-types       (the OSS vault: lib + thin bin)
topos         ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx   ◄── enforced architectural layering
```

Heavy dependencies are placed deliberately and enforced by `cargo xtask check-arch`: `sqlx` lives in
`plane-store` only (kept out of the client build), `axum` powers the vault's HTTP server, `ureq` the
client's transport. Because the vault is pure custody, its graph cannot even name an OIDC/OAuth client, an
HTTP client, or a mailer — check-arch asserts a default build resolves none of them.

## Trust boundaries — the load-bearing invariants

- **One trust implementation.** Every content and consent decision is written once, in `topos-core`, the only
  crate with no I/O and no crypto; the plane, the CLI, the fixtures, and the tests all link it, so no second
  implementation can drift.
- **The client is never an authority.** `topos` takes no dependency on `plane-store`, `sqlx`, or a SQL
  driver. It is a thin sync tool; the dependency graph enforces this at build time.
- **The vault is a library, composed — not a framework with holes.** `topos-plane`'s lib exposes clean
  custody operations plus a `router(state)` builder, with no extension or callback hook. A separate product
  can compose this library, but the custody logic is never reimplemented and never bypassed.
- **Disclosure and integrity, not a second permission system.** Nothing lands that was not disclosed and
  pinned; how much a human sits in the loop is the harness's job. A followed behavior runs with your harness's
  permissions, so Topos proves provenance and consent, not that the contents are safe to run.

## The consent + sync model

- **Content-addressed versions.** A version's identity is a plain sha256 over the raw bytes of every file —
  no normalization; `<skill>@<hash>` pins the exact bytes, which the client re-hashes and matches on every
  apply, so tampering or corruption in transit or storage is a loud integrity error. `current`, the one
  movable pointer a team follows, is a plain record whose authority is the database row behind it.
- **Four-state client transition.** On pull, the client computes where it is (current, behind, diverged, or
  conflicted) and applies with an **atomic directory swap** — all-or-nothing, never a half-written bundle. A
  diverged local draft is resolved with a three-way (diff3) merge and surfaced, never silently overwritten.

## Trust, deliberately boring

Topos extends the same trust a team already gives its git host and CI, and nothing more. There is **no
signing** in the system: no signed pointers, no key pinning, no client-side signature verification, no
anti-rollback cryptography — a follower trusts the plane it enrolled with as it trusts the git remote it
clones from. The accepted consequence is plain: a compromised server can distribute bad content, the risk
every team already lives with on its source host. Assurance is **visibility** — inspectable history, durable
receipts, a one-command revert — and content addressing keeps optional signing open as a later layer.

## The two tiers: app and vault

Authority splits cleanly across the front tier (the web app) and the back tier (the vault).

**The web app is the front tier and the only public surface.** It owns identity and the whole directory in
its own Postgres schema, `web`: people (`user`, `session`, `account`), **seats** (workspace membership, keyed
by `user.id`), devices and the enrollment flow, invitations, the bundle catalog (each row carrying a `kind`
tag — `skill` today — that clients display but never branch on), channels (including the implicit default
`everyone` channel with per-person opt-out), subscriptions (one stance row per person per bundle), detachment
records, notices, proposals and comments, op receipts, and the audit trail. Every authorization is decided
here: `app/lib/auth/guards.server.ts` mints **branded actors** (`requireSession → requireMember →
requireWorkspaceOwner`/`requireReviewer`, and `requireDeviceActor` for the device lane), and every function
in the data-access layer (`app/lib/db/queries.server.ts`) requires an actor as its first argument, so a route
that skipped its guard cannot even compile a query. Misses render **404, never 403**.

**The vault (`topos-plane`, over `plane-store`) is the back tier: pure byte custody.** Its Postgres schema,
`plane`, is a content-addressed **`version`** index, a single-generation compare-and-set **`current_pointer`**
per bundle, and the object upload/lifecycle bookkeeping — nothing else. It runs internal-network-only, has
exactly one caller (the app), authenticated by an internal bearer, and treats every request as
pre-authorized; the attribution it stores is pass-through display text the request carries. There is **no
identity table, route, or ceremony below the app boundary.** Two `check-arch` gates pin this: an
identity-vocabulary gate (no identity word appears in the vault at all) and a schema-boundary gate (the
vault's SQL names no app-schema table).

**The database posture** is two roles, one per application, each owning its schema — `scripts/compose-init-db.sh`
provisions them at first boot. The app reads the vault's `plane` schema **read-only** (an `ALTER DEFAULT
PRIVILEGES` grant, so future vault tables arrive already readable) for its history/fleet/freshness pages; the
vault cannot read `web` at all. `scripts/check-db-grants.sh` proves the cross-lane shape by logging in as each
role.

**The device flow** is a GitHub-style approval. `topos follow <workspace-address>` (or `topos auth login`)
prints "open `<origin>/verify` and enter AB12-CD34"; the signed-in person approves it behind a password
re-entry (step-up), and approval mints the device — owned by that person — and its **one bearer credential**.
The credential is stored only as its SHA-256 on the device row and presented as `Authorization: Bearer` on the
device lane (`/api/v1/…`), which the app serves itself: `requireDeviceActor` resolves credential → device →
person → seat in one query, fail-closed, so a revoked device or an unseated person loses access the instant
the row commits. Revocation is self-service, immediate, and final (trigger-enforced).

**Delivery and entitlement live app-side.** What a person should have is one predicate over the directory
rows — `((default channels − opt-outs) ∪ member channels ∪ direct follows) − unfollows`, active bundles only
— computed in `web` (`entitledBundlesSql`). Reads gate on *entitled ∧ has-a-current-pointer*; no object is
served by bare hash, and every not-entitled or not-found case returns the same **404**, so the read surface is
no oracle for which skills exist. Only the byte and pointer ops of a publish-family verb (ingest, the
`current` compare-and-set, revert, purge, the verified object reads) forward to the vault, over one allowlisted
transport (`app/lib/plane/client.server.ts` → the vault's `/internal/v1` custody lane).

### Inside the vault: the pointer move and the lifecycle fence

The custody mechanics are the vault's alone. A `current` move — publish, genesis create, revert, or an
approved proposal — runs as one `SERIALIZABLE` transaction with bounded retry: a compare-and-set on the
pointer's single `generation` (one winner, one honest `CONFLICT`, never a lost write), a same-bundle
first-parent lineage check on a publish, and a commit that writes the version rows before the pointer advances.
An idempotent-CAS rule makes app-side crash retries safe with zero extra state: a pointer already sitting one
past `expected` and naming the exact target answers success (`replayed`) instead of conflicting.

Bytes move through a database-authoritative lifecycle: ingest into a quarantine (the vault re-hashes — no
client id is trusted), a promotion lease taken **before** any byte moves so the GC keep-set protects every
object a candidate needs, the durable install, and a mark-then-acquire garbage collector whose keep-set is
exactly the read surface (a non-purged version's object edges, or a live lease). A purge tombstones the
version row (the hash stays), denylists the blobs unique to it, and reclaims the bytes; the pointer move, the
lease, and GC coordinate through guarded compare-and-swaps, so a crash leaves recoverable state, never a
half-written bundle. Every byte is re-verified against the id that named it on read; corruption is a typed
integrity fault, never folded into the uniform 404.

## Storage

The system keeps its state in three places.

- **The app's `web` schema** (Postgres) — identity and the whole directory: people, seats, devices,
  invitations, the catalog, channels, subscriptions, proposals, notices, receipts, and the audit trail.
  Reached only through the data-access layer, keyed by `user.id`.
- **The vault's `plane` schema** (Postgres) — custody only: the content-addressed version index, the
  `current` pointer, and the upload/object-lifecycle bookkeeping. Raw SQL and raw git reads are private to
  `plane-store`, so no code outside it can run an unbound query or read a bare object.
- **A git object store + a size-routed large-object store** — content-addressed bundle bytes on the vault's
  disk, verified on read (re-hashed to their id) and managed by the lifecycle fence above; larger blobs are
  offloaded to the local filesystem beside the git store. (An S3-compatible remote backend is planned and
  additive.)

## Contracts (generated, never hand-written)

The cross-language contract lives in `contracts/`: a JSON-Schema per wire type, per verb payload, and per
persisted document; golden `--json` fixtures validated both positively and negatively; and the plane's
OpenAPI. All of it is **generated** from the Rust types by `xtask` and **drift-gated** in CI — so other
languages can depend on the wire without depending on the Rust.

## Harness adapters

A harness is *which directories to read and write* plus *when a update check fires* — no dialect
translation in the OSS core (bytes sync exactly within a harness family). The `HarnessAdapter` port in
`topos-harness` is the one client-side seam; the **Claude Code** adapter is the reference implementation
(discovery, adopt-in-place, an idempotent session-start auto-update hook, clean uninstall). Adding a harness is
a directory map plus a auto-update trigger, not a refactor.

## The gates

`cargo xtask ci` runs the full non-database gate in CI's order: `fmt --check`, `clippy -D warnings`, `doc -D
warnings`, the schema / fixture / OpenAPI drift gates, and `check-arch` — which enforces the crate layering,
the leaf-crate leanness, the vault's **identity-vocabulary** and **schema-boundary** gates (the vault names no
identity word and no app-schema table), and the toolchain/Docker pin pair. Around it, CI runs the
Postgres-backed test suite, `cargo-deny`, a **compose smoke** that drives the whole first-boot story, the
cross-lane **grants-shape** check (`scripts/check-db-grants.sh`, probed by logging in as each role), and the
web app's own gates (`bun run check`: the trust-boundary, email-authorization, design-token, and
generated-contract checks, plus a built-bundle scan). A tagged release reuses the exact same gates first.

## Directory map

The repo is `crates/` (the five libraries), `bins/` (the two Rust programs), `web/` (the product app — the
public surface plus the identity + directory authority), `contracts/` (the generated cross-language
contract), `xtask/` (codegen + the invariant gates), `tests/` (the loopback-HTTP e2e suites), and `scripts/`
(the installer, the compose init/smoke, the DB grants-shape check). Each folder carries a `CLAUDE.md`
(symlinked as `AGENTS.md`) with that unit's contract; the root `CLAUDE.md` is the map.
