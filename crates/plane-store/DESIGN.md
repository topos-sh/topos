# plane-store — design notes

Why this crate is shaped the way it is. The contract + status live in `CLAUDE.md`; this is the *why*,
for a future contributor or auditor. This increment builds the storage authority's **storage + read**
half, in-process and directly tested against a real Postgres database and a real per-workspace git store.
It moves no pointer, signs nothing, and issues no identity.

## The security boundary is the database, not the directory

The plane stores all of one company's skills in a **single per-workspace git object store** (a monorepo).
That is an operational choice — one cloneable artifact, one history, one store to back up — and it is
**not** the security boundary. Git has no per-object access control and objects are content-addressed, so
a physical monorepo must behave like a *logical repo-per-skill at the access layer*. The access layer is
this crate.

Cross-company separation is two independent mechanisms, and the directory is never one of them:

1. **Binding.** Every row in every table carries `workspace_id`, and every query predicates on it (bound,
   never interpolated). Structurally, every database method takes `workspace_id` as a mandatory first
   argument — a query without it cannot be written outside `mod db`. A forgotten predicate is the
   monorepo's likeliest leak, so it is made unrepresentable. The metadata is one shared Postgres database,
   so this binding — not a physical file — is the whole of the metadata separation.
2. **Physical.** A per-workspace git store directory under a confined root (the object bytes). `WorkspaceId`
   is a path-safe newtype (no separators, no `..`), so the store path can never escape.

## The skill-scoped read rule

`read_object(principal, workspace, skill, object_id)` returns the bytes **only** when two independent facts
hold: the caller is rostered for the skill, and some commit of that skill reaches the object
(`skill_commit ⋈ commit_object`). Both are funneled through **one** SQL join that returns a *witness*
commit or nothing.

- **One not-found.** An empty join is the single not-entitled/not-found signal. Not-rostered, the skill
  does not reach the object, and the object does not exist are byte-for-byte indistinguishable — a caller
  can never probe which skills or objects exist. There is **no read-by-bare-hash path anywhere**; the only
  object-returning surface is this skill-scoped one.
- **Integrity is not a not-found.** The witness already proved reachability, so once the git store is
  touched there is no benign "object not in this version" case left: any store failure there is a
  divergence between the authority's own provenance and its store (corruption). It maps to a distinct
  integrity fault, kept textually separate so the distinction cannot rot, and it discloses nothing because
  it is reachable only *after* entitlement was proven.
- **The byte fetch.** A new `topos-gitstore` method reads one object from a version by content id: it
  resolves the witness commit, walks its tree, and returns the blob whose recomputed sha256 matches — the
  hash match *is* the verification, so a corrupted or forged blob can never be returned. Keying on the
  sha256 keeps a future size-routed large-object store a one-branch change, with no change to identity,
  the database, or the read signature.

## The upload (publish-side confused-deputy guard)

A full candidate tree is uploaded — **every byte**, never a "reference this blob by id" — and the server
recomputes every id from the bytes (blob ids, the canonical manifest, the bundle digest, the commit id).
A client-supplied id is never trusted. The canonical reject rules fire once, in the kernel, over the
uploaded bytes.

- **Authorization before provenance.** The read join trusts `skill_commit` directly, so nothing readable
  may be recorded for an un-rostered caller. Objects are written to the store first (harmless: with no
  garbage collection yet, an un-rostered or crashed upload leaves orphan objects that are unreachable
  through the only public surface — the read join is over the database, and there is no bare-hash path).
  Provenance + reachability are recorded **only after** the authoritative roster check, in one immediate
  transaction. A cheap roster pre-read before the git write is a non-load-bearing fail-fast that bounds
  orphan creation; it never changes the response shape.
- **No cross-skill adoption.** Content-addressing makes a re-upload of another skill's commit the *same*
  commit id. The `skill_commit` primary key `(workspace_id, commit_id)` makes a commit belong to exactly
  one skill, so the adoption is refused at insert — structurally, not by a remembered check.
- **Edges are derived internally** from the recomputed bytes (the distinct blob ids), never from client
  input — a forged edge could otherwise make another skill's object look reachable.

## Dedup is invisible

Whether a blob is new or already present must not be observable, or the existence of bytes in a restricted
skill could leak across tenants. So: the upload always consumes the full tree and hashes it before any
decision; the inserts are conflict-tolerant with no `rows_affected` branching; and the receipt is a pure
function of the upload — it charges **logical** uploaded bytes (the sum of file lengths), never physical
stored bytes. A re-upload of identical bytes returns a byte-identical receipt. The no-timing-leak property
is structural (the always-full-rehash-then-record shape), not a flaky runtime assertion. This layer ships
no presence/count table at all.

## Backend shape

`Authority` holds a concrete Postgres pool directly — no trait, no `sqlx::Any` (it forfeits the
compile-time-checked queries). Postgres is the single backend; there is no dialect abstraction to earn its
keep. The invariant that does earn its keep is that **no `sqlx` type crosses the `db` module boundary**:
every method there is domain-typed, so the choice of engine (and the retry machinery below) stays sealed
inside `mod db` and the rest of the crate reasons in trust terms only.

Because Postgres does not serialize writers the way SQLite's single-writer file did, every write runs
through one private `run_serializable!` macro: a `SERIALIZABLE` transaction with a bounded retry on a
serialization failure (`40001`) or deadlock (`40P01`), and on the two idempotency-key unique-violations
(`op_receipts`/`workspace_events`) so a concurrent same-`op_id` sibling's retry replays the committed
receipt instead of surfacing a spurious 500. Every read-then-write invariant SQLite's `BEGIN IMMEDIATE`
gave for free — the whole-`(epoch,seq)` CAS, the last-owner write-skew guard, the object-presence fence —
is re-proven by SSI + retry rather than by a global lock.

## The object-lifecycle / garbage-collection fence

The database is the single authority for every object's byte status; the git store holds dumb bytes and
always *trails* the database. No git ref is used for reachability, and no operation stats the store to decide
presence — `object_presence` is the sole presence authority. A few decisions earn their keep:

- **The keep-set is exactly the read-authorization surface, not the `current` ancestry.** `read_object`
  authorizes a blob on *any* `commit_object` edge of a rostered skill — it never consults `current` (that
  decoupling is what lets a member read an unpromoted version). So the readable set is "every object some
  commit references." If GC reclaimed by `current`-reachability instead, it could unlink a blob that is still
  readable via a non-`current` commit — a corruption alarm on a previously-valid read. The fence therefore
  spares any object with a `commit_object` edge ∪ any object named by a live lease, and folds that into a
  single guarded claim compare-and-swap that re-checks **at delete time** (closing the snapshot-then-delete
  race against both a freshly-inserted lease and a concurrent legacy upload). The `current`-ancestry walk the
  eventual design wants becomes meaningful only once the pointer-move makes `current` the trunk root and
  proposals create reclaimable non-trunk branches; it lands then, as one coherent change, not here.
- **The locator lives in the database, not a ref.** A fenced object's row carries its `git_oid` (the physical
  loose-object locator), written in the same compare-and-swap that flips it to `present`. The database is
  already the lifecycle authority, so the locator belongs with it — one crash-consistent artifact (the row)
  instead of two (a row plus a separate ref that a crash could strand). `topos-gitstore` stays a dumb byte
  layer keyed by `git_oid`. (This row is also the carrier the size-routed large-object store uses to recover
  an offloaded blob's identity — see below.)
- **`deleting` is non-resurrectable by construction.** The only writer of `present` is the install
  compare-and-swap, whose `WHERE status = 'absent'` structurally cannot fire on a `deleting` row. A migrate
  that meets a `deleting` object waits for `absent` (polling **outside** any write transaction, so it never
  pins a connection across the wait or freezes a SERIALIZABLE snapshot the finalize needs) and then re-copies
  fresh — it can never rescue
  bytes a GC has authorized to unlink.
- **Lease-before-migrate, with a two-state lifetime.** The promotion lease names a commit's *full* object set
  — including an old, already-present object a dedup-skip would otherwise leave exposed — and is inserted
  before any byte migrates, so a concurrent GC's keep-set protects everything the commit needs. A finite TTL
  guards a crashed/abandoned migrate (its objects become reclaimable); a *successful* migrate makes the lease
  non-expiring, so the migrated version stays rooted until the later pointer-move consumes it (a finite TTL
  would let GC reclaim a good, just-migrated version).
- **Built behind the privacy boundary, wired to no verb yet.** The fence is the directly-testable internal
  ingest/migrate/GC ops; the legacy straight-to-git upload path is left untouched and GC ignores any object
  with no presence row, so this increment is additive. One residual is documented: a concurrent legacy upload
  could add a `commit_object` edge in the tiny window between a guarded claim and the (transaction-free)
  unlink — not reachable in production this increment (the fenced ingress has no public verb), and closed when
  the pointer-move routes all writes through the fence.

## The size-routed large-object store (offload)

Built on the fence: at migrate (`install_one`, the migrate's install step) a **file blob** is routed by
**size** — at/above a configurable ~1 MiB threshold it is physically offloaded to a per-workspace
content-addressed **`LocalLargeStore`** (`object_presence.location = large-local`), below it stays a git
loose object (`git`); commits and trees always stay in git. A per-blob ~100 MiB reject cap fails typed at
**ingest**, before any bytes are staged. A few decisions earn their keep:

- **Identity is placement-independent, and there are no pointer files.** `version_id` / `bundle_digest` are
  topos's own sha256 over real bytes, computed at ingest *before* any routed write, so which store holds a
  blob changes no id/digest. The git tree faithfully carries an offloaded blob's `git_oid` (the migrate tree
  is built with the low-level plumbing editor, which tolerates a child object that is absent from git) — no
  LFS pointer object, no `.gitattributes`. `size`/`location` are operational and never enter the manifest.
- **The DB owns `location`; reads + the GC unlink dispatch on it.** A single-object `read_object` looks up
  the *present* row's `location` and fetches from git or the large store — always after the same skill-scoped
  authz join (404-not-403, never by bare hash); a post-authz miss in either store is `Integrity`, never
  not-found. Whole-bundle `render_version` is **tree-driven** (the migrate writes no `commit_object` edges
  yet): it walks the version's tree structure and joins the offloaded subset by `git_oid`, re-verifying every
  byte to its content id. The GC unlink step dispatches on `location` (a loose-object delete, or a
  `LocalLargeStore::delete` keyed on the object id); the lease, the CAS, the `deleting` fence, and recovery
  are unchanged.
- **Per-workspace roots; no cross-workspace dedup.** Each workspace gets its own large-object root, so
  byte-identical content in two workspaces is two distinct physical objects and the hard tenant boundary is
  the path, not just an ACL. `LocalLargeStore` is a dumb byte layer (crash-safe two-phase install,
  verify-on-read); routing + the `location` dispatch live here in `plane-store`.

## What this deliberately is not, yet

The large-object store's **S3-compatible remote backend** (a second `LocalLargeStore`-shaped impl + a
`large-remote` `location` arm) and its **idempotent online backfill** (copy → verify → flip `location` →
`git repack`) — both additive and client-invisible — the pointer-move write (the compare-and-set, the
in-process signer, durable receipts) that *moves* the `current` pointer this layer only creates **and**
consumes a migrated candidate's lease, the `purge` verb + force-unlink (the tombstones table + denylist check
already exist), the HTTP surface, and identity/roster issuance are each a clean follow-on against
this foundation. The lineage predicate is built and tested read-only so wiring it into the pointer-move
transaction is a small change.
