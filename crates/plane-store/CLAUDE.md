# `plane-store` — the vault's byte-custody boundary

**A crate so that raw access is private.** It owns the vault's per-workspace SQL — **raw `sqlx`, no
ORM** — and per-workspace object storage. The pool, every transaction, every raw SQL statement, and
every raw object read are `pub(crate)`-private; the **only** public surface is the custody
operations on `Authority`.

## The trust shape

The vault is **PURE BYTE CUSTODY** with ONE caller — the composing server fronting the product app —
and treats every request as **pre-authorized**: authorization, protection, and entitlement are
decided app-side, once. No identity, membership, or policy row lives here (two automated gates pin
that: the identity-vocabulary gate and the schema-boundary gate — `cargo xtask check-arch`).
Requests carry opaque `(workspace_id, bundle_id, …)` strings plus `attribution` display strings the
vault stores verbatim (the commit frame's author, `author_display`, `moved_by_display`); the vault
validates SHAPE (charset/length), never meaning.

## File map (the orchestration/SQL twin convention)

- `src/custody/X.rs` — orchestration OUTSIDE the transaction (filesystem work, candidate assembly;
  no SQL); `src/db/custody/X.rs` — the raw-SQL half (the one SERIALIZABLE `run_serializable!` write
  transaction + pool reads; no `sqlx` type crosses out of `mod db`).
- `custody/upload.rs` — the candidate DTOs (`CandidateUpload { files, parent, attribution, message }`).
- `custody/lifecycle.rs` / `db/custody/lifecycle.rs` — the object-lifecycle fence: quarantine ingest
  (server rehash — no client id trusted), lease-before-migrate, the durable install, the fenced
  `object_presence` state machine (`present`/`deleting`/`absent`/`unavailable`), promotion leases,
  the `upload` staging audit rows, tombstones.
- `custody/commit.rs` / `db/custody/pointer.rs` — the write orchestration + the ONE commit
  transaction: version rows (`version` + `version_digest` + `version_object`) and the
  generation-fenced pointer CAS, the purge, and the bundle/workspace row reclaims.
- `custody/read.rs` / `db/custody/read.rs` — the verified reads: the pointer record, one object's
  bytes (never by bare hash — only through a bundle whose live version reaches it; verify-on-read),
  a version's metadata + file listing, the first-parent log, the whole-bundle render.
- `custody/gc.rs` — the three-step mark-then-acquire GC, the recovery sweep, the quarantine janitor
  (public ops `run_gc`/`run_recovery`/`run_janitor` the composing server MUST schedule; this library
  holds no scheduler). Clock convention: epoch **milliseconds** everywhere.
- `authority.rs` — the sealed facade (`Authority` + `PoolConfig`); `error.rs` — the boxed-source
  error (typed `Conflict` carrying the live pointer, `TargetPurged`, `PointedAt`); `id.rs` — the
  validated id newtypes (opaque shape: `[A-Za-z0-9._-]{1,128}`, no leading dot — the path fence for
  the ids that become store directories) + the attribution shape check.

## The semantics (each behind a test in `src/tests/`)

- **A version IS the hash of its bytes**: `version_id` = the kernel commit id over
  `{parents, tree = bundle_digest, author = attribution, message}` — recomputed server-side from the
  rehashed bytes; `version.commit_id` carries the same value (a version IS its commit today; the two
  columns exist so the identities could diverge without a schema change). Committing an identical
  candidate twice converges on the same ids (`deduped`).
- **The generation-fenced pointer**: one movable `current_pointer` per bundle; every move
  compare-and-sets a single `generation` (expected `None` = genesis at generation 1). The
  **idempotent-CAS rule**: a pointer already sitting at `expected + 1` and naming the exact target
  answers success (`replayed`) instead of CONFLICT — app-side crash retries are safe with zero
  vault-side receipts. Any other mismatch is the typed `Conflict` carrying the live
  `(generation, version_id)`. BOTH movers enforce the same-bundle lineage fence — the target's
  first parent (persisted on the `version` row) must be the pointed version: a publish under
  `Some(g)`, and equally a promote of an existing version (the app's review-approve path), so
  approving a proposal whose base has since advanced CONFLICTS rather than silently fast-forwarding
  over the intervening version. A refused CAS rolls the whole commit transaction back — no version
  row lingers after a CONFLICT.
- **Revert is a FORWARD commit** `{tree: target.tree, parents: [current]}` + the CAS; a purged
  target refuses typed; a crashed retry is answered by the pre-stage idempotency probe (the pointer
  one past `expected` carrying the target's exact digest).
- **Purge** (`purge_version`): refused while pointed-at; stamps `purged_at` (dropping the version's
  reachability edges out of the GC keep-set), denylists the blobs UNIQUE to it (tombstones; ingest +
  the install CAS refuse them forever; a reclaimed tombstoned blob finalizes to the terminal
  `unavailable`), and reclaims the bytes inline. The version row — the hash — stays.
- **Bundle / workspace reclaim** (`delete_bundle` / `delete_workspace`): app-instructed row drops +
  an inline GC pass / physical store removal. Idempotent.
- **The GC keep-set is two clauses**, re-verified at acquire time: a NON-PURGED version's
  `version_object` edge, or a live promotion lease. The lease→edge handoff (the commit transaction
  holds the committed lease across the edge write; the lease is released only after commit) closes
  the reclaim window by construction.
- **Verified reads**: every byte re-verified against the id that named it; corruption is a typed
  `Integrity` alarm, NEVER folded into the uniform `NotFound`; a post-witness miss re-checks
  reachability once (a concurrently-purged object reads NotFound, genuine corruption alarms).

## Transaction discipline

Every write runs through the private `run_serializable!` macro: `SERIALIZABLE` isolation + a bounded
full-jitter retry on 40001/40P01 (and the two CONVERGENT unique violations: `version_pkey`,
`current_pointer_pkey`). Compile-time-checked `query!` against the committed `.sqlx`;
`cargo sqlx prepare --check -- --tests` is the CI drift gate. Reads run autocommit at READ COMMITTED.

## Build note

Postgres-only (`sqlx` pure-Rust — no C toolchain). Dependencies: `topos-core`, `topos-gitstore`,
`thiserror`, `sqlx`, `tokio` (`time` + `rt`), `getrandom` (op-id mint + retry jitter), `tracing`.
Nothing signs, nothing hashes a credential — there is no credential. The `test-fixtures` feature
exposes only `Authority::from_pool` + the embedded `MIGRATOR` for downstream test harnesses; the
production build never enables it (a CI guard asserts this). The client (`bins/topos`) takes NO edge
to this crate (`cargo xtask check-arch`).
