# `topos-plane` — the OSS vault (library-first)

PURE BYTE CUSTODY. The vault listens **internal-network-only** with ONE caller — the composing
product app — authenticated by the internal bearer token, and treats every request as
PRE-AUTHORIZED (authorization/protection/entitlement decided app-side, once). It must never be
publicly reachable: no published port, no public router.

## lib — the composable surface

- **The leak-free construction path:** `PlaneConfig { database_url, git_root, large_root }` +
  `PlaneState::open(cfg)` (builds the `plane-store::Authority` internally; the composer names no
  `plane_store` type). `PlaneState::new(Arc<Authority>)` stays the explicit test/advanced path.
  `PlaneState::with_internal_token(token)` arms the custody lane (sha256-only retention; unarmed,
  every `/internal/v1/*` route answers the uniform 404 so a composition can never expose an
  unauthenticated custody lane).
- **`router(state)`** — the whole HTTP surface: `GET /healthz` (unauthenticated liveness) + the
  bearer-gated `/internal/v1` custody lane; anything else is a uniform JSON 404. Request-level
  tracing (method + matched route template + status + latency; never a raw path) wraps everything.
- **The internal custody lane** (`routes/internal.rs`) — lane-local snake_case DTOs (deliberately
  NOT in `topos-types`, NOT in the committed OpenAPI):
  - `POST /internal/v1/workspaces/{ws}/bundles/{bundle}/versions` — ingest + commit WITHOUT a
    pointer move (the propose path). Body `{files:[{path, mode, content_base64}], parent?,
    attribution, message}` → `{version_id, commit_id, bundle_digest, deduped}` (idempotent per
    content).
  - `POST …/publish` — the composite: ingest + commit + CAS, one flow. Adds
    `expected_generation: Option<u64>` (absent = genesis) → the commit answer + `pointer
    {version_id, generation, moved_at_ms, moved_by_display, replayed}`.
  - `POST …/pointer` — CAS to an EXISTING version (the approve path): `{version_id,
    expected_generation?, attribution}`.
  - `POST …/revert` — forward commit `{tree: target.tree, parents: [current]}` + CAS:
    `{to_version_id, expected_generation, attribution}` (the revert message is server-constructed
    + deterministic, so retries re-derive the identical id).
  - `GET …/current` · `GET …/versions/{version_id}` (meta + file listing) ·
    `GET …/objects/{object_id}` (verified bytes, octet-stream) · `GET …/log?limit=N`
    (the first-parent chain from current, capped).
  - `GET /internal/v1/storage` — every workspace's stored byte total (`present` custody only,
    ordered by workspace id): `{workspaces: [{workspace_id, stored_bytes}]}` — the operational
    accounting read.
  - `POST …/versions/{version_id}/purge` (`{attribution}`) · `DELETE …/bundles/{bundle}` ·
    `DELETE /internal/v1/workspaces/{ws}`.
  - Errors: 400 `BAD_REQUEST`/`REJECTED`, the uniform 404 `NOT_FOUND`, 409 `CONFLICT` (carrying the
    live `generation` + `version_id`) / `TARGET_PURGED` / `POINTED_AT`, 500 `INTEGRITY`/`INTERNAL`
    (chains logged server-side, never on the wire). 401 only on a wrong bearer; an UNARMED lane is
    404-invisible.
- **`routes/door.rs`** — contract-ONLY stubs (never routed) carrying the `#[utoipa::path]`
  annotations for the PUBLIC device lane the product app serves: device-auth start/poll (the
  gh-style flow — on approval the device code is promoted to the device's ONE bearer credential),
  publish/propose/revert/review, the reads (current/catalog/version/object/proposals/delivery/
  me/channels/inbox/log/reach), the row ops, notices-ack, invitations, the browser-free
  device-link lane (`GET`/`POST /v1/device/link` — person-scoped; an enrolled device joins a
  further workspace with no second ceremony), and the global device self-revoke
  (`DELETE /v1/device`).
  `openapi()` (emitted to `contracts/openapi/` by `xtask`) is generated from these stubs; the
  internal custody lane stays OUT of the committed contract.
- **The storage-maintenance scheduler** (`maintenance.rs`): `spawn_maintenance(state, every)` /
  `run_maintenance_pass(state)` — recovery → janitor → per-workspace GC, faults logged + tallied,
  never crashing the loop. The composition root spawns it once; `router()` deliberately does not.

## bin

A thin `axum` main: parse config (`TOPOS_PLANE_BIND`, `DATABASE_URL`, `TOPOS_PLANE_GIT_ROOT`,
`TOPOS_PLANE_LARGE_ROOT`, `TOPOS_PLANE_INTERNAL_TOKEN`, `TOPOS_PLANE_GC_INTERVAL_SECS`; pool tuning
via `TOPOS_PLANE_DB_*`), open the state, arm the lane, spawn maintenance, serve. No subcommands, no
trust logic.

Dependencies: `plane-store`, `topos-core` (sha256 for the token hash), `topos-types`
(contract-derives — this crate is one of the two contract producers), `axum`, `utoipa`, `tokio`,
`tracing`, `clap`, `serde`/`serde_json`, `base64`.
