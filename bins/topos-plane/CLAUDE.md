# `topos-plane` — the OSS plane (library-first)

## lib (`plane-core`) — the composable surface a downstream plane builds on

**Implemented** — the HTTP surface over the built `plane-store::Authority`:

- **The leak-free construction surface (what a downstream plane composes without naming `plane-store`):**
  `pub struct PlaneConfig` (plain/owned fields only — `mode: String`, `database_url: String`, paths, `Option<SmtpConfig>`) +
  `pub async fn PlaneState::open(cfg: PlaneConfig) -> anyhow::Result<PlaneState>`, which builds the
  `Authority` + the (now crate-private) enrollment config **internally**. The **bin dogfoods it** (one
  construction path — `main.rs` names no `plane_store` type). A `no_run` doc-test + a runtime parity test pin
  it. `PlaneState::new(Arc<Authority>)` stays the explicit test/advanced path that does name `Authority`.
- `pub async fn PlaneState::set_review_required(&self, workspace_id: &str, review_required: bool) ->
  anyhow::Result<()>` — the `review_required` workspace-policy toggle, **set via the public API** (a leak-free
  wrapper over `Authority::set_review_required`: the id is parsed + both errors stringified internally). A
  composing admin route calls it; it is **not** itself device-op-signed (the device-signed `PUT /policy`
  governance route is later work).
- `pub fn router(state: PlaneState) -> axum::Router` — the **single** composed surface a downstream plane
  imports verbatim (the limiter lives inside `PlaneState`). There is **no** `PlaneExtension`/callback/fork
  hook (a check-arch guard also proves the production build never enables the test-only seeding feature).
- Thin handlers (`routes/*`, `wire/*`) over the frozen routes: `GET /v1/current/{read_token}` (conditional
  GET, `ETag = "<epoch>.<seq>"`, a commit-sensitive 304 via a `Topos-Known-Version-Id` header),
  `GET /v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}` and the sibling
  `GET /v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` (both skill-scoped via an opaque read
  credential, **404-not-403**, never by bare hash), the proposals-listing read
  `GET /v1/workspaces/{ws}/skills/{skill}/proposals` (the OPEN proposals' `{version_id, base, created_at}` —
  count + handles only, no bytes/roles; same Bearer-read scope + 404-not-403 + the shared `open ∧ base==current`
  staleness clause, so a staled proposal vanishes; a mutable list, so `must-revalidate`, no ETag), and the
  device-signed writes `POST /v1/publish|/v1/proposals|/v1/reverts|/v1/reviews`. Each handler is parse → call
  the authority → serialize: **no trust decision, no raw object read, no client-asserted principal** in a
  handler.
- **The enrollment + governance HTTP surface** (`routes/{bootstrap,enroll,governance,oidc}.rs`): the
  unauthenticated TOFU bootstrap `GET /i/{token}` (the workspace + the plane signing root to pin; **no bytes,
  no role**; a dead invite ⇒ 404); the enrollment flow `POST /v1/device/authorize`, `POST /v1/device/token`,
  `GET /v1/enroll/verify/{user_code}`, `POST /v1/enroll/passcode` (the returned code is sent fire-and-forget on
  `spawn_blocking`, so the constant-shaped ack never leaks whether an address was rostered),
  `POST /v1/enroll/passcode/confirm`, the central **redeem** `POST /v1/workspaces/{ws}/devices` (the enroll
  possession sig rides the `Topos-Device-Signature` header; mints per-skill read creds, **never a user
  token**), and `POST /v1/admin-claim`; and the governance mutations `POST /v1/invites`,
  `PUT|DELETE /v1/workspaces/{ws}/roster/{email}`, `DELETE /v1/workspaces/{ws}/devices` (each a governance-frame
  signature → ONE authority op). A confirmed identity is **never `Principal::parse`d in a handler** (it comes
  from a server-trusted row in the authority); a target email is op data, bound into the signed frame. The
  OIDC routes (`POST /v1/enroll/oidc/{start,callback}`) are behind the default-off `enroll-oidc` feature (so
  the committed OpenAPI contract excludes them).
- The wire mapping (`wire/map.rs`): a *read* enrollment step (bootstrap / device-auth / verification /
  passcode) → a plain typed DTO (a miss is the route's indistinguishable 404); every terminal protocol outcome
  of an op_id-carrying *write* (publish/propose/revert/review, and the redeem/admin-claim/invite/roster/revoke
  envelopes) → **HTTP 200** carrying the canonical all-outcome receipt/envelope (a failure adds the flat wire
  error + `next_actions`; a governance role-DENIED is a 200+DENIED — the actor is authenticated, nothing to
  hide). Non-2xx only for transport/auth/integrity (400/404/429/500). `op_id` idempotent retry replays
  byte-identically.
- A minimal **in-process token-bucket rate limiter** (`rate_limit.rs`, no extra dependency) that freezes the
  429 wire shape (`Retry-After` + a `RETRYABLE_FAILURE` envelope); on by default, env-disableable.
- **The self-host operator policy route** (`routes/policy.rs`): `PUT
  /v1/workspaces/{ws}/policy/review-required` sets the `review-required` workspace policy through
  `Authority::set_review_required` (enforcement stays in the write path — a direct publish under the gate
  fails typed). Authenticated by the plane's **admin bearer token** (`--admin-token` /
  `TOPOS_PLANE_ADMIN_TOKEN`; the state retains only its sha256 via `PlaneState::with_admin_token`): with NO
  token configured the route answers **404** (invisible — a downstream composition that merges
  `router(state)` without setting a token can never expose an unauthenticated toggle on its open `/v1/`
  lane); configured-but-wrong is an honest **401** (the one scoped exception to the 404-not-403 read
  posture — an operator's own secret, not an object-existence oracle); success is **204**, an idempotent
  set. NOT device-op-signed (the device-signed governance variant needs a new kernel frame — later work).
- A generated **OpenAPI** (`openapi()`, utoipa) emitted to `contracts/openapi/` and folded into the
  `gen-schema` drift gate.

**Implemented — the enrollment protocol GLUE the routes drive** (`src/enroll/`). No durable state, no
issuance decision (every credential/identity decision is `plane-store::Authority`'s):

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
- **`PlaneState` extension** — `mailer: Arc<dyn Mailer>` + `enroll: Arc<EnrollConfig>` (+ a feature-gated
  `oidc: Option<Arc<OidcConfig>>` under `enroll-oidc`); the **crate-private** `with_enroll_config` builds the
  mailer INTERNALLY (SmtpMailer when SMTP is set, else NoopMailer), mirroring `with_rate_limit` + the internal
  Limiter — `PlaneState::open` calls it from a leak-free `PlaneConfig` (`EnrollConfig` is now `pub(crate)`,
  so it never crosses the public API). The feature-gated `with_oidc_config` loads the connector. A test-gated
  `with_mailer` shim injects the FakeMailer (a check-arch guard keeps the `test-fixtures` feature off in
  production).

**Planned (lands later):** the **verification-page HTML** (the routes above are the JSON surface a composing
web layer renders; the page itself is a separate surface); the **device-signed `PUT /policy` variant** (the
admin-token operator route is built — see above; a device-op-signed governance route over the same policy
needs a new kernel frame, still later work); the **audit outbox** read via durable cursors; **TLS
termination** (loopback HTTP today — terminate at a reverse proxy).

## bin

A thin `axum` `main` (composition root only — no trust logic): parses config (bind addr / database URL / git-root /
large-root / plane-key / enrollment secret / base URL / mode / SMTP relay / the optional operator
admin token, which enables the policy route), resolves its two bin-local
marshals (the base URL default + the 5-or-none SMTP relay), then builds the serving state through the
**single leak-free constructor** `PlaneState::open(PlaneConfig { .. })` — which opens the `Authority`,
loads the plane key + enrollment secret, and builds the enrollment config INTERNALLY (the bin names no
`plane-store` type, dogfooding the same path a downstream plane uses) — and serves `router(state)`. Under
`enroll-oidc` it reads `TOPOS_PLANE_OIDC_*` and loads the connector onto `PlaneState` (`with_oidc_config`) so
the `/v1/enroll/oidc/*` routes can drive it.

## The litmus for what belongs in this lib

*Would you expose it even if there were no private product?* `router()`, the ops, the policy — **yes**
(self-hosters, integrators, and the tests all want them). An extension/callback hook that serves only a
closed product — **no**, and there is none here. A separate private program IMPORTS this library and
composes it (its routes call these public ops; its middleware sits in front); the library never calls back
into it. Anything that must run inside the publish transaction is, by definition, trust logic and lives
here, parameterized by config (like the `review-required` boolean) — never injected from outside.

Dependencies: `plane-store`, `topos-core`, `topos-types`, `axum`, tower middleware, `oauth2` /
`openidconnect` (feature-gated), `lettre`, `tracing`.
