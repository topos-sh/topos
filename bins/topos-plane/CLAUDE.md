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

**Implemented — the enrollment protocol GLUE the verification routes (landing next) drive** (`src/enroll/`).
No durable state, no issuance decision (every credential/identity decision is `plane-store::Authority`'s):

- **The passcode mailer seam** (`enroll/mailer.rs`) — a `pub(crate)` `Mailer` trait (SYNC + dyn-compatible,
  no async-trait: the handler runs the blocking send on `spawn_blocking`, fire-and-forget so neither the body
  nor its latency leaks whether an address was rostered), with `SmtpMailer` (lettre, blocking SMTP + rustls),
  `NoopMailer` (the no-SMTP self-host default — silently drops, so the bootstrap won't advertise the passcode
  method), and a recording `FakeMailer` (test-gated). **Redaction:** the code rides in a `Passcode`
  whose hand-written `Debug` is `<redacted>`; `SmtpMailer` / `SmtpConfig` `Debug` omit the transport / creds.
- **The OIDC connector** (`enroll/oidc.rs`, behind `enroll-oidc`, **DEFAULT-OFF** — a default build resolves
  NO oauth2/openidconnect/reqwest). A minimal single-provider id-token flow: `start` builds the authorize
  redirect (PKCE + CSRF state + nonce, the `user_code` bound into `state`); `callback` runs SERVER-SIDE
  (validate state → exchange code → validate the id_token via JWKS/nonce → confirm the session). **The
  id/access token is consumed here and dropped — it NEVER returns to the agent; only the
  proven email crosses to `confirm_external_identity`.** A regression test pins the callback's Ok type to `()`.
- **`PlaneState` extension** — `mailer: Arc<dyn Mailer>` + `enroll: Arc<EnrollConfig>`; the public
  `with_enroll_config` builds the mailer INTERNALLY (SmtpMailer when SMTP is set, else NoopMailer), mirroring
  `with_rate_limit` + the internal Limiter. A test-gated `with_mailer` shim injects the FakeMailer (a
  check-arch guard keeps the `test-fixtures` feature off in production).

**Planned (lands next):** the **enrollment + governance HTTP routes** that wrap the now-built issuance core +
this glue — the request/response DTOs, the verification-page HTML, the passcode-send + OIDC start/callback
handlers, the workspace-policy mutation — so today it's the **passcode floor + OIDC behind a feature, no
routes yet**; plus the **`review-required` workspace policy** + **governance** mutation routes (roster /
policy); the **audit outbox** read via durable cursors; **TLS termination** (loopback HTTP today — terminate
at a reverse proxy).

## bin

A thin `axum` `main` (composition root only — no trust logic): parses config (bind addr / db / git-root /
large-root / plane-key / enrollment secret / base URL / mode / SMTP relay), opens the `Authority` (now wiring
`with_enrollment_config` so it mints real credentials) + builds the `EnrollConfig` for `PlaneState`, and
serves `router(state)`. Under `enroll-oidc` it reads `TOPOS_PLANE_OIDC_*` for the connector.

## The litmus for what belongs in this lib

*Would you expose it even if there were no private product?* `router()`, the ops, the policy — **yes**
(self-hosters, integrators, and the tests all want them). An extension/callback hook that serves only a
closed product — **no**, and there is none here. A separate private program IMPORTS this library and
composes it (its routes call these public ops; its middleware sits in front); the library never calls back
into it. Anything that must run inside the publish transaction is, by definition, trust logic and lives
here, parameterized by config (like the `review-required` boolean) — never injected from outside.

Dependencies: `plane-store`, `topos-core`, `topos-types`, `axum`, tower middleware, `oauth2` /
`openidconnect` (feature-gated), `lettre`, `tracing`.
