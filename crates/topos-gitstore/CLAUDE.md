# `topos-gitstore` — the gix object mechanics + the large-object store

The shared dumb byte layer over `gix`: object read/write, a **recursive byte-oriented tree render**, and
the **sha256-id ↔ git-OID mapping** carried as a ref name (a tested invariant — git OIDs are SHA-1, an
internal detail; the version id is always our own sha256).

**Re-verifies bytes → expected sha256 on every read** (never trusts gix's object id). The untrusted tree
renderer is fuzzed. Holds **no access control** and **no `~/.topos/` policy** (it never fsyncs — it only
*names* the durability set for the client to sync, so the client owns the fault-injectable seam).

## Implemented (each behind a test in `src/tests.rs`)

- `Store::{init, open}` — one **bare** repo per skill (no worktree/index).
- `write_bundle` — validate every path through the kernel, write one real content-addressed git blob per
  file (no size cap, **no LFS pointer files**), build a tree mirroring paths + modes; returns the kernel
  `bundle_digest`.
- `commit` — snapshot a tree as a commit under `refs/topos/versions/<version_id>`; **re-derives the
  `version_id` through the kernel `commit_id` and refuses a lying ref**; maps parent `version_id`s → git
  commits (a missing parent is typed).
- `render_verified` — resolve → recursively walk the tree → re-hash **every** blob through the kernel
  sha256 → recompute the canonical `bundle_digest` → assert it equals the caller's pin. A flipped/forged
  byte, a non-UTF-8 name, or a non-blob entry fails **typed** (verify-on-read; the put→render round-trip is
  fuzzed byte-identical).
- `read_object_in_version` — read + verify **one** object's bytes from a version by its content id: walk
  the version's tree, re-hash each blob, return the one whose sha256 matches (the match **is** the
  verification); a content id absent from that tree is the typed `ObjectNotInVersion`. The plane's
  skill-scoped read drives this *after* authorization yields a witness version — there is **no**
  read-by-bare-hash path. (The git-resident / all-in-git read path; the authority's location-dispatching
  read handles an offloaded object — see the two primitives below.)
- `read_tree_structure` — recover a version's tree **structure** (`path, mode, git_oid` per file) **without
  reading any blob bytes**, so a version containing offloaded blobs (absent from the git odb) walks fine.
  The dumb gitstore half of the authority's location-dispatching whole-bundle render.
- `read_git_blob_verified` — read one **git-resident** blob by git id, returning `(bytes, recomputed
  sha256)`; the authority's render dispatches here for a leaf the DB locates in git. A missing object is the
  typed `MissingObject` (it never *infers* offload from absence — location is the DB's fact).
- `log` / `list_versions` — first-parent history + the ref-set reverse map, with duplicate-lineage rejected.
- `durability_set` — the WHOLE-tree walk (every loose object + version ref + the repo scaffolding + parent
  dirs) a client fsyncs to make a **fresh staging store** durable *before* any JSON references it. Scoped
  to just-created stores (`add`'s staged import; the `follow` baseline's empty init) — there the whole tree
  IS the op's writes. A store carrying history uses `version_durability` instead.
- `version_durability` — the per-write durability set of ONE version: its commit object, every tree + blob
  reachable from its tree, and its version ref (+ the parent dirs whose entries changed) — exactly what a
  `write_bundle` + `commit` pair created, accumulated per written version via `WriteBatch::extend` across a
  multi-version op. Keeps the client's per-op fsync cost bounded by what the op wrote, never lifetime
  history (the client never packs, so a full-tree sync would grow forever). The crash-safety contract
  (reachable ⇒ durable before any doc records it) is documented on `WriteBatch`.
- `unified_diff` (`diff`) — a byte-stable line-oriented unified-diff renderer over two bundles (`DiffFile`
  views). The diff **algorithm** is `imara-diff` (histogram); the unified-diff **formatting** (hunk headers,
  mode-change, binary detection, the no-newline marker) is **owned here**, so the committed `diff` golden
  stays byte-stable across imara-diff releases. The `diff` verb calls this; the `current..<hash>` plane half
  reuses it later.
- `merge_file` (`merge`) — the per-file three-way (diff3) content **execution** behind the kernel's merge
  policy: `merge_file(base, mine, theirs) -> Clean(bytes) | Conflict(bytes-with-markers) | Binary`, over
  `diffy` (pinned **exact**; its conflict bytes are a consent artifact locked by a byte-golden, so an
  upgrade is a reviewed change). Fixes `ConflictStyle::Diff3` (base section present) and **lengthens the
  conflict markers until unique** vs the content (no embedded `<<<<<<<` line can forge a boundary). A
  non-UTF-8 side is **never line-merged** (`Binary` → the client keeps both sides). Client-side input +
  expanded-output **size caps** are checked **before** allocation (typed `MergeError`; the server's ingest
  caps don't exist on the client). Bytes are never normalized (CRLF/EOF survive).
- **The object-lifecycle fence primitives** (`fence.rs`) — the dumb byte ops the plane's server-side
  garbage-collection fence drives, holding **no database and no access control**: `stage` writes a candidate's
  blobs into a per-op quarantine object store (returning each blob's `object_id`/`git_oid`/size + the kernel
  `bundle_digest`); `install_object_durable` copies one staged blob into the main store and **fsyncs** it (the
  object + its parent dirs) so the authority may mark it present only after the bytes are durable;
  `commit_durable` builds a migrated version's tree from already-installed ids (`write_tree`, **never**
  re-writing a blob) and records the commit + version ref durably (its synced set walks EVERY tree object,
  subtrees included — blobs stay the install's responsibility); `delete_loose_object` is the GC unlink;
  `read_staged_blob` reads one staged blob's bytes from a quarantine (the large-install path's byte fetch);
  `object_exists` is an idempotency belt only. Unlike the client write path (which names a durability set for
  the client to fsync), these server-side ops are self-durable and **return the path set they synced**.
  `write_tree` builds the tree via the **low-level plumbing editor** (not `repo.empty_tree().edit()`, which
  validates child existence) so it can faithfully carry an **offloaded** blob's `(path, mode, git_oid)` even
  though that blob's bytes never enter git; identity is unaffected (it's over real-byte sha256s, not the git
  tree OID). The client write path (`write_bundle`) and `render_verified`/`read_object_in_version` are unchanged.

## The large-object store — `LocalLargeStore` behind the `LargeObjectStore` trait (**wired**)

A content-addressed `put`/`get`/`exists`/`delete` trait keyed by `blob_id = sha256(raw bytes)`, with the v0
**local-filesystem** impl `LocalLargeStore` (a dumb byte layer — **no access control, no database**). Layout
under a per-workspace root: sharded finals `objects/aa/bb/<64-hex>` + same-filesystem `tmp/` staging.
**Crash-safe two-phase install**: recompute `sha256 == blob_id` in memory → write a unique `O_EXCL` temp →
`fsync` → atomic rename to the final shard → fsync the shard dir chain (always overwrite, so a re-put
self-heals a crash-lost object). **Verify-on-read** (`get` re-hashes; a mismatch is the typed
`BlobIntegrity`). `delete` is idempotent. Durability matches the git fence's `sync_all` convention (the
macOS `F_FULLFSYNC` power-loss gap is the same documented residual). The authority constructs **one store per
workspace** (rooted at `large_root/<ws>/`), so cross-workspace isolation is the path — no cross-workspace
dedup; routing + the `location` dispatch live in `plane-store`. The deferred S3-compatible remote backend is
a second impl of this same trait.

## Planned (lands later)

The S3-compatible remote large-object backend (a no-op extraction behind the `LargeObjectStore` trait).

Dependencies: `gix` (plumbing-only: `sha1` + `tree-editor`), `imara-diff` (the diff engine), `diffy`
(pinned exact — the diff3 merge engine), `topos-core`, `topos-types`, `thiserror`.
