# `topos-plane` — the OSS vault (library-first)

PURE BYTE CUSTODY. The vault listens **internal-network-only** with ONE caller — the composing
product app — authenticated by the internal bearer token, and treats every request as
PRE-AUTHORIZED (authorization/protection/entitlement decided app-side, once). It must never be
publicly reachable: no published port, no public router.

## lib

- **Construction:** `PlaneConfig { database_url, store: StoreBackend, staging_root }` +
  `PlaneState::open(cfg)` (leak-free — the composer names no `plane_store` type; `StoreBackend`
  is `Local { root }` or `S3 { endpoint, bucket, keys, region }`); `PlaneState::new(Arc<Authority>)`
  for tests. `PlaneState::with_internal_token(token)` arms the custody lane (sha256-only
  retention; unarmed, every `/internal/v1/*` route answers the uniform 404).
- **`router(state)`** — the whole HTTP surface: `GET /healthz` (unauthenticated liveness) + the
  bearer-gated `/internal/v1` custody lane; anything else is a uniform JSON 404. Request-level
  tracing wraps everything (matched route template, never a raw path).
- **The internal custody lane** (`routes/internal.rs`) — lane-local snake_case DTOs (deliberately
  NOT in `topos-types`, NOT in the committed OpenAPI). Per workspace/bundle: ingest-without-move
  (`…/versions`, the propose path), `…/publish` (ingest + commit + CAS, `expected_generation`),
  `…/pointer` (CAS to an existing version — the approve path), `…/revert` (forward commit),
  verified reads (`current` · version meta/listing · object bytes · first-parent `log`),
  `…/versions/{id}/purge`, bundle/workspace delete, and `GET /internal/v1/storage` (per-workspace
  stored-byte accounting). Errors: 400, the uniform 404, 409 `CONFLICT`/`TARGET_PURGED`/
  `POINTED_AT`, 500 `INTEGRITY`/`INTERNAL`; 401 only on a wrong bearer.
- **`routes/door.rs`** — contract-ONLY `#[utoipa::path]` stubs (never routed) describing the
  PUBLIC session lane the web app serves; `openapi()` is generated from them so the committed
  contract pins that wire in one artifact.
- **`maintenance.rs`** — `spawn_maintenance` / `run_maintenance_pass`: recovery → janitor →
  per-workspace GC; the composition root spawns it once, `router()` deliberately does not.

## bin

Two commands, no trust logic. **serve** (the default — a bare `topos-plane`, flags and env as
ever): parse config (`TOPOS_PLANE_BIND`, `DATABASE_URL`, `TOPOS_PLANE_STORE` local|s3 with
`TOPOS_PLANE_GIT_ROOT` as the local root and `TOPOS_PLANE_S3_*` for a bucket, `TOPOS_PLANE_TMP`
for the ephemeral staging dir, `TOPOS_PLANE_INTERNAL_TOKEN`, `TOPOS_PLANE_GC_INTERVAL_SECS`; pool
tuning via `TOPOS_PLANE_DB_*`), open the state, arm the lane, spawn maintenance, serve.
**import-local** (`src/import.rs`): the one-shot cutover from the pre-object-store on-disk layout
— run with the vault stopped; prints per-workspace counts + PASS/FAIL; idempotent.

Dependencies: `plane-store`, `topos-core` (sha256 for the token hash), `topos-types`
(contract-derives — one of the two contract producers), `axum`, `utoipa`, `tokio`, `tracing`,
`clap`, `serde`/`serde_json`, `base64`.
