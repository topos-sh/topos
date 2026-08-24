# `topos-gitstore` — the gix object mechanics + the in-memory loose-object codec

The shared dumb byte layer over `gix`: object read/write, a recursive byte-oriented tree render,
the sha256-id ↔ git-OID mapping carried as a ref name (git OIDs are SHA-1, an internal detail;
the version id is always our own sha256), and the PURE in-memory codec the vault stores its
objects with. Role rule: **no network, no async, no SQL** — `std::fs` stays (the CLIENT keeps one
bare repo per bundle; the vault holds no repo at all). Bundle-generic — it never asks what a
bundle is.

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
- **The lifecycle-fence primitives (`fence.rs`)** — the dumb byte ops a repo-backed fence drives
  (client-side today): `stage` (quarantine), `install_object_durable`, `commit_durable`,
  `delete_loose_object`, `read_staged_blob`, `object_exists`. Self-durable; return the synced
  path set.

## The in-memory codec (`codec.rs`)

Pure encode/decode of git loose objects WITHOUT a repository — what the vault frames its store
objects with: `encode_blob`/`blob_git_oid`, `encode_tree` (nested paths → subtree objects, git
tree order, the same component validation as the repo-backed editor), `encode_commit` (the SAME
reproducible frame `Store::commit` writes — fixed committer, epoch-zero time — parity pinned by
test), and `decode_loose`/`decode_commit`/`decode_tree` with verify-on-decode (a frame that does
not hash to the id that named it is refused typed).

Dependencies: `gix` (plumbing-only), `flate2` (the codec's zlib), `imara-diff`, `diffy` (pinned
exact), `topos-core`, `thiserror`.
