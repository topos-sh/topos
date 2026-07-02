# Changelog

Delivery history, **newest first** — the increment-by-increment story of how this repository was built.
This file is *history*, not status: the current state of every area lives in the root `CLAUDE.md` status
table and in each crate's own `CLAUDE.md`. There are no version numbers yet (nothing is released); each
entry is one shipped increment.

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
