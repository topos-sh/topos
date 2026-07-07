# Changelog

Delivery history, **newest first** — the increment-by-increment story of how this repository was built.
This file is *history*, not status: the current state of every area lives in the root `CLAUDE.md` status
table and in each crate's own `CLAUDE.md`. Version **0.1.0** is the first public release; the entries below
are the increment-by-increment history that leads up to it, each one a shipped increment.

## The web-session review lane — browser-side approve/reject on a hosted composition

A hosted composition's web tier could render a proposal (the read lane) but only the pasted-token CLI
could act on it; the plane now also serves PRIVILEGED lib-level review writes (the write twin of the
session-read leg): **approve** and **reject** an open proposal from a verified web session, plus a
proposal-detail read. The write TERMINATES in the SAME serializable pointer-move transaction the
device-signed CLI runs — one approve predicate, one `(epoch, seq)` CAS, one plane-signed pointer, one
four-eyes gate — branching on a new `WriteActor` (device vs session) ONLY at the authorization step: the
device arm is byte-identical, and the session arm is an in-transaction confirmed **owner-or-reviewer**
workspace-seat gate — the FIRST enforcement of the reviewer role (a deliberate lane asymmetry: the CLI
lane keeps its per-skill roster). The orchestration mirrors the session-roster leg's trust shape:
self-host uniformly denied, a canonical-UUID `request_id` idempotency under a fresh
`TOPOS_SESSION_REVIEW_V1` domain tag (so no stored identity from another domain can byte-match a review
request), a pool-level confirmed-member pre-gate before any proposal/digest/render work, and a MANDATORY
non-empty reject reason. The recording rule keeps the ledger honest: an unproven caller's refusal is
synthesized and NEVER persisted (a web-verified email proves nothing about membership in the target
workspace — it must not grow the receipts table or squat op-id slots), while a confirmed plain member's
role refusal is a durable typed `REVIEWER_ROLE_REQUIRED` denial.

- **Migration `0012`** renames `op_receipts.device_key_id → actor` (the slot always held the acting
  identity — a signing device key id, or now the session's verified email), adds a `method` discriminant
  (`device_signed` / `web_session`), a `request_sha256` (the session lane's full-request identity; NULL on
  the device lane, whose identity is the signed device-op frame), a reserved `step_up_attestation` column,
  and a `(workspace_id, op_id)` index; and adds `proposals.resolved_reason` + `resolved_at` (a device
  reject writes NULL — the CLI keeps its surface). The receipt is henceforth the audit trail for WHICH
  leg acted.
- **The replay probe went lane-blind** — one `(workspace, op_id)` lookup that fails closed in BOTH
  directions on cross-lane id reuse (a device op id and a session request id never replay each other),
  while each lane's own slot still replays byte-identically on a full `(method, actor, request_sha256)`
  match (the per-device slots are preserved).
- **Proposer disclosure is session-lane-only.** The proposal-detail read discloses the proposer +
  resolution facts + the `review_required` policy at read time; the thin `/v1` proposals listing stays
  proposer-free and byte-unchanged.
- **The wire, OpenAPI, schemas, and fixtures are all byte-unchanged** (the drift gates prove it) — the
  review leg is lib-only, with NO OSS HTTP route (a composition's authenticated admin routes are the
  callers). `topos-plane` wraps the three ops leak-free (`session_review_cmd.rs`), returning typed
  `Approved` / `Rejected` / `Conflict` / `Denied { reason }` / `NotFound` summaries. Consent stays
  end-to-end: a session approve carries no reviewer signature over the candidate, but the plane still
  signs the moved pointer and followers still re-verify bytes against the approved digest before applying.

## The share-link document is the browser face too — human hand-off first, served as text/plain

`GET /i/{token}`'s non-JSON representation — the agent-instruction document — now opens with the
human's one move (**"paste this link to your agent and ask it to follow"**; the claim variant says
"the link you were given", never echoing the one-time bearer token) before the agent's numbered
steps, and is served as `text/plain; charset=utf-8` instead of `text/markdown`: browsers display
plain text inline where `text/markdown` triggers a download, and the document IS the browser face —
no HTML page exists, here or on a hosted front. Why: real agent web-fetch tools ask for
`text/markdown, text/html, */*`, so any Accept fork that routes `text/html` to a human-facing page
sends the *agent* there too, and an agent on a fresh machine then never sees the install line. One
representation for every non-JSON reader ends that class of misrouting; the JSON machine contract
(explicit `application/json`, or no Accept at all) is unchanged.

## The web-session read lane — member-scoped session reads

A hosted composition's web tier could render a skill only through a pasted per-skill read token; the
plane now also serves PRIVILEGED lib-level member-scoped reads (the read twin of the session-roster
leg): the workspace skill index (per skill: the current pointer facts, the consent digest, the OPEN
non-stale proposal count), current / version metadata / object bytes / proposals listing — authorized
by ONE shared preamble (self-host uniformly denied, canonical principal fold, a CONFIRMED
workspace-member probe; every pre-gate miss is the single indistinguishable not-found), deliberately
broader than the device lane's per-skill roster (catalog visibility IS workspace membership, an
explicit decision stated loudly in the module docs). Under it, the three read authorizations were
split into a principal GATE (a `ReadLane` dispatch: skill-roster vs workspace-member) and ONE
lane-blind reachability statement each, so both lanes share identical reachability SQL — the
`open AND base == current` staleness predicate keeps its five tracked copies, the index count
delegates per skill to the same listing statement (count == list by construction), and the
re-authorize-on-miss guard re-gates on the caller's lane (reclaimed reads 404; corruption stays an
Integrity alarm — both directions pinned, plus a rejected-candidate-version 404 pin through both
lanes). `topos-plane` wraps the five ops leak-free (`session_read_cmd.rs`), returning the stored
signed record verbatim and PRE-SERIALIZED `/v1` wire JSON via the same mappers the token routes use —
parity by construction. Reads mint no receipts and no events; no OSS HTTP route (the wrappers are the
seam a hosted composition's authenticated admin routes call).

## Jittered backoff in the serializable retry loop

`run_serializable!` retried a serialization failure immediately, so two writers that collided once
retried in lockstep and collided again — on a loaded machine the whole 10-attempt budget could burn
in milliseconds and a raw `40001` leaked out as a 500 (observed as an intermittent full-suite
failure of the standup cap-boundary racing test; single-run and CI always green). Each re-run is
now preceded by a full-jitter exponential pause (uniform in `[0, min(10ms · 2^(n−1), 250ms)]`): the
happy path never sleeps, colliding writers desynchronize so one commits while the other waits, and
the worst single pause stays far below any client timeout. The retry cap and its
never-a-receipted-terminal contract are unchanged; the window arithmetic is unit-pinned.

## Canonical principal form — one mailbox, one identity

Principals (emails, and the device-rooted `dev.dk_…` ids) were stored and compared byte-exact, so
`alice@x` and `Alice@x` were two identities: a lowercased invite seat could never match a mixed-case
device-confirmed principal at the redeem roster gate ("invited but can't join"), a mixed-case owner
seat denied its own lowercased web session, and the per-identity workspace cap counted one human once
per casing. Now:

- **The kernel owns the fold.** `topos_core::sign::canonical_principal` (ASCII lowercase — the
  principal charset is ASCII-only, so the fold is total) is a cross-component identity rule like
  `device_key_id`: every email-valued signing-preimage input (the governance Invite email set, the
  roster-mutation targets) is folded **before signing**, and the plane folds at its parse boundary
  (`Principal::parse`) before storage, every compare, and its preimage re-derivation — signer and
  verifier bind the same bytes by construction. The signing frames themselves are byte-unchanged.
- **`topos invite` folds its emails once at op entry**, so the signed frame and the wire body agree
  (an older, case-preserving plane verifies the wire bytes — frame/body divergence would strand it).
- **Migration `0010`** folds the durable rows that predate the rule, deduping case-variant duplicate
  seats deterministically first (`roster` losslessly; `workspace_member` keeps the strongest seat:
  confirmed > invited, then owner > reviewer > member, then earliest seat — a confirmed owner can
  never lose its seat, so no workspace is orphaned), then pins the invariant with
  `lower(… COLLATE "C")` CHECKs on the two roster tables ("C" so a locale-sensitive `lower()` can
  never emit non-ASCII bytes the parse would later reject). Ephemeral flow tables and the audit
  ledger are deliberately not rewritten.
- **Named residuals, honestly:** an in-flight mixed-case enrollment crossing the deploy denies at
  the roster gate and is re-run fresh (`topos follow` again — a re-poll alone cannot heal it, the
  deterministic grant pinned the old casing); a cross-deploy same-op-id retry of a pre-deploy
  mixed-case governance/session op replays as the key-reuse denial; an OLD client binary (a
  kernel without the in-frame fold) inviting a mixed-case email against a new plane is DENIED at
  signature verification — retry with the lowercase form succeeds. (A client on the new kernel
  cannot hit this: the governance frame folds its email inputs itself.)

## Main-domain share links + the agent-readable bootstrap (paste a link to your agent)

The `/i/<token>` share link is now the complete cold-start artifact: a human pastes the bare link to
their agent, and the link itself teaches the agent every step.

- **`GET /i/{token}` content-negotiates.** An `Accept` asking for JSON (or no `Accept` at all) gets the
  unchanged versioned `BootstrapData` machine contract; everything else — curl's `*/*`, a browser, an
  agent's web fetch — gets a `text/markdown` **agent-instruction document** rendered from the same
  authority read: what the link is (workspace, verified-domain badge, offered skills), the checksummed
  installer one-liner if `topos` is missing, the `follow` command, the show-the-human verification step,
  the resume, and the per-digest offer consent (nothing auto-lands — the document says so). The CLAIM
  variant warns that the first redeemer becomes the workspace owner and NEVER echoes the token or link
  (the same custody rule as the JSON `token_id` placeholder). Both 200s are `no-store` + `Vary: accept`
  + `noindex`; errors stay the uniform JSON envelope on every `Accept`. Named skew, pre-GA: an OLD
  client binary's fresh `follow` sent `*/*` and now receives markdown — the new client sends
  `Accept: application/json` explicitly; already-enrolled devices are unaffected (pulls never touch `/i/`).
- **`--link-base-url` / `TOPOS_PLANE_LINK_BASE_URL`** (default: the base URL): the PUBLIC base every
  minted `/i/` link rides — create-invite, mint-claim, and the standup self-invite — so a hosted plane's
  user-visible links live on its web origin while the API stays on its own host. Only the link STRING
  moves: the bootstrap payload keeps declaring the API `base_url`.
- **The client re-roots.** `follow <link>` fetches the bootstrap from the link's host, then re-roots
  onto the bootstrap's declared `plane.base_url` for everything after — the device flow, the redeem,
  every pull, and the pinned `instance.json` (normalized; the same URL gate as the link base; an https
  link never downgrades to a plain-http plane). The TOFU pin and the one-plane-per-install refusal key
  on the RE-ROOTED base, so a second link from the same plane matches the pin whatever host it rides;
  the standup publish pins identically from its response's plane block. The enrolled plane is disclosed
  as `FollowData.plane_base_url` and on the pending TTY receipt. The claim-retry dedup now matches on
  the token alone (HMAC-derived, unique per plane), so a re-pasted claim link retries the redeem POST
  without ever refetching the possibly-consumed bootstrap. The client's fabricated
  `{base}/device` verification-URL fallback is gone — the server-built URL is used verbatim or the
  session restarts typed.
- **One authoritative source for the disclosed bases.** The standup `device/authorize` plane block and
  the bootstrap document read the Authority's enrollment config (`Authority::enrollment_disclosure`; the
  domain bootstrap carries `link_base`) — previously a `PlaneState::new` composition silently served the
  state-side default (blank base URL, self-host posture) in the standup block.
- **Receipts teach the motion.** `invite` and a genesis `publish` now say it plainly: teammates paste
  the link to their agent and ask it to follow — the link walks the agent through the rest.
- Proven end-to-end over loopback HTTP on ONE listener with split host strings
  (`tests/tests/follow_e2e.rs`): the minted link rides `localhost`, the markdown serves over the real
  socket, and the client pins + pulls on `127.0.0.1`.

## Workspace standup — the chain proof (the loopback full-chain e2e + the mint-claim smoke)

The two halves below are now proven END TO END, over real loopback HTTP, with the genuine client against
the genuine plane — the release-blocker e2e for the self-serve genesis:

- **`tests/tests/standup_e2e.rs`** (9 tests) walks every door: **door 1** — an un-enrolled direct
  `publish` goes PENDING (the sign-in envelope: `signin_required`, the server-built
  `verification_uri_complete` verbatim, the 16-char high-entropy code — 19 with the group dashes — and
  the same-command resume argv), a verified email approves via the authority op (the lib surface a
  composing web page calls), and re-invoking the SAME publish enrolls + lands the genesis at `(1,1)`
  in one invocation — the receipt disclosing "workspace X — owner Y", the workspace born `cloud` with
  the localpart-default name, the owner member confirmed, and the landed object pulled back byte-exact
  by a follower. The chain calls
  ZERO operator ops — by construction AND by the `admin_claim` table staying empty. **Door 2** —
  `create_workspace` (idempotent per request: a web retry replays ONE workspace + the identical
  self-invite), the owner's two-call follow through the web-approve leg, a genesis publish, a real
  `invite`, and a member whose redeem flips `invited → confirmed` and whose pull lands the bytes exactly.
  **Self-host** — the operator's one-time claim enrolls the first owner in ONE `follow <claim-link>`
  invocation (device-rooted owner, the workspace born at THE PLANE'S mode), then publish → invite → a
  second client's bearer redeem (no roster requirement) → byte-exact placement. **Adversarial
  witnesses** — a leaked self-invite is inert off-roster and the client surfaces the REQUEST_ACCESS
  ask-an-owner guidance (the production error envelope, asserted); approve-standup misses are the one
  uniform NotFound and a double-approve is idempotent (exactly ONE workspace); the 4th create for one
  identity is the typed cap denial; a standup session refuses every enroll identity leg (passcode
  start/complete, external confirm) yet stays live for its real approval; a consumed claim is Denied to
  a different device but replays `Redeemed` to the SAME device (lost-200 recovery); an expired claim is
  Denied + `/i/` NotFound; and cross-species tokens fail EXACTLY like unknown tokens in both directions,
  consuming nothing.
- **The harness grew the missing drivers** (all inside the existing feature-gated facades):
  `FollowHarness` gained `adopt`/`draft_digest`/`publish` (the real publish over the real transports,
  standup branch included — an explicit loopback base, never the compiled-in hosted default),
  `invite` (the real signed governance verb), `resume_expect_denied` (the production error envelope's
  code + next-action codes + redacted message), the cross-species `admin_claim_attempt` /
  `device_authorize_attempt` pokes, and the `user.json` accessors; `PublishResult` gained the `Pending`
  arm; the shared `tests/common` plane scaffold gained `start_plane_mode` (self-host planes) and keeps
  the per-test pool for row-level witnesses.
- **The mint-claim smoke** (`bins/topos-plane/src/tests/misc.rs`): the string
  `PlaneState::mint_admin_claim` returns — which the bin's `mint-claim` subcommand prints as its ONLY
  stdout line — is a single `<base_url>/i/<token>` line (43-char base64url token), the bearer token never
  enters tracing (a TRACE-capturing subscriber wraps the mint), and a cloud-mode mint without an owner
  email is the typed refusal.

## Workspace standup — the client half (the un-enrolled publish that creates a workspace; `follow <claim-link>`)

The `topos` CLI now walks through both standup doors the server opened:

- **An un-enrolled direct `publish` stands the workspace up instead of failing.** The FULL normal
  pre-flight runs first — skill resolution, scan, digest computation, the `--approve` consent gate — so
  consent binds BEFORE any network call. Only then does the client start a standup device authorization
  against the hosted plane (`TOPOS_PLANE_URL` override, else the compiled-in `https://api.topos.sh`;
  never consulted once enrolled), TOFU-pin the plane key from the response's plane block, write a `0600`
  `AuthorizingStandup` WAL, and return an `ok` PENDING receipt: `PublishData.pending`
  (`signin_required` + the SERVER-built `verification_uri_complete`, verbatim + persisted, + the code +
  an RFC-3339 expiry) with an `ENROLL_RESUME` next-action whose argv is THE SAME publish command —
  re-invoking it IS the resume, and the consent digest re-derives from `--approve` on every invocation
  (bytes that drift between the two calls hit the existing digest-mismatch refusal, never a silent ship).
  The re-invoked command polls ONCE: pending re-emits; denied/expired clears the WAL typed; granted
  signs the possession proof over the EMPTY offered set, redeems, records the `Redeemed` WAL BEFORE
  promotion (the existing crash fence — a later invocation re-promotes without re-redeeming), promotes
  the enrollment, and CONTINUES into the ordinary publish in the same invocation — the receipt disclosing
  `workspace <name> — owner <principal>` (hijack visibility: an owner you don't recognize means someone
  else approved the sign-in). `--propose` never stands up (a proposal against a workspace that doesn't
  exist yet is meaningless) and keeps its typed not-enrolled error; an enrolled device never reaches the
  branch. `PublishData` widened honestly: `version_id`/`current_generation` became optional (they are
  unknowable at pending), `pending` + `standup` blocks are new, and the pending envelope + a claim-follow
  envelope joined the golden fixtures.
- **`follow <claim-link>` enrolls in ONE invocation** (the self-host bearer door). The `/i/` bootstrap now
  branches on `enrollment_method`: `admin_claim` skips the device-auth session entirely — pin the key,
  write a pre-send `ClaimPending` WAL (`0600`; the claim token is the bearer secret, `Debug`-redacted),
  POST `/v1/admin-claim`, promote. An UNCERTAIN send retries the POST directly from the WAL on the next
  `follow` invocation (same link, or `--resume`) — never refetching the possibly-consumed `/i/` link,
  because the server's same-device replay of a consumed claim deterministically re-answers `Redeemed`.
  An enrollment method this build does not understand now fails CLOSED (typed), instead of silently
  running the device flow under an undisclosed posture.
- **The seated principal is persisted + disclosed.** The redeem's new `principal` lands in `user.json`
  (an email-shaped one also fills `email`; a device-rooted `dev.…` id never pretends to be one), the WAL
  context records which door rooted the enrollment (invite / standup / claim — `user.json.invite_rooted`
  is now honest for the new doors), and a DENIED grant redeem became a typed ask-an-owner error carrying
  the existing `REQUEST_ACCESS` action code ("ask a workspace owner to run `topos invite <your-email>`,
  then re-run `topos follow`").
- **The server-built `verification_uri_complete` wins everywhere.** The invite follow persists it in the
  `Authorizing` WAL and re-emits it verbatim on a pending resume; the client-side reconstruction survives
  only as the fallback for a plane that omits the field.

## Workspace standup — the server half (self-serve genesis + the hardened one-time claim)

Standing up a workspace's FIRST owner is now in-band on the server side, three doors onto ONE shared
genesis seat (`workspace` + confirmed `owner` written together, the owner only when this call created the
workspace — no genesis path can seat an owner into a live workspace):

- **The standup device flow** (hosted planes only). `POST /v1/device/authorize` accepts an optional
  `intent` (`enroll`/`standup`) with an optional `invite_token`: a standup start opens a session with NO
  workspace, returns a HIGH-entropy 16-char user code (19 with the group dashes; approving CREATES
  ownership, so the code must be unguessable — enroll codes keep their short shape), and carries the
  plane block (base URL, posture, the signing key to TOFU-pin) that an invited device would have read
  from `/i/`. The new lib-only
  `Authority::approve_standup` (+ the leak-free `PlaneState::approve_standup`) is the web leg a composing
  plane calls with a verified email: ONE transaction runs the per-identity creation cap, seats the
  workspace + owner (server-minted `w_…` id; freemail-aware domain claim; a server-side display-name
  default), and CASes the session pending→confirmed — the CAS is the idempotency (same-email re-click
  replays, anything else is the uniform miss). The granted poll now carries `{workspace_id, display_name}`
  and the redeem response the seated `principal`, so a standup client can bind its possession frame and
  disclose "workspace X — owner Y".
- **Direct create** (`Authority::create_workspace` / `PlaneState::create_workspace`, lib-only): the same
  genesis body for an already-verified email, idempotent per `request_id` (a replay returns the SAME
  workspace and the SAME deterministic self-invite link; the same request under a different owner is
  denied), capped per identity, and finished with the owner's self-invite so a web page can print one
  `topos follow <link>`.
- **The one-time claim, hardened.** `topos-plane mint-claim --workspace … [--display-name … --owner-email
  … --ttl 72h]` mints a bearer claim link (printed EXACTLY once to stdout; the token is never logged and
  every `Debug` redacts it). The claim row now carries the mint-time display name, owner email, and
  expiry; the redeem takes its facts from THAT ROW (the request's display name is disclosure-only), seats
  the workspace at THE PLANE'S deployment mode, and orders its checks so a consumed claim's SAME-device
  replay deterministically re-answers `Redeemed` (lost-200 recovery — expiry applies only to the first
  consumption) while every other dead-claim case stays one static denial. A cloud-mode mint REQUIRES an
  owner email. `GET /i/{token}` now also serves claim links (`enrollment_method: "admin_claim"`, no
  skills) — invites and claims live in disjoint tables, so a token can never cross doors.
- **First-writer-wins confirmations everywhere.** Every session confirmation is a pending→confirmed CAS:
  a same-principal replay is idempotent, a different principal is the uniform miss, and a confirmed
  principal is never overwritten; the passcode/OIDC legs refuse standup sessions outright (an
  `intent = 'enroll'` guard) — a standup session is only ever advanced by its approval.
- **Verification URLs got one base.** The plane now emits both `verification_uri` (`{base}/verify`) and
  `verification_uri_complete` (`{base}/verify/{code}`), built on the new optional
  `--verify-base-url` / `TOPOS_PLANE_VERIFY_BASE_URL` (default: the base URL) — the passcode mail body
  points at the same base, and `GET /v1/enroll/verify/{user_code}` now discloses the session's `intent`
  (a standup session shows an empty workspace name; the page renders create-copy from the intent).

Wire changes are additive-only. The client gained only the mechanical field widenings; the standup client
verbs and the hosted web pages are separate work.

## Hardening sweep — scheduled maintenance, honest errors, bounded durability

- **The maintenance the storage layer mandates is now scheduled.** `topos-plane` gained a `maintenance`
  module: `run_maintenance_pass` (recovery sweep → quarantine janitor → a GC pass per workspace, every
  fault traced with its source chain and tallied, never fatal) and `spawn_maintenance` (first tick
  immediate — the mandated startup recovery). The bin schedules it via `--gc-interval-secs` /
  `TOPOS_PLANE_GC_INTERVAL_SECS` (default 300; `0` disables); a downstream composition calls the same
  function. `Authority` exposes `run_gc` / `run_recovery` / `run_janitor` / `workspaces()` to drive it.
- **The GC clock got one unit.** The lifecycle TTL constants were seconds-valued while the server clock
  is epoch **milliseconds**, so every quarantine/lease/claim expiry fired ~1000× early (a fresh upload
  was already "expired" to the janitor). The constants are now `*_MS` in the clock's own unit, with a
  test pinning the fence's arithmetic against a real timestamp.
- **Unified diffs merge hunks correctly.** Two change clusters within `2 × CONTEXT` lines of each other
  now merge into one hunk instead of emitting overlapping (invalid) hunks; the diff and merge kernels
  additionally gained **seeded generative suites** (randomized inputs under a fixed seed) alongside the
  golden vectors.
- **The CLI says what the engine knows.** `pull` renders a per-skill outcome line (fast-forwarded /
  up-to-date / offered / held / diverged / alarm) with the concrete next command instead of a bare
  summary; pasted **short hashes** (≥ 8 hex chars, resolved against the skill's recorded history) work
  for the pull go-back, `revert --to`, and `diff` refs; `list` leads with the enrollment header
  (workspace, plane URL, hook state) and each row's follow state; `log` renders human-readable columns
  with raw JSON only as the unknown-shape fallback.
- **Error codes got honest.** A typo'd hash or bad flag combination reports `INVALID_ARGUMENT` (remapped
  only at the argv boundary) instead of masquerading as a corrupt sidecar; filesystem errors carry their
  `io::ErrorKind`, so permission-denied / read-only / disk-full classify `PERMANENT_FAILURE` instead of
  inviting an agent to retry forever. Every top-level client error appends its full display chain to
  `~/.topos/log.jsonl`; `TOPOS_DEBUG=1` additionally prints the chain to stderr while stdout stays the
  clean envelope. Plane-supplied ids are validated at every client boundary (`SkillId` and friends), and
  the session-start pull sweep degrades fast behind a circuit breaker instead of hammering a dead plane.
- **Durability got bounded and exact.** The client's write paths fsync exactly what each write created — a
  new per-version durability batch (commit + tree walk + ref + dir chains) replaces whole-tree walks
  everywhere except a fresh staging store; the server fence's `commit_durable` batch gained the subtree
  objects it previously missed. Pinned by a recording-filesystem test: complete, bounded, ordered.
- **One identity derivation.** `device_key_id`, the governance role byte, and the invite no-expiry
  sentinel — previously written twice (client and plane) — now live once in `topos-core::sign` next to the
  frames that bind them.
- **Reproducible gates.** A committed `.cargo/config.toml` makes `cargo xtask` real and defaults
  `SQLX_OFFLINE=true` (non-forced); `cargo xtask ci` runs the full non-DB gate sequence in CI's order;
  `check-arch` now also gates the leaf crates' leanness, the plane's OIDC default-off claim, and the
  Dockerfile/rust-toolchain pin pair; CI gained a compose smoke job. The per-directory docs were re-read
  against the code and rewritten where they had drifted.

## Packaging + release — the installable self-host

- **A tag-triggered release pipeline** builds and publishes the plane container image and the prebuilt
  client binaries (`cargo xtask dist` is the local arm of the same build), with the container supply
  chain hardened (pinned digests, non-root, minimal layers).
- **A checksummed echo-then-match installer** ships the client: the script prints the checksum it
  expects, downloads, verifies, and installs — rehearsable locally before any release exists.
- **The real restore procedure is built and documented:** restoring the database from a backup is
  followed by `topos-plane restore-bump-epoch` (re-signs every selected `current` pointer one epoch
  forward, same version) so every follower's next pull is an ordinary forward move instead of a
  reused-generation alarm — proven by its own e2e suite.
- **An EXPERIMENTAL built-in ACME TLS listener** lands behind a default-off `acme` cargo feature
  (tls-alpn-01, rehearsed against a local ACME test server); a TLS-terminating reverse proxy remains
  the documented default posture.

## The Hermes adapter — per-turn currency behind Hermes's own consent

- The second sibling harness adapter: placement + a per-turn currency trigger that rides Hermes's own
  consent surface, its config surgery hardened against review-caught edge shapes. Provisional behind
  the pilot readiness probe (the exact pilot build's injection behavior remains to be probed).
- The hero e2e gained the Hermes case row (the table-driven runner's designed extension point).

## The OpenClaw adapter — placement + first-topos-touch currency

- The first sibling harness adapter: placement + a first-topos-touch currency trigger, built behind
  the `HarnessAdapter` trait exactly as the port promised (a dir + a currency trigger, no refactor).
  Provisional behind the pilot readiness probe; a squatted plugin path degrades gracefully.
- Wired into the client's `adapter_for` dispatch; the distribute hero proven on the real adapter
  (one case row + one test).

## The distribute hero closes — proven on the real Claude Code adapter

The last-mile gaps between the built halves were wired: a **genesis publish stands up its own roster** (a
brand-new skill's first publish by a confirmed workspace member self-inserts the author's per-skill roster
row in the same transaction as the pointer; a non-member / unconfirmed / non-genesis shape stays DENIED);
the **`unfollow` verb** landed (flip `following=false` keeping the read credential for a resume;
byte-inert; idempotent), with `load_enrollment` decoupled from active following so an enrolled author with
zero follows still publishes; **follower currency arms at `follow`** (the promote step installs the
session-start hook best-effort + idempotent, mirroring `add`, disclosed on the result's `currency` field);
the app's harness wiring became an **`adapter_for(HarnessId)` dispatch** (one match arm per harness); and
the self-host **admin-token policy route** (`PUT /v1/workspaces/{ws}/policy/review-required`,
404-invisible unless `--admin-token` is set, 401 on a wrong token, 204 on the idempotent set) toggles the
review gate. Proven by a **real-adapter hero e2e** over loopback HTTP: an author genesis-publishes over
the wire, a pure follower's real two-call `follow` arms the REAL `settings.json` hook (asserted as the
full byte-exact command) and lands the first bundle byte-exact incl. the exec bit into a temp stand-in
`$CLAUDE_CONFIG_DIR`, an author update fast-forwards on the follower's next bare sweep, a real `revert
--to <good>` (a FORWARD move) restores the good bytes, and a drafting confirm-each follower surfaces every
move as a `diverged` row with its local draft never clobbered. The runner is table-driven so a sibling
adapter is one case row + one test. Honest ceiling, stated in the test's module doc: it proves
hook-installed + pull-materializes; that a live session's SessionStart stdout reaches model context before
skill resolution is a documented manual MUST-VERIFY.

## Postgres-only storage + self-host packaging

`plane-store` runs on **Postgres** (raw sqlx, no ORM; SERIALIZABLE write transactions with retry), with
compile-time-checked queries against committed offline metadata (`crates/plane-store/.sqlx`) so building
needs no database — only running the tests does. The self-host story became one **stateless plane
container plus a Postgres**: a pinned-builder Dockerfile (non-root runtime, CA roots only), a
`docker-compose.yml` running both, environment-tunable DB pool + timeouts, and
`scripts/compose-smoke.sh` — a one-command proof that the image builds, starts, migrates, and answers a
database-backed request.

## The plane becomes composable, leak-free

The OSS `topos-plane` lib gained a **leak-free construction surface**: a plain/owned `PlaneConfig` +
`PlaneState::open(cfg)` that builds the `Authority` + enrollment config **internally** (a composer never
names a `plane_store` type), the **bin dogfoods it** (one construction path, no drift), and a public
`PlaneState::set_review_required(ws, bool)` sets the workspace policy through the public API. A `no_run`
doc-test + a runtime parity test pin the surface; `PlaneState::new(Arc<Authority>)` stays the explicit
test/advanced path. This is what a separate, private downstream plane *composes* (imports + `.merge`s
`router(state)`, gates in front, sets policy via the API) — never forks, never the authority.

## The contribute loop closes — the client finally writes

The four device-signed write verbs were wired: **`publish`** (and **`--propose`**), **`review --approve |
--reject`**, and **`revert --to`**, plus the plane-sourced **`diff <skill> current..<hash>`**. A creds-free
**`ContributeSource`** transport (the 64-byte device-op signature in the `Topos-Device-Signature` header)
POSTs the four frozen write routes and maps the all-outcome **200 receipt** to a typed outcome (OK /
NEEDS_REVIEW / CONFLICT / APPROVAL_REQUIRED / DENIED). The crux — **commit parity** — holds: the client
computes the byte-identical `commit_id` + `bundle_digest` the plane re-derives (the same `topos-core`
digest + commit encoder; the candidate bytes ride inline base64 in one signed POST — no upload route), so
a valid signature *is* the binding of this device to this exact identity (a two-halves wire test fails on
any post-sign tweak). `publish` gates the outward ship behind **`--approve <skill>@<digest>`** (recompute +
refuse on mismatch — never a silent mode-flip), persists an **op-WAL** (`0600`) before the first send so an
uncertain retry replays the same `op_id` (no double-advance), advances local state read-your-writes on OK,
and (on a genesis publish) folds in a best-effort owner-gated `/i/` link; a direct publish under
`review_required` fails typed (`APPROVAL_REQUIRED`). A minimal **proposals-listing read route** (rostered,
404-not-403, the shared open-and-base-is-current predicate so a staled proposal vanishes — count + handles
only) makes `pull --json`'s `proposals_awaiting` and `list <skill>` real. The whole loop is proven
end-to-end over loopback HTTP (publish-direct → follower auto-applies byte-exact; `--propose` → a
four-eyes `review --approve` → follower applies with no prompt; `revert` → follower rolls forward; the
plane `diff` renders a proposal). The client stays edge-clean.

## Enrollment turns the fixtures into a real follow

Identity issuance built end to end, so a real `topos follow <link>` enrolls, mints credentials, registers
a device, pins the plane key, and lands the first skill. The **kernel** gained two domain-separated
verify-only frames (the device-enrollment possession proof + the governance-op signature). The **plane**
mints the opaque credential family — one `/i/` invite, the RFC-8628 device-flow grant, per-(device,skill)
read tokens — **deterministically HMAC-derived over a `0600` enrollment secret and stored only as its
sha256** (a lost-ack retry re-derives the identical credential — naturally idempotent redeem, instant
revoke). The central **`redeem_enrollment`** runs ONE serializable transaction (possession proof via the
kernel's `verify_enroll` → the deployment-mode roster gate [cloud requires a confirmed, already-rostered
identity; self-host grants membership from the bearer] → device register with anti-squat → per-skill read
scope + minted read tokens — **never a user token**). The device key id is server-derived from the public
key. The device-flow / emailed-passcode floor / self-host invite-chain are concrete `topos-plane` modules
behind thin routes (OpenAPI drift-gated); a single generic **OIDC connector** is compiled behind a
default-off cargo feature (the id token is consumed server-side, never returned to the agent).
**Governance** mutations (invite / roster set+remove / device-revoke) are device-signed against the kernel
governance frame, role-gated (owner/reviewer/member), op_id-idempotent, and instant. The **client** mints
an Ed25519 device key (a separate `0600` seed file, never in JSON), and **`follow`** is a two-call
agent-driven flow (TOFU-pin the plane key over the unauthenticated `/i/` channel; a `0600` write-ahead
log; the first version always an offer behind one `--approve`, never auto-landed); **`invite`** mints an
`/i/` link by signing the governance op. Proven end-to-end over loopback HTTP (a real `follow` enrolls +
redeems + lands the first skill byte-exact; a leaked invite is inert to an off-roster identity).

## The HTTP plane + the client's real transport

The authority went on the network — the seam the two built halves meet at. The OSS `topos-plane` is a
`router(state)` **library** (the single surface a downstream plane composes — no extension/fork hook) plus
a thin `axum` bin; every handler is thin (parse the wire DTO → call the authority → serialize; no trust
decision, no raw object read, no client-asserted principal in a handler). The frozen routes: the
conditional-GET signed-`current` read (`ETag = "<epoch>.<seq>"`, a commit-sensitive 304), the skill-scoped
object read and a version-metadata read (both 404-not-403, never by bare hash, behind an opaque per-skill
read credential resolved inside the authority), and the device-signed writes
(`publish`/`proposals`/`reverts`/`reviews`). **Every terminal protocol outcome is an HTTP 200** carrying
the canonical all-outcome receipt (a failure adds the flat wire error + `next_actions`; non-2xx is
reserved for transport/auth/integrity faults); an `op_id` retry replays byte-identically; a minimal
in-process rate limiter freezes the 429 shape; a generated **OpenAPI** landed under `contracts/openapi/`
(drift-gated). The client's real transport was wired too: a blocking `ureq` `PlaneSource` conditional-GETs
the signed pointer and reassembles a version (metadata + per-blob content-addressed GETs, re-verifying
each sha256) to feed the existing pull engine. The distribute loop was proven end-to-end over loopback
HTTP (a first pull fast-forwards byte-exact incl. the exec bit, a second is a 304 no-op, a tampered
signature is refused with last-known-good retained). The client gained no `plane-store`/`sqlx`/`tokio`
edge (`check-arch` holds the line).

## The contribute authority — propose → review (the server half)

`publish --propose` · `review --approve | --reject` built on the shared pointer-move write: `propose`
ingests + migrates a candidate like publish, then opens a `proposals` row and roots its bytes through a
**gated `proposal_object`** root (NOT `commit_object`) without moving `current` or signing
(`NEEDS_REVIEW`); a proposal's bytes are retained AND readable only while open with its base still
`current` — **one derived predicate shared verbatim by the read-authorization join and both GC-claim
queries** — so keep-set == read surface holds across the eventless "stale" transition, with a read-time
re-authorize guard so a reclaimed object reads 404, never an integrity fault. `review --approve`
uploads/leases nothing, runs the shared `(epoch,seq)` CAS (a stale base ⇒ CONFLICT), enforces four-eyes
under `review_required`, then reuses the SAME promote — whose edge-write is the `proposal_object →
commit_object` handoff to the permanent trunk root — and flips the proposal `accepted` (sideways, signed);
`review --reject`/withdraw is a standalone status-flip, after which GC reclaims the now-unrooted unique
bytes. The legacy standalone upload path was retired — every write goes through the shared ingest, so a
`commit_object` edge means accepted-trunk by construction. Exercised in-process by the stale-approve + ABA
interleavings.

## The author-side three-way (diff3) merge

Resolves a DIVERGED draft (the prior layer only detected + snapshotted + refused): a pure kernel
**policy** (file-set reconciliation over `(path, mode, content-id)` → a plan + an outcome, plus the
presence-based **publish guard**), a `topos-gitstore` **execution** (`diffy`, pinned exact + golden — its
conflict bytes are a consent artifact; non-UTF-8 is never line-merged; client-side size caps before
allocation), and a client **resolution** reachable only through a `DivergedWitness` capability token (the
structural author-only gate — followers never merge). A **clean** merge lands a draft-on-current (forward
1-parent commit on `current`; `applied = observed`); a **conflict** materializes the complete marker tree
(both sides kept via `.topos-mine` sidecars where there are no merge bytes) plus a durable
**`conflict.json`** that is both the publish-block fact and a pre-swap recovery journal (a crash
mid-materialize re-renders the recorded result, never re-merging on-disk markers; a clean re-run always
converges — proven by a fault-injection sweep). The disclosed escape (`pull <skill> --onto-current`)
commits the author's bytes on `current` with a drop-diff; unrelated histories fall back to a 2-way manual
choice, never a silent merge.

## The client pull/apply sync engine

The `checkForUpdates → plan → apply` machine over the pure four-state currency transition (in
`topos-core`), reading a signed `current` pointer through a source seam, authenticating its signature +
workspace/skill scope, holding the **anti-rollback floor** (`observed` rises only on a verified
strictly-higher record; a record at or below the floor is never auto-applied, and one naming a different
commit than recorded raises a loud ALARM), snapshotting a local draft before any decision (never clobbered
— a divergence is detected + surfaced), fetching + re-verifying the bytes (digest == tree == `commit_id`)
and recording them durably before a **crash-safe, namespace-atomic byte-writing materialization** (a
sibling staging dir → fsync → atomic directory swap → fsync parent → `map → lock → sync` commit, so a
fault at any boundary leaves the placement holding old-or-new complete bytes and `applied` advances only
after the swap; a crash-after-swap heals forward rather than showing a false divergence). `pull <skill>`
accepts a pending update and `pull <skill>@<hash>` goes back to a version locally (a `held` pin that never
lowers the floor). Consent stays the kernel's one `decide()` policy. (The plane response + follow-state
were fixture-fed in-process at this stage — no HTTP yet.)

## The pointer-move write (set-current)

The write that *moves* `current` (publish · genesis · revert): **one serializable pure-DB transaction**
(no filesystem op inside it) does receipt-replay → in-transaction authoritative device authz (a device-op
signature against a non-revoked registered key bound to a rostered principal — a revoke committed first
blocks the move) → a **compare-and-set on the whole `(epoch,seq)` pair** (CONFLICT carries the live
generation; a restore that bumps `epoch` while reusing `seq` is caught) → object-availability + a
lease-completion gate → same-skill lineage + the first-parent assert → provenance + reachability written
**before** the pointer advance and the lease release (so a concurrent GC never has a window to reclaim the
freshly-current bytes) → an **in-process Ed25519 signer** (the only private-key holder; load-or-generate
`0600`) → a durable **all-outcome receipt** keyed `(workspace, device_key_id, op_id)` (a lost-ack retry
replays it byte-for-byte). `revert` is a **forward** commit (`seq` advances; the pointer never moves
backward); the **review-required typed-fail gate** fails a direct publish closed (`APPROVAL_REQUIRED`,
ingesting nothing). Exercised in-process by deterministic interleaving tests.

## The size-routed large-object store

At migrate, a file blob at or above a configurable ~1 MiB threshold is physically offloaded to a
**per-workspace content-addressed side store** (the local filesystem, `object_presence.location =
large-local`) keyed by the **same `blob_id`**; smaller blobs stay in git; a ~100 MiB per-blob reject cap
fails typed at ingest. Identity stays **placement-independent** (the same `version_id`/`bundle_digest`
whichever store holds the bytes — no pointer files); reads (single-object and whole-bundle render) and the
GC unlink **dispatch on `location`**, still through the skill-scoped access rule (404-not-403, never by
bare hash); there is no cross-workspace dedup. The database leads and the filesystem trails throughout.

## The object-lifecycle / garbage-collection fence

The DB-authoritative lifecycle over the store: a GC-excluded upload **quarantine**; the fenced
**`object_presence`** state machine (`present`/`deleting`/`absent`/`unavailable`) whose guarded
compare-and-swaps make a `deleting` object non-resurrectable; **promotion leases** that root a commit's
full object set before any byte migrates; **migrate** (lease-before-migrate, server-side dedup, durable
install — size-routing to git or the large-object store) recording a real version; **transactional
mark-then-claim GC** (claim → unlink → finalize, the unlink outside any transaction, the keep-set exactly
the read-authorization surface) with a recovery sweep + a quarantine janitor; and the **tombstones
denylist** (no `purge` verb yet).

## The plane's storage + read authority (`plane-store`)

Built behind its privacy boundary: per-workspace Postgres + git-object storage; the skill-scoped
object-read access rule (rostered AND reachable, one indistinguishable not-found, never served by bare
hash); full-tree upload with server rehash that records provenance + reachability only after an
authoritative roster check; and the cross-skill lineage predicate — all directly tested against a real
database + git store. (At this point it moved no pointer and signed nothing yet.)

## The Claude Code harness adapter

Discovery, adopt-in-place (track a skill where it sits, writing nothing into it), the idempotent
content-blind session-start currency hook in `settings.json`, and a clean (skill-byte-preserving)
uninstall.

## The local, accountless core

The embedded-git sidecar (`topos-gitstore` over `gix`, with verify-on-read), the crash-safe document
protocol, the bundle scanner, and the local verbs (`add`/`list`/`diff`/`log`/`pull`/`uninstall`).

## Contracts + the pure trust kernel

The boundary contracts frozen and schema-generated (the `--json` envelope, the
outcome/receipt/error/action-code shapes, the closed signature-alg + signed pointer, the per-verb `data`
payloads, and the load-bearing persisted client documents — sync/lock/map/op), with golden `--json`
fixtures validated positive **and** negative against the schemas. The pure trust kernel (`topos-core`)
implements the **byte-exact digest**, the **consent truth-table**, and the frozen **signing/commit
byte-encodings** — the `commit_id` construction, the Ed25519 device-op signature frame, and the JCS
`current`-pointer preimage, all with verify and all behind known-answer vectors.
