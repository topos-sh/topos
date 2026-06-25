# `topos-gitstore` — the gix object mechanics + the large-object store

The shared dumb byte layer over `gix`: object read/write, dedup, **tree → empty-staging-dir render** (never
an in-place reset), diff/diff3 execution, and the **sha256-id ↔ git-OID mapping** (a tested invariant — git
OIDs are SHA-1, an internal detail; the version id is always our own sha256).

**Re-verifies bytes → expected sha256 on every read** (never trusts gix's object id). The untrusted tree
renderer is fuzzed. Holds **no access control**.

## The `LargeObjectStore` seam

A small content-addressed `put` / `get` / `exists` / `delete` trait (streaming, verify-on-read, crash-safe
two-phase install: temp → fsync → recompute-sha256 == `blob_id` → commit) for the size-routed large-blob
offload. The **v0 impl is local-filesystem** (sharded `objects/aa/bb/<sha256>` dirs). Keyed by
`blob_id = sha256(raw bytes)` — **no pointer files**. The S3-compatible remote impl is a later no-op
extraction behind this trait.

Isolates the large, pre-1.0 `gix` dependency behind a small surface shared by the plane and the client.

Dependencies: `gix`, `topos-core`, `topos-types`.
