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

## The security boundary is the database, not the key prefix

The vault stores every byte in **one object store** — a local directory (the self-host default) or
any S3-compatible bucket, behind one seam (`src/store.rs`) — as immutable zlib **loose git
objects** keyed by their own git OID at the exact bare-repo path shape
`<workspace>/objects/<aa>/<38-hex>`. That shape is an operational choice (a pre-existing git root
is readable in place; any git tooling pointed at a synced store reads it natively) — and it is
**not** the tenant boundary. Objects are content-addressed and the store has no per-object access
control, so the boundary has to live at the access layer:

1. **Binding.** Every row in every table carries `workspace_id`, and every query predicates on it
   (bound, never interpolated). Structurally, every database method takes `workspace_id` as a
   mandatory argument — a query without it cannot be written outside `mod db`. A forgotten predicate
   is the likeliest leak, so it is made unrepresentable. The metadata is one shared Postgres
   database, so this binding — not a key prefix — is the whole of the separation.
2. **Prefixing.** Every key begins with the workspace id — `WorkspaceId` is a path-safe newtype (no
   separators, no `..`, no leading dot), so a key can never name another tenant's prefix. The
   prefix is bookkeeping hygiene (it is what lets workspace deletion be one bounded prefix sweep,
   and it forecloses cross-workspace byte dedup by construction), never the access decision.

## The vault is stateless

Postgres + the object store are the vault's whole durable state. Upload staging is an **ephemeral
local directory** (`TOPOS_PLANE_TMP`, default under `/tmp`): no volume, no fsync, and losing it on
a container replacement is safe by design — the janitor reconciles leftover `upload` rows via the
existing protocol (`remove` tolerates a dir that is already gone), and an interrupted upload
replays from the client's write-ahead log. A PUT that landed without its `absent → present`
transition is a harmless content-addressed orphan, dedup-verified on the next ingest of the same
content. The vault must serve correctly after container replacement with only Postgres + the
store — the composed e2e suite proves exactly that (restart with an empty local filesystem).

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

## History lives in rows; the skeleton lives in the store

Refs are gone server-side. A version row carries its **git commit locator** (`git_commit_oid`) and
its **message** beside the facts it always carried (`first_parent`, attribution, timestamps), so
`log` is one recursive Postgres query and version metadata needs the store only for the file
listing (commit → tree walk). Both columns are nullable — rows written before the object-store
import have neither, and every read that needs them **fails closed** with a typed error naming
`topos-plane import-local` (which backfills them from the old repos' refs). New writes always fill
both; a deduped re-commit of an identical candidate heals a pre-import row's NULLs.

Tree and commit objects — the version **skeleton** — carry no presence or reachability rows: they
are addressing structure, not custody content, and they are retained until **workspace deletion**
(a store prefix-list + bulk-delete, run after the rows drop), exactly the retention the
per-workspace repo used to give them. They are written in `migrate_finish`, immediately before the
commit-version transaction; a crash in between leaves **orphan skeleton objects — accepted**
(immutable, content-addressed, bytes-cheap, invisible to every read, reclaimed with the
workspace). No sweeper exists for them in this increment, deliberately.

**Write policy: plain idempotent put.** Keys are content-addressed on both lanes (sha256-identified
file bytes framed as git blobs; OID-keyed skeleton), so an overwrite writes the identical logical
object — a re-put is self-HEALING, never destructive — and no conditional-write support is asked of
a backend (the weakest-backend rule; the atomic swap is the Postgres pointer transaction).
Corrupted-object repair is a re-put from a healthy source; verify-on-read remains the gate. (The
zlib envelope may differ across compressor versions; identity is the OID over the *decompressed*
frame, which every read verifies.)

## The object-lifecycle / garbage-collection fence

The database is the single authority for every object's byte status; the store holds dumb bytes
and always *trails* the database. No operation stats the store to decide presence —
`object_presence` is the sole presence authority, and GC acts only on objects that have a row
there. A few decisions earn their keep:

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
  `status_updated_at` and that value is the actor's token: the delete step re-confirms ownership of
  it immediately before issuing, and the finalize is gated on matching it, so a recovery sweep
  taking over a frozen pass can never also delete or finalize the same row. Each step is its own
  short transaction (or none, for the delete), so no write transaction is held across a store op.
- **THE DELETE-LIFETIME INVARIANT.** On a remote store, "one actor" needs one more leg: a DELETE
  issued under a token must be provably DEAD before that token can be superseded, or a
  delayed/retried DELETE could remove bytes a later ingest re-installed under a fresh token —
  leaving Postgres saying `present` over missing bytes. The seam enforces it structurally: the GC
  delete rides a dedicated client with transport retries DISABLED (a single attempt), a hard
  request timeout, and an outer wall-clock bound (`REMOTE_DELETE_TIMEOUT_MS`, 15s) **strictly
  below** the recovery-staleness threshold (`RECOVERY_STALE_MS`, 60s) — the ordering is a
  compile-time assertion — while recovery may supersede a token only once the row is that stale.
  An AMBIGUOUS outcome (timeout, transport fault) **never finalizes absence**: the row stays
  `deleting` for a later pass, which re-runs the idempotent delete under its own token. The named
  residual: a DELETE whose connection was torn down at the bound may still execute server-side
  moments later — that execution window is bounded far below the 45s margin between the two
  constants, and transport-level re-issue (the dangerous, minutes-later replay) is what the
  retry-disabled client forecloses.
- **Scheduling is the composing server's.** `run_gc` / `run_recovery` / `run_janitor` are public ops;
  this library holds no scheduler and no background task. One server-clock unit is one epoch
  **millisecond** throughout — a seconds-valued TTL constant would collapse these fences
  thousandfold, which is why the convention is stated at every site that owns one.

## One store, one blob shape (the large-object split is gone)

Every file blob — 1 KiB or 90 MiB — is one zlib loose blob object at its `git_oid` key; the
size-routed `LocalLargeStore` and its threshold are deleted (the object store is the remote-capable
byte layer they were reserving a seat for). The per-blob reject cap (100 MiB by default) survives,
failing typed at **ingest** before any bytes are staged. `object_presence.location` **stays as a
column with one live value** (`git`): dropping it would buy nothing (the CHECK and every row would
need rewriting) and cost the seam a future placement split would need back, it still documents
which store family holds the bytes, and — the load-bearing part — a legacy value (`large-local`)
is how a PRE-IMPORT row is recognized: reads of one fail closed naming `import-local`, and the
import flips the column as it converts the raw large-store files into git-blob frames (verifying
BOTH identities: the sha256 filename and the recorded `git_oid` locator). `size` stays raw file
bytes, so the storage-accounting contract (`storage_stats` = the sum of present raw sizes, never
compressed store bytes, never the skeleton) is unchanged.

## What this deliberately is not

- **No signing, no keys, no credentials.** Nothing here signs a pointer or hashes a credential;
  integrity rests on content addressing plus verify-on-read. Optional signing could layer on later
  without touching identity, the schema, or the read signature.
- **No conditional writes, no presigned URLs, no cross-workspace dedup.** The store contract is
  put/get/exists/delete plus prefix-list + bulk-delete used ONLY by workspace deletion and import
  verification — the weakest S3-compatible backend suffices, and every byte still streams through
  the vault's verified reads.
- **No policy, no scheduler, no HTTP.** The composing server owns the wire, the clock ticks, and
  every decision about who may ask.

## Named residuals

- The LOCAL backend fsyncs a landed object + its dir chain inside the seam (the backend's own
  stage-and-rename does not), so "put returned ⇒ durable" holds on both backends; macOS
  `F_FULLFSYNC` remains the platform residual it always was.
- Workspace deletion on the local backend prunes emptied shard dirs (the backend's automatic
  cleanup); a crashed staged-upload temp beside a local object is invisible garbage the next
  workspace deletion sweeps with the prefix.
- Workspace deletion assumes the app does not recreate the same workspace id concurrently with the
  prefix sweep — the same assumption the old `rm -rf` carried.
