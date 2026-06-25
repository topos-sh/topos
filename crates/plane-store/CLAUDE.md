# `plane-store` — the server authority boundary

**A crate so that raw access is private.** Owns ALL plane SQL — **raw `sqlx` + `sqlx::migrate` + thin
per-table repos, NO ORM** — with SQLite + Postgres as concrete `mod sqlite` / `mod pg` behind one sealed
conformance facade (never `sqlx::Any`). Raw `sqlx` stays `pub(crate)`-private. Also owns git-object access
(via `topos-gitstore`), skill-scoped authorization, the **complete atomic publish transaction** (one
serializable txn spanning pointer · authz · proposals · receipts · object-presence · leases · the
in-process Ed25519 signer, calling `topos-core` for every pure decision), lifecycle/GC, and
roster/authz/tombstones.

## The privacy boundary IS the security mechanism

Raw SQL + raw git reads are `pub(crate)`-private; the only public surface is **authorized authority
operations**. No code outside this crate can bypass the access check to read a bare object — that is
unbypassable by construction. (This is misuse-prevention by encapsulation; it is not isolation against
malicious same-process code.)

Owns the size-routing + object-presence dispatch: it drives the `LargeObjectStore` at publish-migrate and
GC unlink, and de-references each object **through the access check** — the large-object store is never
read by bare hash outside this crate's authorized ops.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, raw `sqlx`, `ed25519-dalek`.
