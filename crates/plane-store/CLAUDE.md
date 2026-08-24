# `plane-store` — the vault's byte-custody boundary

**A crate so that raw access is private.** It owns the vault's per-workspace SQL — raw `sqlx`, no
ORM — and the ONE object store every bundle byte lives in (`src/store.rs`, over `object_store`:
`LocalFileSystem` as the self-host default, any S3-compatible endpoint optionally; every byte an
immutable zlib loose git object keyed by its git OID at the exact bare-repo path shape
`<ws>/objects/<aa>/<38-hex>`). The pool, every transaction, every raw SQL statement, and every raw
object read are `pub(crate)`-private; the **only** public surface is the custody operations on
`Authority` (+ `StoreConfig` and the `import_local` report types). The vault is STATELESS over
Postgres + the store: upload staging is an ephemeral local dir, safe to lose.

## The trust shape

The vault is **PURE BYTE CUSTODY** with ONE caller — the composing product app — and treats every
request as **pre-authorized**. No identity, membership, or policy row lives here (two automated
`cargo xtask check-arch` gates pin that: identity-vocabulary + schema-boundary). Requests carry
opaque `(workspace_id, bundle_id, …)` strings plus `attribution` display strings stored verbatim;
the vault validates SHAPE (charset/length), never meaning.

## File map (the orchestration/SQL twin convention)

- `src/custody/X.rs` — orchestration OUTSIDE the transaction (filesystem work, candidate
  assembly; no SQL); `src/db/custody/X.rs` — the raw-SQL half (the one SERIALIZABLE
  `run_serializable!` write transaction + pool reads; no `sqlx` type crosses out of `mod db`).
- `custody/upload.rs` — the candidate DTOs. `custody/lifecycle.rs` + its db twin — the
  object-lifecycle fence: ephemeral-staging ingest (server rehash — no client id trusted),
  lease-before-migrate, the store put + install CAS, the version-skeleton put (in-memory codec →
  tree/commit objects, no presence rows, retained until workspace deletion), the fenced
  `object_presence` state machine, tombstones. `store.rs` — the object-store seam
  (put/get/exists/single-attempt-delete + prefix list/bulk-delete for workspace deletion and
  import verification; the GC deleter is retry-disabled and hard-bounded below the recovery
  staleness threshold). `custody/import.rs` + db twin — the one-shot `import-local` cutover.
- `custody/commit.rs` / `db/custody/pointer.rs` — version rows + the generation-fenced pointer
  CAS, purge, bundle/workspace reclaim. `custody/read.rs` + twin — the verified reads + the
  per-workspace storage stat (raw file bytes, never compressed store bytes). `custody/gc.rs` —
  mark-then-acquire GC (acquire → owned single-attempt delete → finalize; an ambiguous delete
  NEVER finalizes absence), recovery, the staging janitor (`run_gc`/`run_recovery`/`run_janitor`
  — the composing server MUST schedule them, and run the janitor on boot; no scheduler here).
  Clock convention: epoch milliseconds.
- `authority.rs` — the sealed facade; `error.rs` — the boxed-source error; `id.rs` — validated id
  newtypes (`[A-Za-z0-9._-]{1,128}`, no leading dot — the path fence) + the attribution check.

## The semantics (each behind a test in `src/tests/`)

- **A version IS the hash of its bytes:** `version_id` = the kernel commit id, recomputed
  server-side from rehashed bytes; committing an identical candidate converges (`deduped`).
- **History is a LIST, not a DAG:** a version has at most one parent (0 for genesis, 1 otherwise).
  The count is fenced at the commit-frame mint — before any byte is staged and before any pointer
  moves — so both lanes are covered: a multi-parent candidate never becomes a version row, hence
  can never be published OR approved. The kernel frame and the git store stay general; the
  authority refuses.
- **The generation-fenced pointer:** one movable `current_pointer` per bundle; every move CASes a
  single `generation`. Idempotent-CAS rule: a pointer already at `expected + 1` naming the exact
  target answers `replayed` (crash retries are safe); any other mismatch is the typed `Conflict`
  carrying the live state. Both movers enforce the same-bundle first-parent lineage fence, so
  approving a proposal whose base has advanced CONFLICTS rather than fast-forwarding. A refused
  CAS rolls the whole commit transaction back.
- **Revert is a FORWARD commit** (`{tree: target.tree, parents: [current]}`) + the CAS; a purged
  target refuses typed.
- **Purge:** refused while pointed-at; stamps `purged_at`, denylists the version's unique blobs
  (tombstones — ingest and install refuse them forever), reclaims inline. The version row — the
  hash — stays.
- **The GC keep-set is two clauses**, re-verified at acquire time: a non-purged version's
  `version_object` edge, or a live promotion lease (the lease→edge handoff closes the reclaim
  window by construction).
- **Verified reads:** every byte re-verified against the id that named it; never by bare hash —
  only through a bundle whose live version reaches it. Corruption is a typed `Integrity` alarm,
  NEVER folded into the uniform `NotFound`.

## Transaction discipline

Every write runs through the private `run_serializable!` macro: SERIALIZABLE + bounded full-jitter
retry on 40001/40P01 and the two convergent unique violations. Compile-time-checked `query!`
against the committed `.sqlx`; `cargo sqlx prepare --check -- --tests` is the CI drift gate.
Reads run autocommit at READ COMMITTED.

Postgres-only (`sqlx` pure-Rust). Dependencies: `topos-core`, `topos-gitstore` (the in-memory
loose-object codec), `thiserror`, `sqlx`, `tokio` (`time`+`rt`), `object_store` + `futures-util`
(owned by this crate + `topos-plane` ALONE — a check-arch assertion), `getrandom`, `tracing`.
Nothing signs, nothing hashes a credential.
The `test-fixtures` feature exposes only `Authority::from_pool`, `Authority::with_reject_cap`,
and the embedded `MIGRATOR`; the
production build never enables it, and the client takes NO edge to this crate (`check-arch`).
