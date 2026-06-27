# plane-store — design notes

Why this crate is shaped the way it is. The contract + status live in `CLAUDE.md`; this is the *why*,
for a future contributor or auditor. This increment builds the storage authority's **storage + read**
half, in-process and directly tested against a real SQLite database and a real per-workspace git store.
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
   argument — a query without it cannot be written outside `mod sqlite`. A forgotten predicate is the
   monorepo's likeliest leak, so it is made unrepresentable.
2. **Physical.** A per-workspace SQLite file + a per-workspace git store directory under a confined root.
   `WorkspaceId` is a path-safe newtype (no separators, no `..`), so the store path can never escape.

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

`Authority` holds a concrete SQLite database directly — no trait (premature for one backend), no
`sqlx::Any` (it forfeits the compile-time-checked queries), and no single-arm enum yet. The invariant that
earns its keep is that **no `sqlx` type crosses the `sqlite` module boundary**: every method there is
domain-typed. That boundary is exactly the seam a future `enum Db { Sqlite, Pg }` wraps mechanically, with
no change to callers — so the second backend is an add, not a reshape. This follows the workspace's
governing posture: concrete first, extract on the second implementation.

## What this deliberately is not, yet

The garbage-collection fence (quarantine, object presence, promotion leases, the size-routed large-object
store), the pointer-move write (the compare-and-set, the in-process signer, durable receipts) that *moves*
the `current` pointer this layer only creates, the HTTP surface, identity/roster issuance, and Postgres are
each a clean follow-on against this foundation. Object-write and provenance-recording are kept separable so
the garbage-collection fence can later wrap both. The lineage predicate is built and tested read-only so
wiring it into the pointer-move transaction is a small change.
