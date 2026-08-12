# plane-store — design notes

Why this crate is shaped the way it is. The contract lives in `CLAUDE.md`, the public surface in
`src/lib.rs`, and the table-by-table reasoning in `migrations/0001_custody.sql`; this is the *why*,
for a future contributor or auditor.

## The vault decides nothing about people

Authorization, protection, and entitlement are decided once, app-side, before a byte op is ever
forwarded. This crate holds no identity, membership, or policy row, re-verifies nothing about a
caller, and treats every request as **pre-authorized** — `workspace_id` / `bundle_id` arrive as
opaque strings and `attribution` as a display string stored verbatim, shape-checked (charset,
length) but never interpreted. Two `cargo xtask check-arch` gates — identity vocabulary and
app-schema table names — keep that mechanical rather than remembered.

The consequence for this document: every rule below is a **custody** rule — tenant separation,
integrity, reachability, lifecycle. None of them is an access-control rule, and none should be grown
into one; a request that reached this layer has already been judged.

## The security boundary is the database, not the directory

The vault stores all of one workspace's bundles in a **single per-workspace git object store** (a
monorepo). That is an operational choice — one cloneable artifact, one history, one store to back up
— and it is **not** the tenant boundary. Git has no per-object access control and objects are
content-addressed, so the boundary has to live at the access layer. It is two independent
mechanisms, and the directory layout is never one of them:

1. **Binding.** Every row in every table carries `workspace_id`, and every query predicates on it
   (bound, never interpolated). Structurally, every database method takes `workspace_id` as a
   mandatory argument — a query without it cannot be written outside `mod db`. A forgotten predicate
   is the monorepo's likeliest leak, so it is made unrepresentable. The metadata is one shared
   Postgres database, so this binding — not a physical file — is the whole of the metadata
   separation.
2. **Physical.** A per-workspace git store and a per-workspace large-object root, both under
   confined roots (the object bytes). `WorkspaceId` is a path-safe newtype (no separators, no `..`,
   no leading dot), so a store path can never escape its root.

## The bundle-scoped read rule

`read_object(workspace, bundle, object_id)` serves bytes **only** when some live (non-purged)
version of that bundle reaches the object — an indexed probe over `version_object`, whose inverse
index exists for exactly this question. **There is no read-by-bare-hash path anywhere**: holding a
content id is not authority to fetch its bytes.

- **One not-found.** An empty probe is the single not-found signal. Unreachable, purged, and never
  existed are byte-for-byte indistinguishable, so a caller can never probe which bundles or objects
  exist.
- **Integrity is not a not-found.** The probe already proved reachability, so once a store is
  touched there is no benign "the object is not here" case left: a miss there is a divergence
  between the vault's own bookkeeping and its bytes. It maps to a distinct `Integrity` fault, kept
  textually separate so the distinction cannot rot, and it discloses nothing because it is reachable
  only *after* reachability was proven.
- **Verify-on-read is the whole verification.** Every byte served is rehashed against the id that
  named it; the hash match *is* the check, so a corrupted or substituted blob can never be returned,
  and no signature is needed to make the read safe. Keying on the content id is also what let the
  size-routed store below land as one dispatch branch, with no change to identity, the database, or
  the read signature.

## Ingest recomputes every id

A byte-introducing write uploads a full candidate tree — **every byte**, never a "reference this
blob by id" — and the vault recomputes every id from those bytes (object ids, the canonical
manifest, the bundle digest, the commit id). A client-supplied id is never trusted, and the kernel's
canonical reject rules fire once, over the rehashed bytes. That is what makes "a version IS the hash
of its bytes" a property of the vault rather than a promise by its caller.

Ingest stages into a **GC-excluded quarantine** and only then migrates, so rehashing and the
tombstone check both happen before any object joins the main store and a rejected or abandoned
candidate leaves no reachable debris (the janitor sweeps a stale staging row). Reachability edges
are derived internally from the recomputed bytes — the distinct object ids of the candidate's tree —
never from client input, because a forged edge could otherwise make another bundle's object look
reachable.

## History is a list; the pointer is the only mutable thing

A version has **at most one parent** (0 for genesis, 1 otherwise), fenced at the commit-frame mint —
before any byte is staged and before any pointer moves — so a multi-parent candidate never becomes a
version row and therefore can never be published *or* approved. The kernel frame and the git store
stay general; the refusal is the authority's.

Every bundle has one movable `current_pointer`, and every move is a compare-and-set on a single
`generation` counter. A pointer already sitting exactly one past `expected` and naming the exact
target answers `replayed`, so an app-side retry after a crash is safe without vault-side receipts;
any other mismatch is the typed `Conflict` carrying the live state, never a silent overwrite. Both
movers enforce the same first-parent lineage fence, so approving a proposal whose base has since
advanced conflicts instead of fast-forwarding over the intervening version. A refused CAS rolls the
whole commit transaction back, so a version row can never outlive the pointer move that justified
it. Revert is a **forward** commit (`{tree: target.tree, parents: [current]}`) plus the ordinary
CAS: the pointer never moves backward, and history only grows.

## Backend shape

`Authority` holds a concrete Postgres pool directly — no trait, no `sqlx::Any` (which would forfeit
the compile-time-checked queries). Postgres is the single backend; there is no dialect abstraction
to earn its keep. The invariant that does earn its keep is that **no `sqlx` type crosses the `db`
module boundary**: every method there is domain-typed, so the engine choice and the retry machinery
stay sealed inside `mod db` and the rest of the crate reasons in custody terms only.

Every write runs through one private `run_serializable!` macro: a `SERIALIZABLE` transaction with a
bounded full-jitter retry on a serialization failure (`40001`), a deadlock (`40P01`), and the two
**convergent** unique violations (`version_pkey`, `current_pointer_pkey`) — where the id is
content-derived, so a collision only ever means "the same version", and the loser's retry finds the
winner's row and converges instead of surfacing a spurious 500. The scope is exactly those two
constraints, so an ordinary unique violation (a genuine bug) still surfaces. Every read-then-write
invariant — the whole-`generation` CAS, the object-presence fence, the GC keep-set — is re-proven by
SSI plus retry rather than by a global lock. Reads run autocommit at READ COMMITTED.

## The object-lifecycle / garbage-collection fence

The database is the single authority for every object's byte status; the git store holds dumb bytes
and always *trails* the database. No git ref is used for reachability, and no operation stats the
store to decide presence — `object_presence` is the sole presence authority, and GC acts only on
objects that have a row there. A few decisions earn their keep:

- **The keep-set is exactly the read-authorization surface, not the `current` ancestry.**
  `read_object` serves a blob reachable from *any* non-purged version of the bundle; it never
  consults `current` (that decoupling is what lets a reader fetch an unpromoted version). So the
  readable set is "every object some live version references". Reclaiming by `current`-reachability
  instead could unlink a blob that is still readable through a non-`current` version — a corruption
  alarm on a previously-valid read. The fence therefore spares any object with a `version_object`
  edge ∪ any object named by a live promotion lease, and re-verifies both **at delete time** inside a
  guarded compare-and-swap, closing the snapshot-then-delete race against a freshly inserted lease.
- **The locator lives in the database, not a ref.** A fenced object's row carries its `git_oid` (the
  physical loose-object locator), written in the same compare-and-swap that flips it to `present`.
  The database is already the lifecycle authority, so the locator belongs with it — one
  crash-consistent artifact (the row) instead of two (a row plus a separate ref a crash could
  strand). `topos-gitstore` stays a dumb byte layer keyed by `git_oid`.
- **`deleting` is non-resurrectable by construction.** The only writer of `present` is the install
  compare-and-swap, whose `WHERE status = 'absent'` structurally cannot fire on a `deleting` row. An
  install that meets a `deleting` object waits for `absent` — polling **outside** any write
  transaction, so it never pins a connection across the wait or freezes a snapshot the finalize
  needs — and then re-copies fresh. It can never rescue bytes a GC has authorized to unlink.
- **Lease-before-migrate, with a two-state lifetime.** The promotion lease names a candidate's
  *full* object set — including an already-present object a dedup-skip would otherwise leave exposed
  — and is inserted before any byte migrates, so a concurrent GC's keep-set protects everything the
  candidate needs. A finite TTL guards a crashed or abandoned migrate (its objects become
  reclaimable); a *successful* migrate makes the lease non-expiring, so the version stays rooted
  until the commit transaction consumes it and its `version_object` edges take over. That
  lease→edge handoff is what closes the reclaim window by construction rather than by timing.
- **One actor removes the bytes.** The acquire stamps an accurate wall-clock into
  `status_updated_at` and that value is the actor's token: the unlink re-confirms ownership of it and
  the finalize is gated on matching it, so a recovery sweep taking over a frozen pass can never also
  unlink or finalize the same row. Each step is its own short transaction (or none, for the unlink),
  so no write transaction is held across a filesystem op.
- **Scheduling is the composing server's.** `run_gc` / `run_recovery` / `run_janitor` are public ops;
  this library holds no scheduler and no background task. One server-clock unit is one epoch
  **millisecond** throughout — a seconds-valued TTL constant would collapse these fences
  thousandfold, which is why the convention is stated at every site that owns one.

## The size-routed large-object store

Built on the fence: at install a **file blob** is routed by **size** — at or above a configurable
threshold (1 MiB by default) it is physically offloaded to a per-workspace content-addressed
`LocalLargeStore` (`object_presence.location = large-local`), below it stays a git loose object
(`git`); commits and trees always stay in git. A per-blob reject cap (100 MiB by default) fails
typed at **ingest**, before any bytes are staged.

- **Identity is placement-independent, and there are no pointer files.** `version_id` and
  `bundle_digest` are topos's own sha256 over real bytes, computed at ingest *before* any routed
  write, so which store holds a blob changes no id or digest. The git tree faithfully carries an
  offloaded blob's `git_oid` (the tree is built with the low-level plumbing editor, which tolerates a
  child object absent from git) — no LFS pointer object, no `.gitattributes`. `size` and `location`
  are operational and never enter a manifest.
- **The database owns `location`; reads and the GC unlink dispatch on it.** A read looks up the
  *present* row's `location` and fetches from git or the large store — always after the same
  bundle-scoped reachability probe, and a post-probe miss in either store is `Integrity`, never
  not-found. The GC unlink step dispatches on `location` (a loose-object delete, or a
  `LocalLargeStore` delete keyed on the object id); the lease, the CAS, the `deleting` fence, and
  recovery are unchanged by the routing.
- **Per-workspace roots; no cross-workspace dedup.** Each workspace gets its own large-object root,
  so byte-identical content in two workspaces is two distinct physical objects and the hard tenant
  boundary is the path, not just a predicate. `LocalLargeStore` is a dumb byte layer (crash-safe
  two-phase install, verify-on-read); the routing and the `location` dispatch live here.

## What this deliberately is not

- **No signing, no keys, no credentials.** Nothing here signs a pointer or hashes a credential;
  integrity rests on content addressing plus verify-on-read. Optional signing could layer on later
  without touching identity, the schema, or the read signature.
- **The `large-remote` backend is schema-reserved, not built.** An S3-compatible impl shaped like
  `LocalLargeStore` plus a third `location` arm, and the idempotent online backfill it would need
  (copy → verify → flip `location` → repack), are additive and client-invisible when they land.
- **No policy, no scheduler, no HTTP.** The composing server owns the wire, the clock ticks, and
  every decision about who may ask.
