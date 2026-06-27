# `plane-store` — the server authority boundary

**A crate so that raw access is private.** It owns the plane's per-workspace SQL — **raw `sqlx`, no
ORM** — and per-workspace git-object storage, and it is the single place the **skill-scoped access rule**
is enforced. The pool, every transaction, every raw SQL statement, and every raw git-object read are
`pub(crate)`-private; the **only** public surface is authorized authority operations on `Authority`.

## The privacy boundary IS the security mechanism

No code outside this crate can run an unbound query or read a bare object — that is unbypassable by
construction. (This is misuse-prevention by encapsulation; it is not isolation against malicious
same-process code.) The error type holds this line too: internal faults carry a **boxed** source, so no
`sqlx` or git-store type appears in any public signature.

## Implemented (each behind a test in `src/tests.rs` + the module unit tests)

- **Per-workspace storage + hard tenant binding.** One `topos-gitstore` repo per workspace under a
  confined root, plus a SQLite database whose every row carries `workspace_id` and whose every query
  binds it. `WorkspaceId` is a validated, path-safe id, so the per-workspace store directory can never
  escape the root. Isolation is the database binding, never the directory.
- **The schema** (`migrations/0001_init.sql`, SQLite STRICT / WITHOUT ROWID; content ids as 32-byte
  BLOBs): `skill_commit` (provenance — **PK `(workspace_id, commit_id)`** makes a content-derived commit
  belong to exactly one skill), `commit_object` (reachability, with the inverse access-join index),
  `roster` (membership = a row exists), `current` (the pointer — created + seedable, **never moved**).
- **`Authority::read_object`** — the skill-scoped read. One join authorizes on rostered ∧ reachable and
  yields a witness commit; the bytes are then read + re-verified from the store. Every
  not-entitled/not-found case returns one indistinguishable `NotFound`; a store failure on an
  already-authorized object is a separate `Integrity` fault (corruption), never a not-found. **No object
  is served by bare hash.**
- **`Authority::upload_candidate`** — full-tree upload + server rehash. The server recomputes every id
  from the uploaded bytes (no client id trusted; no reference-by-id), applies the canonical rules, writes
  the objects, then records provenance + reachability **only after** an authoritative roster check, in one
  `BEGIN IMMEDIATE` transaction. The reachability edges are derived internally from the recomputed bytes.
  Dedup is invisible (the receipt charges **logical** uploaded bytes and is identical on a hit). No
  pointer is moved.
- **`Authority::check_lineage`** — the cross-skill lineage predicate (a tiny database gather + a pure
  decision function), built **read-only** here; the pointer-move write enforces it transactionally later.
- **Transaction discipline.** WAL + `synchronous = NORMAL` + a busy timeout + foreign keys on, set on the
  connect options; one private `begin_with("BEGIN IMMEDIATE")` entrypoint (a plain `begin()` or a bare
  `execute("BEGIN IMMEDIATE")` on the pool are unreachable). Compile-time-checked `query!` against the
  committed `.sqlx`; `cargo sqlx prepare --check -- --tests` is the CI drift gate (the `--tests` scope
  matches how the metadata is generated — three queries are `#[cfg(test)]`-only seed helpers — and the CLI
  is pinned to the library version).

## Backend shape (concrete now; a second backend is a mechanical add)

`Authority` holds a concrete `sqlite::Db` directly — no trait, no `sqlx::Any`, and no single-arm enum yet
(concrete-first, per the workspace's governing posture; only one backend ships). The load-bearing invariant
is that **no `sqlx` type ever crosses the `sqlite` module boundary**: every method there takes the id
newtypes + data and returns plain domain values. A future Postgres backend is a sibling module with its own
`query!` invocations and its own `.sqlx`, behind an `enum Db { Sqlite, Pg }` that wraps that same
domain-typed boundary with no change to callers.

## Planned (lands later)

The object-lifecycle / garbage-collection fence (quarantine, `object_presence`, promotion leases,
transactional GC, the size-routed large-object store); the pointer-move write (compare-and-set on
`(epoch, seq)`, the in-process Ed25519 signer, durable all-outcome receipts) that *moves* the `current`
row this layer only creates; the cross-skill lineage predicate's transactional **enforcement**; Postgres
(SQLite-first); proposals / the review gate; per-skill encryption-at-rest.

## Build note

Adding `sqlx` pulls the bundled SQLite C library into this crate (and the plane binary), so building the
server crate now needs a C toolchain (CI runners have one). The **client never gets this edge** —
`cargo run -p xtask -- check-arch` asserts `topos` depends on no `plane-store` / `sqlx` / `libsqlite3-sys`.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `thiserror`, raw `sqlx` (sqlite,
runtime-tokio, macros, migrate — no TLS); `tokio` is a dev-dependency only (it drives `#[tokio::test]`;
the runtime is otherwise the caller's, via sqlx's `runtime-tokio` feature).
