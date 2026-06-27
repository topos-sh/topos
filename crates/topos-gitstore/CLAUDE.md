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
- `log` / `list_versions` — first-parent history + the ref-set reverse map, with duplicate-lineage rejected.
- `durability_set` — the loose objects + version refs + their parent dirs the client fsyncs to make a write
  durable *before* any JSON references it.
- `unified_diff` (`diff`) — a byte-stable line-oriented unified-diff renderer over two bundles (`DiffFile`
  views). The diff **algorithm** is `imara-diff` (histogram); the unified-diff **formatting** (hunk headers,
  mode-change, binary detection, the no-newline marker) is **owned here**, so the committed `diff` golden
  stays byte-stable across imara-diff releases. The `diff` verb calls this; the `current..<hash>` plane half
  reuses it later.

## The `LargeObjectStore` seam (declared, **unwired**)

A content-addressed `put`/`get`/`exists`/`delete` trait keyed by `blob_id = sha256(raw bytes)` — **no impl
ships yet** (every blob lives in the git store). Because identity is recomputed over real bytes, a later
size-routed local / S3-compatible backend is a pure drop-in behind this trait with zero id/digest impact.

## Planned (lands later)

Size-routing + the local large-object store impl + GC; `diff3` *execution* (three-way merge; the two-way
`unified_diff` renderer lands above); the S3-compatible remote backend (a no-op extraction behind the seam).

Dependencies: `gix` (plumbing-only: `sha1` + `tree-editor`), `imara-diff` (the diff engine), `topos-core`,
`topos-types`, `thiserror`.
