# `topos-gitstore` — the gix object mechanics + the large-object store

The shared dumb byte layer over `gix`: object read/write, a recursive byte-oriented tree render,
and the sha256-id ↔ git-OID mapping carried as a ref name (git OIDs are SHA-1, an internal detail;
the version id is always our own sha256). Path-parameterized and **bundle-generic** — one bare
repo per bundle for the client, one per workspace for the plane; it never asks what a bundle is.

**Re-verifies bytes → expected sha256 on every read** (never trusts gix's object id). Holds **no
access control** and **no policy**; it never fsyncs on the client path — it *names* the durability
set for the client to sync, so the client owns the fault-injectable seam.

## Surface (each behind a test in `src/tests.rs`)

- **Write:** `Store::{init,open}` (bare repos), `write_bundle` (kernel-validated paths → real
  content-addressed blobs + tree; no LFS pointers), `commit` / `commit_backfill` (version refs
  under `refs/topos/versions/<id>`; re-derives the `version_id` through the kernel and refuses a
  lying ref; backfill omits locally-absent parents for shallow history past a purge).
- **Read, always verified:** `render_verified` (whole-bundle walk, every blob re-hashed, digest
  asserted against the caller's pin — typed failure on any forgery), `read_object_in_version`
  (one object by content id, the hash match IS the verification; no read-by-bare-hash path),
  `read_tree_structure` (structure without blob bytes — walks fine over offloaded blobs),
  `read_git_blob_verified`, `log` / `list_versions` (first-parent history).
- **Durability sets:** `durability_set` (whole fresh store) and `version_durability` /
  `WriteBatch` (exactly what one written version created — keeps per-op fsync cost bounded).
  Contract: reachable ⇒ durable before any doc records it.
- **Diff/merge engines (pinned):** `unified_diff` / `unified_diff_sections` — `imara-diff` for the
  algorithm, the unified formatting owned here so the committed goldens stay byte-stable;
  sections concatenate byte-identically for file-boundary truncation. `merge_file` — the per-file
  diff3 execution over `diffy` (pinned exact; conflict bytes are a consent artifact locked by a
  golden), diff3-style markers lengthened until unique, non-UTF-8 never line-merged, size caps
  checked before allocation. Bytes are never normalized.
- **The lifecycle-fence primitives (`fence.rs`)** — the dumb byte ops the plane's GC fence drives:
  `stage` (quarantine), `install_object_durable`, `commit_durable` (tree from already-installed
  ids via the low-level plumbing editor, so an offloaded blob's `(path,mode,git_oid)` is carried
  without its bytes entering git), `delete_loose_object`, `read_staged_blob`, `object_exists`.
  These are self-durable and return the path set they synced.

## The large-object store

`LocalLargeStore` behind the `LargeObjectStore` trait: content-addressed `put`/`get`/`exists`/
`delete` keyed by `blob_id = sha256(bytes)`, sharded finals + same-filesystem tmp staging,
crash-safe two-phase install, verify-on-read. One store per workspace (`large_root/<ws>/`) —
isolation is the path; routing + `location` dispatch live in `plane-store`. An S3-compatible
remote backend would be a second impl of this trait.

Dependencies: `gix` (plumbing-only), `imara-diff`, `diffy` (pinned exact), `topos-core`,
`thiserror`.
