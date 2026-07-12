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
  composing admin route calls it; it is **not** itself device-credential authenticated (a
  device-credential-authenticated `PUT /policy` governance route is later work).
- `pub fn router(state: PlaneState) -> axum::Router` — the **single** composed surface a downstream plane
  imports verbatim (the limiter lives inside `PlaneState`). There is **no** `PlaneExtension`/callback/fork
  hook (a check-arch guard also proves the production build never enables the test-only seeding feature).
- Thin handlers (`routes/*`, `wire/*`) over the frozen routes — EVERY device-lane route now
  authenticates by the ONE **workspace credential** in the `Authorization: Bearer` header (never a body
  field, never a path segment): `GET /v1/workspaces/{ws}/skills/{skill}/current` (conditional
  GET, `ETag = "<epoch>.<seq>"`, a commit-sensitive 304 via a `Topos-Known-Version-Id` header — the old
  token-in-path `/v1/current/{read_token}` shape is gone with the tokens),
  `GET /v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}` and the sibling
  `GET /v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` (membership-scoped,
  **404-not-403**, never by bare hash), the proposals-listing read
  `GET /v1/workspaces/{ws}/skills/{skill}/proposals` (the OPEN proposals' `{version_id, base, created_at}` —
  count + handles only, no bytes/roles; same Bearer scope + 404-not-403 + the shared `open ∧ base==current`
  staleness clause, so a staled proposal vanishes; a mutable list, so `must-revalidate`, no ETag), the
  **workspace-catalog read** `GET /v1/workspaces/{ws}/skills` (the member-scoped catalog —
  a missing/blank credential folding to the uniform 404;
  calls `Authority::list_skills_device` → `WireSkillIndex`; the FIRST HTTP-routed member-scoped read, serving
  cloud AND self-host), **the DELIVERY read** `GET /v1/workspaces/{ws}/delivery` (→
  `Authority::delivery` → `WireDelivery`: the entitled set — skill id + catalog name + resolved
  protection + pinned version + `via` attribution — plus the person's detached ids, the unacked
  notices feed, and the open-proposal count; `Cache-Control: no-store` — per-device, hot, never
  cacheable), **the fleet report** `PUT /v1/workspaces/{ws}/report` (`WireAppliedReport` → 204;
  the device's post-reconcile applied snapshot, small-body-capped), and the device-credential writes
  `POST /v1/publish|/v1/proposals|/v1/reverts|/v1/reviews` (bodies carry NO credential material — the
  Bearer credential is resolved in-transaction by registry-row lookup; keeping the secret out of bodies
  keeps it out of receipt request identities and the client's persisted op-WAL). Each handler is parse → call
  the authority → serialize: **no trust decision, no raw object read, no client-asserted principal** in a
  handler.
- **The enrollment + governance HTTP surface** (`routes/{bootstrap,enroll,governance,oidc}.rs`): the
  unauthenticated bootstrap `GET /i/{token}` (the workspace + the plane API base to dial; **no bytes,
  no role, no trust root**; a dead invite ⇒ 404 — and the route now ALSO serves one-time admin-CLAIM links, probed after
  the invite table: `enrollment_method: "admin_claim"`, no skills, the same uniform 404 for
  consumed/expired/unknown; the two live in disjoint tables, so a token never crosses doors). The route
  **content-negotiates**: an Accept asking for JSON (or no Accept) gets the versioned `BootstrapData`
  contract; anything else — curl's `*/*`, a browser, an agent's web fetch — gets a markdown
  **agent-instruction document** served as `text/plain` so browsers display it inline — the document IS
  the browser face, no HTML page exists (`routes/bootstrap_doc.rs`, a pure renderer over the same
  authority read: the human paste-this-to-your-agent hand-off first, then install `topos` if missing via
  the checksummed installer line, `follow` the link, surface the verification URL to the human, land
  offers per-digest; the CLAIM variant warns first-redeemer-becomes-owner and NEVER echoes the token or
  link — the same custody rule as the JSON `token_id` placeholder).
  Both 200s are `Cache-Control: no-store` + `Vary: accept` + `X-Robots-Tag: noindex`; errors stay the
  uniform JSON envelope on every Accept; the
  enrollment flow `POST /v1/device/authorize` (now intent-dispatching: an `enroll` start needs its
  `invite_token`; a `standup` start — explicit intent, or no invite at all — opens a workspace-less session
  on a hosted plane [self-host ⇒ 404], answers with the high-entropy code + `verification_uri_complete` +
  the plane block [API base + posture + method, no trust root], and a contradictory intent/invite body is a 400), `POST /v1/device/token`
  (a granted poll now carries the `{workspace_id, display_name}` context a standup client lacks),
  `GET /v1/enroll/verify/{user_code}` (now disclosing the session's `intent`, so a web page renders
  join-copy vs create-copy; a standup session's workspace name is `""` until approval),
  `POST /v1/enroll/passcode` (the returned code is sent fire-and-forget on
  `spawn_blocking`, so the constant-shaped ack never leaks whether an address was rostered),
  `POST /v1/enroll/passcode/confirm`, the central **redeem** `POST /v1/workspaces/{ws}/devices` (the grant is
  the bearer credential in the body, checked against the device public key it is bound to — no signature;
  mints the device's ONE **workspace credential**, **never a user token, never a per-skill token**), and
  `POST /v1/admin-claim`; and the governance
  mutations `POST /v1/invites`, `PUT|DELETE /v1/workspaces/{ws}/roster/{email}`,
  `DELETE /v1/workspaces/{ws}/devices` (each the acting Bearer workspace credential + op → ONE authority
  op; the revoke's TARGET stays named by its non-secret `device_key_id`). A confirmed
  identity is **never `Principal::parse`d in a handler** (it comes from a server-trusted row in the
  authority); a target email is op data. The
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
- **The storage-maintenance scheduler** (`maintenance.rs`): `plane-store` mandates that the composing server
  run the recovery sweep + quarantine janitor on startup and, with a per-workspace GC pass, periodically —
  and holds no scheduler. This module is that half, in the LIBRARY so every composition owns it the same
  way: `pub fn spawn_maintenance(state, every) -> JoinHandle<()>` (one pass immediately — the mandated
  startup run — then one per interval; `every` clamped ≥ 1 s) over `pub async fn run_maintenance_pass(state)
  -> MaintenancePass` (recovery → janitor → `Authority::workspaces()` → `run_gc` per workspace; `now` is the
  same epoch-ms wall clock the wire layer stamps, re-read per step). Every step error is
  `tracing::error!`-logged with its FULL source chain and tallied (`MaintenancePass.faults`) — a fault never
  crashes the loop or the server, and one faulting workspace never starves the rest. `router()` deliberately
  does NOT start it (building a router is pure composition; spawning is the composition root's one-time
  runtime decision — the OSS bin spawns it; a downstream plane makes the same call, or drives the pass from
  its own scheduler).
- **Server diagnostics** (`router.rs` + `wire/error.rs`, no new dependency): request-level tracing — one
  `request` span per request (method + the matched ROUTE TEMPLATE, never the raw path, which carries the
  invite token on `/i/{token}` — the workspace credential rides only the never-logged Authorization
  header; unmatched logs the
  constant `(unmatched)`) closed by an `info` event with status + latency, layered OUTERMOST in `router()`
  so every composition gets it and 429s are recorded too. The 500 mapper honors `plane-store`'s "retained
  for server-side diagnostics" error contract: an `AuthorityError::{Integrity, Internal}` is
  `tracing::error!`-logged with its full flattened `source()` chain (inside the request span, so it
  correlates) BEFORE flattening to the schema-pinned flat wire body — chain detail never crosses the wire.
- **The self-host operator policy route** (`routes/policy.rs`): `PUT
  /v1/workspaces/{ws}/policy/review-required` sets the `review-required` workspace policy through
  `Authority::set_review_required` (enforcement stays in the write path — a direct publish under the gate
  fails typed). Authenticated by the plane's **admin bearer token** (`--admin-token` /
  `TOPOS_PLANE_ADMIN_TOKEN`; the state retains only its sha256 via `PlaneState::with_admin_token`): with NO
  token configured the route answers **404** (invisible — a downstream composition that merges
  `router(state)` without setting a token can never expose an unauthenticated toggle on its open `/v1/`
  lane); configured-but-wrong is an honest **401** (the one scoped exception to the 404-not-403 read
  posture — an operator's own secret, not an object-existence oracle); success is **204**, an idempotent
  set. NOT device-credential authenticated (a device-credential-authenticated governance variant over this
  policy is later work).
- A generated **OpenAPI** (`openapi()`, utoipa) emitted to `contracts/openapi/` and folded into the
  `gen-schema` drift gate.
- **The backup/restore epoch bump** (`restore_cmd.rs`): `PlaneState::restore_bump_epochs(workspaces,
  epoch_at_least)` — the leak-free wrapper (plain `String`/`u64` in, a plain `EpochBumpSummary` out, ids
  parsed at this edge) over `Authority::restore_bump_epochs`, which rewrites every selected `current`
  pointer one epoch forward (same commit, same seq) so a reused `(epoch, seq)` tuple after a database restore
  can't confuse the proposal-staleness predicate or an in-flight CAS / conditional GET (concurrency
  correctness, not follower-alarm avoidance).
- **The workspace-standup wrappers** (`standup_cmd.rs`) — the leak-free, deliberately LIB-ONLY surface for
  the PRIVILEGED genesis ops (there is NO OSS HTTP route for any of them; the bin's `mint-claim`
  subcommand and a downstream composition's authenticated admin routes are the callers):
  `PlaneState::mint_admin_claim` (returns the full one-time `/i/` claim link ONCE — the bearer owner
  capability is never logged and every `Debug` redacts it; a cloud-mode plane requires `--owner-email`),
  `create_workspace` (a `CreateWorkspaceSummary`: Created/Replayed with the deterministic self-invite
  link, or a typed Denied — the cap, a reused request id), `approve_standup` (an
  `ApproveStandupSummary`: Approved / idempotent AlreadyApproved / typed Denied / the uniform NotFound),
  and `approve_session` (the member/owner web-approve leg over an enroll session, with the
  first-writer-wins semantics surfaced: same-email re-approve ⇒ Confirmed, anything else ⇒ NotFound).
  Every wrapper parses the plane's deployment mode STRICTLY — a mode string the constructor could only
  warn-fallback is a typed refusal here (fail closed), so an operator typo can never decide what mode a
  workspace is born with.
- **The session-roster wrappers** (`roster_cmd.rs`) — the same leak-free, LIB-ONLY surface for the
  web-session membership ops (a downstream composition's authenticated admin routes call them with a
  session-verified acting email; there is NO OSS HTTP route): `PlaneState::invite_members` (seats
  emails at `"member"`/`"reviewer"` — anything else is a typed denial — and returns the standing
  workspace door link), `remove_member` (idempotent; the last-owner lockout denies typed),
  `rotate_join_link` ("reset link" — future redemption only), and `read_roster` (a `RosterSummary`:
  the seats + the owner-only door link, or the uniform `NotFound`). Same strict-mode threading as the
  standup wrappers; the authority ops themselves uniformly deny a self-host plane.
- **The session-read wrappers** (`session_read_cmd.rs`) — the read twin of the roster wrappers, the
  same leak-free, LIB-ONLY surface for the web-session MEMBER-SCOPED reads (no OSS HTTP route):
  `PlaneState::list_skills_session` (the workspace catalog — per skill: the `current` version id,
  generation, epoch-ms update time, consent digest, open-proposal count), `read_current_session` (the
  stored `WireCurrentRecord` bytes VERBATIM — a pointer with no record row is deliberately FOLDED into the
  uniform `NotFound`, since the catalog only lists current-rowed skills), `read_version_session` +
  `list_proposals_session` (PRE-SERIALIZED wire JSON via the SAME mappers the token-scoped `/v1`
  routes use — parity by construction, a composing route relays the bytes verbatim), and
  `read_object_session` (verified raw bytes). Same strict-mode threading; the authority ops uniformly
  deny a self-host plane and gate on a CONFIRMED workspace member (any role — the SAME membership
  gate the device lane runs).
- **The session-review wrappers** (`session_review_cmd.rs`) — the write twin of the session-read
  wrappers, the same leak-free, LIB-ONLY surface for the PRIVILEGED web-session review ops (a downstream
  composition's authenticated admin routes call them with a session-verified acting email; there is NO
  OSS HTTP route): `PlaneState::review_approve_session` / `review_reject_session` (approve / reject an
  open proposal — the reject `reason` is MANDATORY) return a typed `SessionReviewSummary`
  (`Approved` / `Rejected` / `Conflict` — the same stale-base refusal the CLI gets — / `Denied { reason }`
  / `NotFound`), and `read_proposal_session` returns the proposal detail (status + base + proposer +
  resolution + the review-required policy) or the uniform `NotFound`. `PlaneState::revert_session` (the
  web one-click "roll back to this version") returns a dedicated `SessionRevertSummary`
  (`Reverted` / `Conflict` / `Denied { reason }` / `NotFound`) — distinct from the review summary because a
  revert promotes (never approves/rejects) and its member-entitled refusals are the reviewer-role gate + the
  target refusals (a non-accepted / digest-less / no-current target), not the four-eyes/not-open family.
  Classification posture: malformed/unknown ids, an unproven caller, self-host, and an unknown candidate all
  fold to `NotFound` (disclosing nothing); the member-entitled protocol refusals (the reviewer-role gate,
  four-eyes, a resolved target, an empty reason, a reused request id, a non-accepted revert target) stay
  typed `Denied` so the composing surface can say why. Same strict-mode threading as the roster/read
  wrappers.
- **The two public-base seams**: `PlaneConfig.verify_base_url` (`--verify-base-url` /
  `TOPOS_PLANE_VERIFY_BASE_URL`, default the base URL) — the HUMAN-facing base the device-auth
  `verification_uri`(+`_complete`) and the passcode mail link are built on (`{base}/verify[/{code}]`) —
  and `PlaneConfig.link_base_url` (`--link-base-url` / `TOPOS_PLANE_LINK_BASE_URL`, default the base
  URL) — the PUBLIC base every minted `/i/<token>` share link rides (create-invite, mint-claim, the
  standup self-invite), for a hosted plane whose user-visible links live on its web origin (that origin
  serves or proxies `GET /i/{token}` back to the plane). Only the minted link STRING moves: the
  bootstrap payload keeps declaring the API `base_url` and the client re-roots onto it after the one
  bootstrap fetch. The standup `device/authorize` plane block and the bootstrap document both read the
  AUTHORITY's copy (`Authority::enrollment_disclosure` / the domain bootstrap's `link_base`) — one
  source, so a `PlaneState::new` composition can never serve blank or drifted bases.

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

**Planned (lands later):** a **device-credential-authenticated `PUT /policy` variant** (the
admin-token operator route is built — see above; a governance route over the same policy authenticated by
the acting device credential is still later work); the **audit outbox** read via durable cursors. NOT planned
here: **verification/create page HTML** — the routes above are the JSON surface, and a composing web layer
serves its own pages over them (hosted compositions already do); this lib deliberately ships none. **TLS
termination**: terminate at a reverse proxy (the recommended, documented path); the BIN also carries an
optional, default-off `acme` cargo feature — an EXPERIMENTAL built-in ACME (tls-alpn-01) TLS listener,
composition-root code in `main.rs`/`acme_serve.rs` only (never the lib surface), armed at runtime solely by
a non-empty `--acme-domain`, and rehearsed against a local test ACME server (`scripts/acme-rehearsal.sh`) —
a real box (public DNS, Let's Encrypt staging→prod, rate limits, renewal timing) remains unproven.

## bin

A thin `axum` `main` (composition root only — no trust logic): parses config (bind addr / database URL / git-root /
large-root / enrollment secret / base URL / mode / SMTP relay / the optional operator
admin token, which enables the policy route / the maintenance interval), resolves its two bin-local
marshals (the base URL default + the 5-or-none SMTP relay), then builds the serving state through the
**single leak-free constructor** `PlaneState::open(PlaneConfig { .. })` — which opens the `Authority`,
loads the enrollment secret, and builds the enrollment config INTERNALLY (the bin names no
`plane-store` type, dogfooding the same path a downstream plane uses) — spawns the maintenance scheduler
(`--gc-interval-secs` / **`TOPOS_PLANE_GC_INTERVAL_SECS`**, default **300**; `0` disables it for an operator
running the passes out-of-band), and serves `router(state)`. Under
`enroll-oidc` it reads `TOPOS_PLANE_OIDC_*` and loads the connector onto `PlaneState` (`with_oidc_config`) so
the `/v1/enroll/oidc/*` routes can drive it.

Two operator subcommands: **`topos-plane restore-bump-epoch --workspace <id> … | --all-workspaces
[--epoch-at-least <n>]`** — opens the same state serve does (never binding the listen socket), runs the
epoch bump, prints one line per bumped pointer plus a `bumped <N> pointer(s)` summary, and refuses to run
without an explicit selection. And **`topos-plane mint-claim
--workspace <id> [--display-name <name>] [--owner-email <email>] [--ttl 72h]`** — mints the one-time
workspace-standup claim and prints the `/i/` link as the ONLY stdout line (a shown-once warning goes to
stderr; the token never enters tracing). The **bare invocation still serves**, byte-identically — the
container ENTRYPOINT and every existing flag/env are unchanged.

## The litmus for what belongs in this lib

*Would you expose it even if there were no private product?* `router()`, the ops, the policy — **yes**
(self-hosters, integrators, and the tests all want them). An extension/callback hook that serves only a
closed product — **no**, and there is none here. A separate private program IMPORTS this library and
composes it (its routes call these public ops; its middleware sits in front); the library never calls back
into it. Anything that must run inside the publish transaction is, by definition, trust logic and lives
here, parameterized by config (like the `review-required` boolean) — never injected from outside.

Dependencies: `plane-store`, `topos-core`, `topos-types`, `axum`, tower middleware, `oauth2` /
`openidconnect` (feature-gated), `lettre`, `tracing`.
