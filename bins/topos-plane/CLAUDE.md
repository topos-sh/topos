# `topos-plane` — the OSS plane (library-first)

## lib (`plane-core`) — the composable surface a downstream plane builds on

**Implemented** — the HTTP surface over the built `plane-store::Authority`:

- `pub fn router(state: PlaneState) -> axum::Router` — the **single** composed surface a downstream plane
  imports verbatim (the limiter lives inside `PlaneState`). There is **no** `PlaneExtension`/callback/fork
  hook (a check-arch guard also proves the production build never enables the test-only seeding feature).
- Thin handlers (`routes/*`, `wire/*`) over the frozen routes: `GET /v1/current/{read_token}` (conditional
  GET, `ETag = "<epoch>.<seq>"`, a commit-sensitive 304 via a `Topos-Known-Version-Id` header),
  `GET /v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}` and the sibling
  `GET /v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` (both skill-scoped via an opaque read
  credential, **404-not-403**, never by bare hash), and the device-signed
  writes `POST /v1/publish|/v1/proposals|/v1/reverts|/v1/reviews`. Each handler is parse → call the authority
  → serialize: **no trust decision, no raw object read, no client-asserted principal** in a handler.
- The wire mapping (`wire/map.rs`): every terminal protocol outcome → **HTTP 200** carrying the canonical
  all-outcome receipt (a failure outcome adds the flat wire error + `next_actions`); non-2xx only for
  transport/auth/integrity (400/404/429/500). `op_id` idempotent retry replays byte-identically.
- A minimal **in-process token-bucket rate limiter** (`rate_limit.rs`, no extra dependency) that freezes the
  429 wire shape (`Retry-After` + a `RETRYABLE_FAILURE` envelope); on by default, env-disableable.
- A generated **OpenAPI** (`openapi()`, utoipa) emitted to `contracts/openapi/` and folded into the
  `gen-schema` drift gate.

**Planned (lands later):** the **`review-required` workspace policy** mutation route + the **governance**
mutation routes (roster / policy); the **enrollment** state machine (device-flow / passcode / magic-link /
invite-chain / one generic OSS OIDC connector — concrete modules behind a cargo feature) + invite +
read-credential **minting**; the **audit outbox** read via durable cursors; **TLS termination** (loopback
HTTP today — terminate at a reverse proxy).

## bin

A thin `axum` `main` (composition root only — no trust logic): parses config (bind addr / db / git-root /
large-root / plane-key), opens the `Authority`, and serves `router(state)`.

## The litmus for what belongs in this lib

*Would you expose it even if there were no private product?* `router()`, the ops, the policy — **yes**
(self-hosters, integrators, and the tests all want them). An extension/callback hook that serves only a
closed product — **no**, and there is none here. A separate private program IMPORTS this library and
composes it (its routes call these public ops; its middleware sits in front); the library never calls back
into it. Anything that must run inside the publish transaction is, by definition, trust logic and lives
here, parameterized by config (like the `review-required` boolean) — never injected from outside.

Dependencies: `plane-store`, `topos-core`, `topos-types`, `axum`, tower middleware, `oauth2` /
`openidconnect` (feature-gated), `lettre`, `tracing`.
