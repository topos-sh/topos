# Architecture

This is the public design doc for the Topos OSS core: the shape of the code, the trust boundaries it
enforces, and the model behind sharing a behavior byte-exactly. For how to build and contribute, see
[`CONTRIBUTING.md`](CONTRIBUTING.md); to report a vulnerability, [`SECURITY.md`](SECURITY.md).

## What this repository is

Topos is a layer for AI agents to **share their behaviors** within a team — so every agent stays current
with the same company processes. A *behavior* (a "skill") is a bundle of files (`SKILL.md` + scripts +
reference docs); the **whole bundle** is the unit of trust.

The repository is two programs in one Apache-2.0 Cargo workspace:

- **`topos`** — the local CLI an agent drives to add, follow, publish, and update behaviors across harnesses.
- **`topos-plane`** — the self-hostable sharing server (a library plus a thin binary).

They share one trust kernel, `topos-core`: the single, auditable implementation of the byte-exact digest,
the consent rule, and the sync algorithm. Nothing proprietary lives here.

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
   │   │      content-addressed identity derivations (commit id, device key id, canonical principal).
   │   │      Every trust invariant is a unit/property test here.
   │   ├── topos-gitstore ──► topos-core     (git object mechanics; diff/diff3 execution; large objects)
   │   └── topos-harness  ──► topos-core, topos-types   (the client-side harness port + its impls)
   │
plane-store   ──► topos-core, topos-types, topos-gitstore    (the server authority: private SQL + authz + txn)
topos-plane   ──► plane-store, topos-core, topos-types       (the OSS plane: lib + thin bin)
topos         ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx   ◄── enforced architectural layering
```

Heavy dependencies are placed deliberately and enforced by `cargo xtask check-arch`: `sqlx` lives in
`plane-store` only (kept out of the client build), `axum` powers the plane's HTTP server, `ureq` the client's
transport. The optional OIDC enrollment connector is feature-gated **default-off** — a production-tree check
asserts a default build resolves none of it.

## Trust boundaries — the load-bearing invariants

- **One trust implementation.** Every content and consent decision is written once, in `topos-core`, the only
  crate with no I/O and no crypto; the plane, the CLI, the fixtures, and the tests all link it, so no second
  implementation can drift.
- **The client is never an authority.** `topos` takes no dependency on `plane-store`, `sqlx`, or a SQL
  driver. It is a thin sync tool; the dependency graph enforces this at build time.
- **The plane is a library, composed — not a framework with holes.** `topos-plane`'s lib exposes clean
  authority operations plus a `router(state)` builder, with no extension or callback hook. A separate product
  can compose this library around the authority, but the authority is never reimplemented and never bypassed.
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

## The server authority: custody and directory

`plane-store` is the only crate that touches the database or reads a raw object, and it is split into two
halves along the row/byte line:

- **Custody** owns the bytes: it holds content-addressed **bundles** (a `SKILL.md` bundle today), their
  versions, the object store, and the one movable `current` pointer per bundle. It ingests candidates, holds
  them through an upload/quarantine/GC lifecycle, and moves `current` under a compare-and-set. It is **generic
  over a bundle's kind** — the directory's catalog carries a `kind` tag (`skill` today) naming what a bundle
  is, so a new kind is catalog and surface work, never a custody change. Custody knows nothing about who a
  principal is, or what a bundle is for.
- **Directory** owns identity and policy: workspaces, the roster, enrolled devices, invitations and
  enrollment, governance roles, and the review-required policy. It is the source of every authorization
  decision.

Custody never reads a directory table and never names the directory module — a build-time `check-arch` rule
enforces that one-way boundary. Instead, custody declares an **access-witness** interface that the directory
implements, and calls it *inside its own transaction*, so every authorization reflects the committed roster
at commit time. This is what makes **revocation immediate**: a `revoke` committed before a promotion is seen
by that promotion's in-transaction check and blocks it, in the same serializable window.

**The pointer move.** A `current` move — `publish`, genesis create, `revert`, or an approved proposal — runs
as one `SERIALIZABLE` transaction with bounded retry and no filesystem work inside it: an operation-id replay
probe (a retried write replays its receipt byte-for-byte, never double-applying), the witness access check, a
compare-and-set on the whole `(epoch, seq)` generation (one winner, one honest `CONFLICT`, never a lost
write), an availability and first-parent lineage check, then a commit that writes provenance before the
pointer advances and records a durable, attributed receipt for **every** outcome. Postgres does not serialize
writers on its own, so each read-then-write invariant — the CAS, the last-owner guard, the object-presence
fence — is re-proven by serializable isolation plus retry, not by a lock the caller must remember to take.

**The read model.** Every device-lane request carries **one bearer membership credential** per (workspace ×
device) — minted at enrollment, stored only as its sha256 on the device's registry row, and presented in the
`Authorization: Bearer` header (never a body field). Authorization is a single predicate: a **confirmed
workspace-member seat**, re-resolved from that trusted row — never a caller-asserted id — and re-checked in
the same transaction as the read, so a revoked device or a removed member loses access the instant the row
commits. Reads gate on *member ∧ reachable*. **No object is served by bare hash**, and every not-entitled or
not-found case returns the same indistinguishable **404, not a 403**, so the read surface is no oracle for
which skills exist. A store failure on an already-authorized object is a separate integrity fault, never a 404.

**The object lifecycle fence.** Bytes move through a database-authoritative lifecycle: ingest into a
quarantine, lease-then-install on the way to a version, and a mark-then-claim garbage collector whose keep-set
is *exactly* the read-authorization surface — a readable object is never reclaimed, and a reclaimed object
reads 404, never a false integrity fault. The pointer move, the lease, and GC coordinate through guarded
compare-and-swaps, so a crash leaves recoverable state, not a half-written bundle.

## Storage

The plane keeps three kinds of state.

- **A Postgres metadata database** — the directory plus the pointer/lifecycle bookkeeping: workspaces,
  rosters, devices, enrollment, pointers, proposals, receipts. Access goes through `plane-store`; raw SQL and
  raw git reads are private to that crate, so no code outside it can run an unbound query or read a bare
  object.
- **A git object store** — content-addressed bundle bytes, verified on read (re-hashed to their id) and
  managed by the lifecycle fence above.
- **A size-routed large-object store** — larger blobs offloaded to the local filesystem beside the git store.
  (An S3-compatible remote backend is planned and additive.)

## Contracts (generated, never hand-written)

The cross-language contract lives in `contracts/`: a JSON-Schema per wire type, per verb payload, and per
persisted document; golden `--json` fixtures validated both positively and negatively; and the plane's
OpenAPI. All of it is **generated** from the Rust types by `xtask` and **drift-gated** in CI — so other
languages can depend on the wire without depending on the Rust.

## Harness adapters

A harness is *which directories to read and write* plus *when a currency check fires* — no dialect
translation in the OSS core (bytes sync exactly within a harness family). The `HarnessAdapter` port in
`topos-harness` is the one client-side seam; the **Claude Code** adapter is the reference implementation
(discovery, adopt-in-place, an idempotent session-start currency hook, clean uninstall). Adding a harness is
a directory map plus a currency trigger, not a refactor.

## The gates

`cargo xtask ci` runs the full non-database gate in CI's order: `fmt --check`, `clippy -D warnings`, `doc -D
warnings`, the schema / fixture / OpenAPI drift gates, and `check-arch` (which enforces the layering, the
leaf-crate leanness, the custody↛directory seam, the OIDC-default-off claim, and the toolchain/Docker pin
pair). The Postgres-backed test suite, `cargo-deny`, and a compose smoke round out CI; a tagged release
reuses the exact same gate first.

## Directory map

The workspace is `crates/` (the five libraries), `bins/` (the two programs), `contracts/` (the generated
cross-language contract), `xtask/` (codegen + the invariant gates), `tests/` (the loopback-HTTP e2e suites),
and `scripts/` (the installer, the compose smoke, the ACME rehearsal). Each folder carries a `CLAUDE.md`
(symlinked as `AGENTS.md`) with that unit's contract; the root `CLAUDE.md` is the map.
