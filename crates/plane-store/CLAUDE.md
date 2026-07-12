# `plane-store` — the server authority boundary

**A crate so that raw access is private.** It owns the plane's per-workspace SQL — **raw `sqlx`, no
ORM** — and per-workspace git-object storage, and it is the single place the **membership access rule**
(one predicate: a CONFIRMED `workspace_member` seat, on every lane, for reads AND writes)
is enforced. The pool, every transaction, every raw SQL statement, and every raw git-object read are
`pub(crate)`-private; the **only** public surface is authorized authority operations on `Authority`.

## The privacy boundary IS the security mechanism

No code outside this crate can run an unbound query or read a bare object — that is unbypassable by
construction. (This is misuse-prevention by encapsulation; it is not isolation against malicious
same-process code.) The error type holds this line too: internal faults carry a **boxed** source, so no
`sqlx` or git-store type appears in any public signature.

## File map (the vault/directory grouping over the orchestration/db twin convention — full rule in `src/lib.rs`)

The domains split into two groups — **`custody/`** (byte custody: bytes/versions/pointers/GC) and
**`directory/`** (access/identity/policy) — each mirrored under `db/` for its raw-SQL half. Each write
domain X is a twin: `src/{custody,directory}/X.rs` (orchestration OUTSIDE the transaction — filesystem
work, credential derivation, candidate assembly; no SQL) + `src/db/{custody,directory}/X.rs` (the one
`SERIALIZABLE` (`run_serializable!`) write transaction + its pool reads; no `sqlx` type crosses out of
`mod db`). Custody consults access ONLY through the **access-witness** trait — never a directory module
path, never a directory table (a one-way seam `cargo xtask check-arch` enforces).

**Custody** (`custody/` + `db/custody/`) — byte custody:
- `set_current.rs` / `db/custody/set_current.rs` — the pointer-move: `run`'s ordered arms (replay → authz →
  CAS → availability → lineage → the op tails) as its single story, plus the reject transaction; the
  proposals' orchestration lives here too (propose/approve are arms of the one write).
- `upload.rs` — the candidate DTOs; `lifecycle.rs` / `gc.rs` (+ `db/custody/lifecycle.rs`) — the
  object-lifecycle fence (one fence, one file — `gc`'s SQL lives in `db/custody/lifecycle`).
- `read.rs` / `db/custody/read.rs` — the read surface (the lane-blind reachability half of the gate/reach
  split); `lineage.rs` — the lineage predicate; `restore.rs` / `db/custody/restore.rs` — the epoch bump.
- `db/custody/receipts.rs` — SQL-half-only (no orchestration twin): the durable receipt read/insert/replay
  machinery + the terminal-outcome writers both `db/custody/set_current.rs` paths call.
- `db/custody/proposals.rs` — the contribute tables' SQL.
- `db/custody/witness.rs` — **the `AccessWitness` TRAIT** custody declares and consumes: `device` (resolve
  a presented workspace credential — by its sha256 — to its registry row, INSIDE the live transaction;
  the lookup IS the authentication), `confirmed_member` + `member_role` (the ONE membership gate + the
  role band the protection gate consumes), `session_write_gate` (the three-way session outcome — the
  role matrix itself lives directory-side), `skill_gate` (the catalog's lifecycle status + the resolved
  per-bundle protection cascade), the few directory WRITES the pointer-move makes atomically
  (`register_publish` — catalog registration + `everyone`/`--to` placement + the author self-follow;
  `place_skill`; `set_display_name`; `notify_verdict`), and the pool-level `read_gate` (the membership
  gate, shared by both read lanes).
  Its in-transaction methods take the live write transaction, so a directory row-write is instantly
  effective against byte ops (revoke-blocks-promotion) — no duplicated enforcement, no cache to invalidate.

**Directory** (`directory/` + `db/directory/`) — access/identity/policy:
- `catalog.rs` / `db/directory/catalog.rs` — the skill LIFECYCLE session ops (archive / unarchive /
  delete / purge): owner-gated in the guarded SQL functions, self-host denied like the roster/review/read
  session legs (the channel join/leave session twins deliberately do NOT deny — they are the same guarded
  functions the device lane calls, and self-host runs the whole loop);
  the db twin runs the row policy + the CUSTODY halves (delete un-roots every commit's edges + drops
  `current`; purge un-roots one version's) in ONE transaction, so the shipped GC keep-set reclaims
  exactly what dropped out.
- `channels.rs` / `db/directory/channels.rs` — the device-lane channel-era ops: curation
  (place/unplace, create-on-first-use), self-serve channel join/leave, person-scoped
  follow/unfollow, the device exclusion, the `protect` setter (+ session twins for join/leave).
  Every policy decision is a guarded `topos_*` SQL function call (0015) — the ONE implementation
  Rust calls today and the web tier calls at the door cutover; each function re-runs the membership gate
  itself, so the database answer is authoritative for every caller, not just this one. The db twin
  resolves SKILL names to ids in Rust (`resolve_skill_name`) and leaves CHANNEL names to the functions,
  then maps outcome codes — an asymmetry the web tier will want folded into the functions. Naturally idempotent row ops (no receipts);
  the channel audit is TRIGGER-emitted, so no write path can skip it.
- `delivery.rs` / `db/directory/delivery.rs` — the delivery read ("what should THIS device have":
  the ONE entitlement SRF + via attribution + the person's detached set + the unacked notices feed
  + the open-proposal count) and the fleet's applied-state report (snapshot upsert; detach records
  immutable; `last_report_at` the staleness clock).
- `enroll.rs` / `db/directory/enroll.rs` — enrollment issuance BY ADDRESS (device-auth toward a
  workspace NAME, passcodes, grants, the central redeem — which gates on the ROSTER and mints the ONE
  **workspace credential** per device — and the LOGIN door, which re-mints that credential in every
  confirmed seat) and the device READ lane's resolver (`resolve_read_scope`). The shared credential
  derivations (HMAC mint, sha256 storage form, the server-derived device key id) and the cross-domain
  in-txn helpers (`read_device`, `blob32`) live here.
- `governance.rs` / `db/directory/governance.rs` — the role-gated governance surface (roster/revoke,
  authenticated by in-transaction device-credential lookup and bound to a canonical request
  identity under `TOPOS_DEVICE_GOVERNANCE_V1`; the last-owner-lockout guard; the `workspace_events` audit +
  idempotency) + the workspace ADDRESS-name rules (`validate_workspace_name` — charset/reserved/
  archived-pattern — and the slugify/dedupe/fallback resolution) + the workspace-standup genesis ops
  (the one-time `admin_claim` mint/redeem, `create_workspace`, `approve_standup`, and the shared
  `seat_workspace_and_owner` genesis seat — every genesis names an address and returns it as the
  share line; no invite link exists to mint).
- `session_read.rs` / `db/directory/session_read.rs` — the web-session READ lane (privileged lib-level, no
  OSS HTTP route): pool reads only, no `run_serializable!`, no op_id/`workspace_events`/receipts (the ONE
  new query is the skill index; everything else re-uses `read.rs`'s machinery over the member gate).
- `session_roster.rs` / `db/directory/session_roster.rs` — the web-session roster leg (invite-at-member-
  or-reviewer / remove / roster read — an invitation is a ROSTER WRITE, and the ops disclose the
  workspace ADDRESS, never a tokened link), authorized by an in-transaction
  confirmed-OWNER acting gate (the composing caller's session verification is the authentication),
  `request_id`-idempotent through the `workspace_events` slot, uniformly denied on self-host.
- `session_review.rs` (+ `actor.rs`) — the web-session review leg: approve/reject/revert from a verified
  session, orchestration ONLY with **no db twin** — the write terminates in the SAME
  `db/custody/set_current.rs` transaction (branching on the `WriteActor` lane at its authorization step
  alone); its read sibling (`read_proposal_detail_session`) lives in `session_read.rs` over the same gate.
- `db/directory/witness.rs` — **the `AccessWitness` IMPL for `Db`** over the directory tables
  (`device_registry`, `roster`, `workspace_member`, `workspace_policy`) — the shared
  `device_by_credential` resolver (a presented workspace credential's sha256 → its registry row, O(1)
  on the partial-unique index, workspace-bound) plus the
  directory-owned pool probes (`confirmed_member`, `member_role`, `resolve_read_credential` /
  `resolve_device_credential`, the policy read/write).

**Shared crate-root leaves** (imported by both groups; neither group imports the other):
- `authority.rs` — the sealed facade: `Authority` + `PoolConfig`, exactly the production API (the
  feature-gated test-fixtures shims live in `fixtures.rs`, split out so this file reads as what ships).
- `actor.rs` — the write-actor lane vocabulary (`WriteActor` Device|Session + the ONE `ReceiptActor`
  projection, incl. the session-denial constants), so every terminal writer derives its
  `(actor, method, request_sha256)` triple in one place and the lane vocabulary cannot drift per writer.
- `error.rs` — the boxed-source error (no `sqlx`/git-store type in a public signature); `id.rs` — the
  validated id newtypes; `secret.rs` — the `0600` seed custody (load-or-generate, atomic publish; now used
  only by the enrollment HMAC secret).
- `db/mod.rs` — the pool, `run_serializable!`, the `blob32` helper, and the retry
  classification; `db/seed.rs` — test-only staging.
- `fixtures.rs` — the `feature = "test-fixtures"` `impl Authority` shims (never in a production build);
  `src/tests/` — the in-crate suite, one named module per concern.

## Implemented (each behind a test in `src/tests/` + the module unit tests)

- **Per-workspace storage + hard tenant binding.** One `topos-gitstore` repo per workspace under a
  confined root, plus a Postgres database whose every row carries `workspace_id` and whose every query
  binds it. `WorkspaceId` is a validated, path-safe id, so the per-workspace store directory can never
  escape the root. Isolation is the database binding, never the directory.
- **The schema** (`migrations/0001`–`0004`, Postgres: content ids 32-byte **BYTEA** width-checked
  `octet_length=32`, integer/time/seq/epoch/size columns **BIGINT**, booleans a `BIGINT` 0/1 with
  `CHECK (x IN (0,1))`, no `STRICT`/`WITHOUT ROWID`):
  `skill_commit` (provenance — **PK `(workspace_id, commit_id)`** makes a content-derived commit belong to
  exactly one skill), `commit_object` (accepted-trunk reachability + access, with the inverse index), `roster`
  (membership = a row exists), `current` (the one movable pointer); the object-lifecycle + pointer-move tables
  (`0002`/`0003`); and the **contribute tables** (`0004`): `proposals` (`status ∈ {open,accepted,rejected}`;
  PK `(workspace_id, id)` where `id` IS the opening op_id; a **partial-unique** "one open per
  (skill,commit,base)"; `base_commit_id` = the approve's authoritative first parent), `proposal_object` (the
  **gated** retention/read root for a pending proposal), and `approvals` (the audit log). Later migrations:
  `0005` (`read_token` — DROPPED by `0014`), `0006` (the enrollment/governance schema — see the issuance bullet below), `0007`
  (the `object_presence (workspace_id, git_oid)` index the version-metadata read resolves tree leaves
  through), and `0008` (the workspace-standup schema: session `intent` + a nullable session `workspace_id`
  CHECK-bound to unapproved standups, the claim row's mint-time facts, and the `genesis_requests`
  create-workspace idempotency ledger). The trust-recalibration migration `0013` follows the later feature
  migrations (`0009`–`0012`): `current.signed_record → current.record` with the signature block stripped,
  `op_receipts` likewise + its `key_id` column dropped, and the receipt/audit `method` discriminant
  `device_signed → device` (nothing signs; the receipt's actor is the presented device credential's key id).
  **`0015` is the CHANNELS schema**: `catalog` (the first real name→skill mapping — `skill_id` stays the
  immutable custody key, `name` the mutable user-facing key, `display_name` absorbed off `current`,
  `status` active|archived|deleted, `protection` the per-bundle pin over the `workspace_policy` default),
  `channels`/`channel_skills`/`channel_members` (the structural `everyone`: builtin, roster-derived
  membership, trigger-guarded undeletable/unrenameable/unjoinable), the trigger-emitted `channel_events`
  audit, person-scoped `skill_follows`/`skill_unfollows`, per-device `device_exclusions`, person-scoped
  `notices` with read-state, the fleet `device_skill_state` (+ `device_registry.last_report_at`), version
  purge tombstone columns on `skill_commit`, the `closed` proposal status, the guarded `topos_*` POLICY
  FUNCTIONS (curation, membership, subscriptions, protect, lifecycle, the lapse-detach/re-attach
  reconciles) + the `topos_person_entitled`/`topos_entitled_skills` entitlement SRFs — and the LIFT: the
  interim per-skill `roster` rows moved into person-scoped direct follows, then **`DROP TABLE roster`**.
  **`0016` is the VERB-SURFACE schema**: the workspace ADDRESS (`workspace.name` — unique,
  charset-checked, backfilled from display names), the `invite_policy` + `staleness_window_ms` policy
  knobs with their guarded `topos_*` setters/readers, `topos_invite` (the roster-row invitation with
  channel pre-placement) + `topos_notices_ack`, the token-less enrollment session shape
  (`device_auth_sessions.requested_workspace`, the `login` intent, a nullable grant workspace + a grant
  `intent` discriminant) — and the tokened invite door's interment: `DROP TABLE invites, invite_skill`,
  the session/grant `invite_sha256` columns and the standing-door `workspace.link_epoch` dropped.
- **`Authority::read_object`** — the skill-scoped read. Gate + reach authorize on **confirmed member ∧ reachable** —
  reachable through EITHER the accepted trunk (`commit_object`) OR an **open, non-stale proposal**
  (`proposal_object`), the latter gated on the **same** `open ∧ base == current` predicate the GC keep-set
  uses, so **keep-set == read surface** — and yields a witness commit; the bytes are then read + re-verified.
  Every not-entitled/not-found case returns one indistinguishable `NotFound`; a store failure on an
  already-authorized object is a separate `Integrity` fault (corruption), never a not-found. A post-authz
  fetch miss **re-authorizes** (the read-time TOCTOU guard): a proposal that staled — and whose unique bytes
  a GC reclaimed — between the authorize and the fetch reads **404, never Integrity**. **No object is served
  by bare hash.**
- **The network read surface (what the HTTP plane composes over).** `resolve_read_scope(ws, skill,
  credential)` authenticates the presented **workspace credential** (stored only as its **sha256**, ON the
  device's registry row, workspace-bound) and gates on the CONFIRMED-membership join — the principal comes
  from the trusted row, **never** a caller-asserted id; every miss (unknown/rotated/revoked credential,
  non-member, malformed id) is the same indistinguishable
  `NotFound`. Over it: `read_current` (the `current` record — the unsigned `WireCurrentRecord` — + its generation/version, for the
  conditional-GET/ETag/304 read), `serve_object` (the bundle read — a scope/path mismatch or a malformed id
  is `NotFound`, then the same `read_object`), `read_version_metadata` (a version's
  parents/author/message/digest/file-list — **no blob bytes** — for the client's reassembly walk), and
  `list_open_proposals` (the OPEN proposals on a workspace skill as `{version_id, base, created_at}` —
  **count + handles only, no bytes, no roles**: the reviewer's discovery surface for `proposals_awaiting` /
  `list <skill>`; reuses the SAME `open ∧ base==current` staleness clause verbatim — the **fifth** tracked
  copy — so a staled proposal vanishes [keep==read==list], and folds a non-member into an empty list via the
  membership gate, never a 403/oracle). The version-metadata read is R1-scoped by `authorize_version_read`, which
  **mirrors `read_object`'s predicate** (member ∧ accepted-trunk-or-open-non-stale-proposal), so an
  unaccepted/rejected proposal version is the indistinguishable `NotFound`; `list_open_proposals` applies the
  same scope/path assert first (a cross-skill/workspace token ⇒ `NotFound`). Commit metadata comes from gitstore's exact one-commit `read_commit_meta`
  (fails closed on an unmapped parent, never the lossy `log`). `read_current_record` is `pub(crate)` (the
  public authenticated read is `read_current`). `SetCurrentReceipt` is enriched (command/skill/version/digest/
  expected/created_at — all already persisted) so the network layer builds the canonical all-outcome receipt
  and replays it byte-for-byte. A feature-gated **`test-fixtures`** surface (roster / device / read-token / a
  published genesis + child + a stored-record-tamper helper) lets an out-of-crate test drive a loopback plane; it
  is gated **out of the production build** (a check-arch guard proves production never enables it).
- **Candidate ingest (server rehash — the confused-deputy guard).** Every write that introduces bytes
  (`publish`/`propose`/`revert`) ingests the full candidate tree and **recomputes every id from the bytes**
  (no client id trusted; no reference-by-id), applies the canonical rules, and migrates the
  not-already-present objects (server-side dedup, invisible). The standalone `upload_candidate` op was
  **retired** — its rehash/canonical/dedup machinery IS this shared ingest path, and `commit_object` is now
  written ONLY by the accepted-trunk path (so "a `commit_object` edge" means "accepted-trunk-reachable", by
  construction).
- **`Authority::check_lineage`** — the cross-skill lineage predicate (a tiny database gather + a pure
  decision function), read-only; the pointer-move/contribute writes enforce the same rule transactionally.
- **Transaction discipline.** Every write runs through the private `run_serializable!` macro
  (`src/db/mod.rs`): a `SERIALIZABLE`-isolation transaction with a bounded retry on a serialization failure
  (SQLSTATE `40001`) or deadlock (`40P01`), whether raised by a statement or by `COMMIT` — each re-run
  preceded by a full-jitter exponential pause (10ms doubling, 250ms cap; immediate lockstep re-runs let two
  colliding writers burn the whole budget under load). It is a **macro**,
  not a generic runner fn, so each caller's future stays `Send` (an `AsyncFnMut` runner can't bound the
  closure future `Send` on stable). Because Postgres does not serialize writers, every read-then-write
  invariant is re-proven by SSI + retry — the whole-`(epoch, seq)` CAS, the last-owner write-skew guard, the
  object-presence fence, and op-id idempotency. Two targeted touches harden that under MVCC: the
  proposal-resolution reads take `SELECT … FOR UPDATE`, and `insert_lease` writes the leased `present`
  `object_presence` rows so a concurrent GC claim conflicts (closing a claim-vs-lease race). All governance
  mutations route through the runner (the last-owner guard is a write-skew only `SERIALIZABLE` catches);
  `workspace_events` idempotency is a hard `INSERT`, so a concurrent same-`op_id` governance dup aborts
  rather than double-applies. Compile-time-checked `query!` against the committed `.sqlx`;
  `cargo sqlx prepare --check -- --tests` is the CI drift gate (the `--tests` scope matches how the metadata
  is generated — the seed + lifecycle helpers include `#[cfg(test)]`-only queries — and the CLI is pinned to
  the library version).
- **The DB-authoritative object-lifecycle / garbage-collection fence.** Migration `0002` adds the
  fenced `object_presence` (`present`/`deleting`/`absent`/`unavailable` + the `git_oid` locator/bridge key +
  `size` + the `location` column — now exercised by the offload below), the GC-excluded `upload_quarantine`,
  the `promotion_lease` (+ object child table), and `tombstones`. The transitions are guarded compare-and-swaps
  in `mod db` (a `deleting` object is **non-resurrectable** — the `present`-writer's `WHERE status='absent'`
  cannot fire on it); the orchestration (`lifecycle.rs`/`gc.rs`) builds **ingest** (quarantine + rehash +
  denylist), **migrate** (lease the full object set *before* migrating, server-side dedup, durable install,
  then record a real version + make the lease non-expiring on success), the **three-step mark-then-claim GC**
  (claim → unlink-outside-any-transaction → finalize; the keep-set is **exactly the read-authorization
  surface** — any `commit_object` edge ∪ a live lease ∪ an **open-non-stale `proposal_object` root** — so a
  readable object is never reclaimed and a reclaimed one reads 404), a **recovery sweep** (which re-verifies
  BOTH the `commit_object` edge AND the proposal arm on its re-claim — so a `deleting` row re-rooted after a
  crashed claim is spared — but NOT the lease, since a lease over a `deleting` object is a waiting migrate it
  must unblock), and a **quarantine janitor** (claim-before-rm, so a re-ingest that reuses an op id is never
  swept). The three are **public ops** — `Authority::run_gc` / `run_recovery` / `run_janitor` — the composing
  server MUST schedule (startup + periodic; this library holds no scheduler — `topos-plane`'s
  `spawn_maintenance` is the reference composition); their futures are `Send`
  (compile-pinned), the GC's advisory candidate scan anti-joins the keep-set in SQL (a pass is O(garbage),
  the guarded per-object claim stays the sole authority), and the server clock is one unit throughout —
  epoch **milliseconds** (the TTL constants are `*_MS`). The per-workspace `run_gc` is driven over the
  public **`Authority::workspaces()`** enumeration (the distinct workspace ids holding an `object_presence`
  row — ids only, a scheduling surface, not a read; recovery + janitor enumerate cross-workspace
  internally). The in-crate tests drive it (deterministic
  interleavings for the dedup race, the
  snapshot-then-delete race, cross-workspace isolation, crash recovery, and — pinned by an equivalence test —
  that the read arm and the two GC-claim arms evaluate the proposal predicate identically). `topos-gitstore`
  supplies the dumb byte primitives (quarantine staging, durable per-object install, loose-object delete).
- **The size-routed large-object store (offload).** At migrate (`install_one`, Step D) a file blob is routed
  by **size**: ≥ a configurable ~1 MiB threshold → the per-workspace **`LocalLargeStore`** (`location =
  large-local`), smaller → git (`location = git`); commits + trees always stay in git. A per-blob ~100 MiB
  **reject cap** fails typed at **ingest**, before any bytes are staged. **Identity is placement-independent**
  (every id is over real-byte sha256s, computed before any store write — a test forces the *same* bytes into
  each store and asserts identical `version_id`/`bundle_digest`); **no pointer object** (the tree faithfully
  carries the offloaded blob's `git_oid`, built via the gitstore plumbing editor). Reads dispatch on the
  recorded `location`: `read_object` (single object — through the same skill-scoped join, **404-not-403, never
  by bare hash**; a post-authz failure in *either* store is `Integrity`, never not-found) and `render_version`
  (whole bundle — tree-driven, the offloaded subset joined in memory by `git_oid`, every byte re-verified to
  its content id, the recomputed digest matched to the pin). GC's unlink step **dispatches on `location`**
  (a git loose-object delete, or a `LargeObjectStore::delete` keyed on the object id); the lease, the CAS, the
  `deleting` fence, and recovery are unchanged. Dedup-reuse honors the object's **recorded** location (never
  re-routes by a new candidate's size). Per-workspace large-object roots ⇒ **no cross-workspace dedup**. Every
  write routes through this migrate (the standalone all-git upload path was retired). Backend is the **local FS
  only** — the S3-compatible remote backend + the online backfill are the named next steps.
- **The pointer-move write (`set-current`) — publish · genesis · revert.** The `current` row this layer only
  created now **moves** in **one `run_serializable!` (`SERIALIZABLE` + retry) pure-DB transaction** (no filesystem op
  inside it): in-transaction authentication FIRST (the presented **workspace credential**'s sha256 resolved to its
  registry row — the lookup IS the authentication; unknown mints nothing durable) → receipt-replay →
  the revoked check + the **confirmed-membership** gate (one predicate on every lane; a revoke or a
  membership removal committed before the promotion is serialized ahead and blocks it) → **compare-and-set on the whole `(epoch, seq)` pair** (CONFLICT carries the live
  generation; a restore that bumps `epoch` while reusing `seq` is caught) → availability (every candidate
  object `present` + not tombstoned) + a **lease-completion gate** (the committed lease proves the migrate
  finished) → same-skill lineage + the **first-parent assert** → provenance + reachability written **before**
  the pointer advance (the immediate FK) and **before** the lease release (so the GC keep-set covers the
  objects continuously across the re-root — no reclaim window) → the **unsigned `WireCurrentRecord`** written to
  `current.record` (nothing signs) → a durable **all-outcome
  receipt** keyed `(workspace_id, device_key_id, op_id)` (a lost-ack retry replays it byte-for-byte) — with
  ONE carve-out: a **pre-authentication** DENIED (unknown/revoked device) is synthesized,
  **never persisted** (mirroring the governance preamble: an unauthenticated client must not mint durable
  attacker-keyed rows), its lease still released; a corrected same-op_id retry proceeds fresh. A
  candidate is re-verified **renderable** before the txn (the migrate path defers that re-check to here).
  **`revert --to <good>`** is a **forward** commit `{tree: good.tree, parents: [current]}` (`seq` advances,
  the pointer never moves backward); good's tree digest is read from its provenance row (migration `0003`
  added a `bundle_digest` column — the git commit does not persist it). The **protection gate REROUTES
  instead of refusing**: a device-lane direct publish (or revert) on an effectively-REVIEWED bundle — the
  per-bundle pin, else the `workspace_policy.review_required` default (`topos_effective_protection`, the
  one cascade) — by a plain MEMBER runs the propose arm and answers NEEDS_REVIEW with a `downgraded`
  detail (never a rejection); reviewer+ lands directly; genesis always lands; `APPROVAL_REQUIRED` and its
  preflight are DELETED. The catalog gate refuses every pointer write on an archived/deleted skill, typed.
  A registering publish also writes the directory rows atomically through the witness (catalog row + name
  mint, `everyone`/`--to` placement — gated by the CHANNEL's mode independently of the version gate, the
  outcome riding the receipt details — and the author's self-follow); the workspace default is still set
  by **`Authority::set_review_required(ws, bool)`** (a device-credential `PUT /policy` governance route
  over it is later work). The cross-skill lineage predicate is now
  **enforced transactionally** here. Migration `0003` adds `op_receipts` + `workspace_policy` + a fixture-seeded
  `device_registry`. Two-parent author merges are rejected wholesale (a later increment). Driven in-process
  by the interleaving tests (concurrent-publish → one OK + one stable CONFLICT; the ABA traps; lost-ack
  replay; revoke-blocks-promotion; post-promote GC-reachability; genesis; first-parent) — **no HTTP, no
  client**.
- **The contribute authority (`publish --propose` · `review --approve | --reject`).** The *contribute* motion,
  on the SAME shared write core (no second trust path). **`propose`** ingests + migrates a candidate like
  publish, then — in `run`'s propose arm, AFTER the shared CAS/availability/lineage/first-parent body — opens
  a `proposals` row + roots the bytes through `proposal_object` and releases the migrate lease, **without
  moving `current`** (`NEEDS_REVIEW`; born non-stale). A proposal's bytes are retained +
  readable ONLY while `open ∧ base == current`, **one derived predicate shared verbatim by the read arm and
  both GC-claim arms** — so keep == read across the eventless stale transition (no `commit_parent`, no
  backfill, no reaper), and the instant a publish stales it the unique objects drop out of both. **`approve`**
  uploads/leases nothing; it runs the shared body (a stale base ⇒ `CONFLICT` *before* availability), then
  locks the open proposal, enforces **four-eyes under `review_required`** (the proposer may not self-approve),
  records an `approvals` row, and reuses the SAME promote — whose `commit_object` write is the
  **`proposal_object → commit_object` handoff** to the permanent trunk root — then flips the status to
  `accepted` (sideways `seq += 1`). **`reject`** — with a
  MANDATORY non-empty reason on BOTH lanes now (typed + SYNTHESIZED when empty, since the reason is not
  in the receipt's bound identity; recorded on the row AND the author's verdict notice) — and the
  author-only **`withdraw`** (actor must equal the proposer, a typed durable denial otherwise; closes as
  `withdrawn` with NO notice, idempotent re-withdraw) are small standalone status-flip txns (no pointer
  move); the gate then stops matching and ordinary GC reclaims the unique bytes. A fresh `propose`
  SUPERSEDES the author's other open drafts on the skill (closed as `superseded`, no notice; the
  idempotent same-(candidate, base) re-propose still converges), and `revert` refuses a PURGED target
  typed (`TARGET_PURGED`, after the replay probe, before any staging) on both lanes. All
  outcomes are op_id'd + receipted (lost-ack replay is byte-identical); a reclaimed object reads **404, never
  Integrity** (the read-time re-authorize guard). Driven in-process by the **stale-approve** interleaving
  (approve@stale ⇒ CONFLICT → rebase + re-propose → approve@new ⇒ OK) and the **ABA** interleaving (a `revert`
  makes `current.tree == the proposal's base tree`, yet the whole-`(epoch,seq)` CAS still ⇒ CONFLICT) —
  **no HTTP, no client**.
- **The operator backup/restore epoch bump** (`Authority::restore_bump_epochs`). One `SERIALIZABLE`
  transaction locks the selected `current` rows (`FOR UPDATE`), re-stamps each pointer at `max(epoch + 1,
  epoch_at_least)` — SAME commit, SAME seq — and rewrites the stored `WireCurrentRecord`, updating
  ONLY the `current` table (no receipt/provenance/proposal change; an envelope-parity test pins the rebuilt
  `WireCurrentRecord` DTO to the promote path's), so a reused `(epoch, seq)` tuple after a database restore
  can't confuse the proposal-staleness predicate or an in-flight CAS / conditional GET — **concurrency
  correctness, not follower-alarm avoidance** (there is no rollback floor or alarm; nothing re-signs).
  At-rest encryption of the enrollment secret stays Planned.
- **The enrollment + governance issuance core (real, but basic).** The fixture-seeded device/roster/read-token
  era is over: this layer now **mints real credentials**. Migration `0006` adds the standalone `workspace`
  (deployment posture), the workspace-level RBAC `workspace_member` roster (DISTINCT from the per-skill
  `roster`), the opaque `invites` (+ `invite_skill`), `enrollment_grants` (+ `enrollment_grant_skill`),
  `device_auth_sessions`, `passcodes`, `admin_claim`, and the `workspace_events` governance audit + op_id
  idempotency store — all ws-scoped, 32-byte BYTEA width-checked, with NO foreign key
  onto the standalone `workspace` (so the existing publish/read tests, which seed no workspace, stay green);
  (`enrollment_grant_skill` and `read_token` were later DROPPED by `0014` — the workspace-credential
  clean break). **Every opaque credential is
  deterministically HMAC-derived** (`hmac`/`sha2` over a `0600` enrollment secret loaded with the same `0600`
  seed custody) and **stored ONLY as its sha256** — so a lost-ack retry re-derives the IDENTICAL credential, a
  consumed grant re-derives the SAME workspace credential (naturally idempotent redeem), and a revoke is an
  instant row
  flip; `device_code`/`user_code`/`passcode` are fresh `getrandom`. The ops, all decided IN-Authority against
  server-trusted rows (never a client-asserted id): **`start_device_auth`** (RFC-8628-shaped, toward a
  workspace ADDRESS name — syntax-validated typed; RESOLUTION is never disclosed: an unknown name opens
  the same `pending` session and runs to the redeem's one uniform denial; the device key id is
  **server-derived** `dk_<…>` from the public key, never client-asserted; sessions are born `pending` on
  EVERY posture — identity proof is always a passcode or a web approval, the self-host born-confirmed
  shortcut died with the invite bearer token that anchored it), **`start_login_device_auth`** (the
  workspace-less LOGIN session — sign-in + credential recovery, allowed on BOTH postures),
  **`poll_device_auth`** (pending/slow-down/denied/expired/granted;
  the grant is deterministic so a re-poll re-issues the SAME one, records the session's INTENT — a
  standup session issues an enroll-intent grant — and a granted poll's workspace context carries the
  ADDRESS), **`start_passcode`**/**`complete_passcode`**
  (the email parsed INSIDE the op, a constant-shaped ack, brute-force locked after a cap; the identity
  legs serve enroll AND login sessions — only standup is excluded), the central
  **`redeem_enrollment`** (ONE `run_serializable!` txn: grant → expiry → intent [a login grant here reads
  as the uniform denial] → binding equality — the presented device key must equal the GRANT's bound key →
  THE MEMBERSHIP GATE, the roster as the lock on EVERY posture: an unresolved address, a wrong-workspace
  redeem, a vanished workspace row, and an off-roster identity all answer ONE byte-identical
  `ENROLL_UNAVAILABLE` denial → the invited→confirmed seat flip → device registry register with
  anti-squat, WRITING
  the device's ONE **workspace credential** (`derive_token(b"wscred", [grant_sha256])` — deterministic, so a
  lost-ack replay re-returns the identical plaintext; only its sha256 lands on the registry row; a re-redeem
  through a FRESH grant rotates it) — **NEVER a user token, never a per-skill token**),
  **`redeem_login`** (the LOGIN door: prove the grant's bound key, then register this device + re-mint
  its credential in EVERY workspace where the proven identity holds a confirmed seat —
  `derive_token(b"wscred", [grant_sha256, ws])`, deterministic per (grant, workspace) so a lost-ack
  replay re-returns identical plaintexts; a revoked or key-squatted seat comes back `blocked` with no
  side effect there; zero seats is a valid empty success), and **`admin_claim`** (self-host first-boot standup — same `b"wscred"` mint over
  the claim's sha256, so the consumed-replay probe re-returns the identical credential).
  The **governance** mutations (`roster_set`/`roster_remove`/`revoke_device`) resolve the ACTING workspace
  credential to its registered device in-transaction (resolve → request identity bound to the RESOLVED
  `device_key_id` → replay → revoked → role, so a since-revoked owner still replays its committed OK) under
  the canonical
  request identity `TOPOS_DEVICE_GOVERNANCE_V1`, enforce the role matrix (owner-only for invite/roster;
  owner-or-self for revoke) + a
  last-owner-lockout guard, are op_id-idempotent via `workspace_events` (a same-op_id retry with a matching
  `request_sha256` replays; a different one is a denied key-reuse), and revoke is **instant** (flip `revoked`
  in one txn — the row and its credential hash stay, so replay survives while fresh work is denied; member
  removal deletes the `workspace_member` row, which every gate joins — access dies with the row). Two more read/confirm ops feed the verification surface: **`read_verification_context`** (the
  RFC-8628 confused-deputy disclosure — resolve a LIVE, non-expired session by `user_code` and return the
  machine name + device fingerprint, the workspace identity, and the offered skills; no secret; a miss/expiry
  is the one indistinguishable `NotFound`) and **`confirm_external_identity`** (the OIDC callback's
  in-Authority half — set a live session's `confirmed_principal` + status `confirmed` from an
  already-proven email, the email parsed INSIDE the op; `complete_passcode`'s confirm minus the code check).
  Driven in-process by the device-flow→grant→redeem happy path, the binding-equality teeth (a leaked grant
  redeemed on a different key ⇒ DENIED), deterministic redeem idempotency, the cloud roster gate, self-host SMTP-free
  membership, instant revoke, the governance role matrix, server-derived device ids, the verification-context
  disclosure, and the external-identity confirm-then-grant — **no HTTP** (the verification-page HTML and the
  OIDC/magic-link transport + the mailer land in `topos-plane`). Test-fixture
  shims gain `seed_workspace` / `seed_workspace_member`; `seed_device` seeds the credentialed row.
- **Workspace standup (the first-boot genesis authority).** Three doors onto ONE shared genesis seat
  (`seat_workspace_and_owner`: the workspace INSERT's `ON CONFLICT DO NOTHING … RETURNING` probe is the
  created-or-exists witness, and the confirmed-`owner` member INSERT runs ONLY on Created — no genesis path
  can seat an owner into a live workspace; the deployment mode is a PARAMETER threaded from the plane's
  config, never a request). (1) **The standup device flow** — `start_standup_device_auth` (CLOUD planes
  only; self-host ⇒ the uniform `NotFound`) opens a session with `intent = 'standup'` and NO workspace,
  minting the same HIGH-entropy OPAQUE `user_code` enroll uses (a 32-byte base64url token, ~256 bits — it
  rides only inside `verification_uri_complete` and is clicked, never typed, so entropy is the only dial;
  approval CREATES ownership with no roster gate behind it, so a live code must be unguessable within its
  TTL); `approve_standup` (lib-only,
  for a composing web leg with an already-verified email) runs cap → fresh-`w_<hex32>`-id seat → the
  session's pending→confirmed CAS in ONE txn — the CAS is the idempotency (same-email re-click ⇒
  `AlreadyApproved`; different email / unknown / expired / enroll-intent ⇒ the single indistinguishable
  `NotFound`). The granted poll now carries the workspace's `{id, display name, address}` and the redeem
  outcome its `principal`. (2) **`create_workspace`**
  (lib-only) — the same genesis body for a verified email, idempotent per `request_id` via
  `genesis_requests` (same request + same owner replays the SAME workspace + the SAME ADDRESS — the
  genesis mints no invite link: the address IS the share line; a different owner is denied;
  `genesis_requests_pkey` and `workspace_by_name` sit in the serializable runner's
  convergent-23505 set so racing same-request/same-name creates converge). Both doors share the per-identity creation
  cap (3 confirmed-owner memberships), the freemail-aware domain claim (a non-freemail owner domain is
  recorded `verified` — the sign-in proved an address on it), the server-side display-name default, and
  the ADDRESS-name resolution (`name: Some` is validated + must be free, both refusals typed; `None`
  slugifies the display name, dedupes `-2`…`-9`, and falls back to an id-derived `ws-…` name — the
  admin-claim redeem derives its slug from the mint-time display name through the same helper).
  (3) **The hardened one-time claim** — `mint_admin_claim` (typed refusals: an existing workspace; a
  cloud-mode mint without an owner email) stores mint-time facts (display name / owner email / expiry) the
  redeem trusts (the request's display name is disclosure-only); the refactored `admin_claim` orders
  consumed-replay-probe FIRST (a SAME-device replay of a consumed claim deterministically re-returns
  `Redeemed` — lost-200 recovery; expiry gates only the FIRST consumption) → expiry → anti-squat → seat (at
  THE PLANE'S mode) → register → consume, all checks before any write; `read_claim_bootstrap` serves the
  `/i/` claim branch (`enrollment_method: "admin_claim"`, no skills; consumed/expired/unknown = the uniform
  `NotFound`; claims and invites live in disjoint tables so a token never crosses doors). **First-writer-wins
  confirmations**: passcode + external-identity confirms are pending→confirmed CASes with an
  `intent = 'enroll'` guard (idempotent same-principal replay; different principal = the uniform miss; a
  confirmed principal is never overwritten; a standup session is only ever advanced by its approval). The
  claim-token plaintext is returned once and `MintedClaim`'s `Debug` redacts it. Driven in-process by the
  standup suite (`src/tests/standup.rs`): the full standup chain through the genesis-publish gate, the
  same-device replay + racing double redeem (exactly one owner row), the cap at the 4th create, the
  cross-door token separation, and the intent/first-writer-wins guards.
- **The web-session roster leg (real, but basic).** Three PRIVILEGED lib-level ops (no OSS HTTP route —
  a hosted composition's authenticated admin routes call them; self-host is uniformly denied in-op —
  self-host joining is the device lane): **`invite_members_session`** (seats
  emails at member|reviewer — owner is unrepresentable in `SessionInviteRole` — through the shared
  never-demote row-writer; an invitation is a ROSTER WRITE and nothing more, and what comes back is the
  workspace ADDRESS), **`roster_remove_session`**
  (the device lane's exact instant-revoke txn shape + `would_orphan_owner` lockout),
  and the **`read_roster`** privileged read (seats for
  any confirmed member, plus the address — member-visible: it is a name, not a door; joining still
  gates on the roster). The tokened standing-door machinery (its deterministic `b"door"` derivation,
  the genesis self-invite continuity, `link_epoch` rotation) died with the invite tables in 0016 —
  there is nothing to rotate when links carry nothing. Authorization is a
  signature-FREE session gate: replay BEFORE authz through the same `workspace_events` slot under a
  fresh `TOPOS_SESSION_ROSTER_V1` request identity (a device op id and a session request id fail
  closed against each other as key reuse), then the in-txn confirmed-OWNER check — ONE uniform denial
  for member/reviewer/invited/absent, and only a CONFIRMED member's denial is ever recorded (a
  stranger cannot grow the ledger or squat op-id slots). Receipts gain the `method` discriminant
  (`web_session` with the acting EMAIL as actor vs `device` with the presented device key id) —
  the audit trail says which leg acted, forever. Driven in-process by `src/tests/session_roster.rs`:
  the uniform acting gate + recording rule, role-on-the-seat seeding (a reviewer invitee redeems
  through the ADDRESS into a confirmed reviewer while a verified stranger dies at the roster gate),
  self-host denial, identical replay / divergent-payload + cross-leg key reuse in both directions,
  lockout + the raced mutual removes, canonical-principal folding, and the receipt method/actor matrix.
- **The web-session READ lane (member-scoped session reads).** Five PRIVILEGED lib-level read ops
  (no OSS HTTP route — a hosted composition's authenticated admin routes call them):
  **`list_skills_session`** (the workspace catalog — every skill holding a `current` row, with its
  pointer generation, epoch-ms update time, consent `bundle_digest`, and OPEN non-stale proposal
  count), **`read_current_session`**, **`serve_object_session`**, **`read_version_metadata_session`**,
  and **`list_open_proposals_session`**. ONE shared `member_gate` preamble authorizes them all —
  self-host uniformly denied, canonical principal fold, then a CONFIRMED `workspace_member` probe —
  and every pre-gate miss (self-host / malformed email or skill / unknown workspace / non-member /
  invited-unconfirmed) is the single indistinguishable `NotFound`. **Both lanes now run the SAME gate:
  access IS workspace membership** — any confirmed member, any
  role, reads the workspace's full catalog and every skill's
  content, on the session lane AND the device lane (the lanes differ only in authentication: verified
  session email vs. presented workspace credential; the earlier lane asymmetry — per-skill roster on
  the device lane — is deleted, and the per-skill `roster` table is GONE, lifted into person-scoped
  `skill_follows` by 0015). Mechanically, the read
  authorizations are split into the ONE membership GATE (`read_gate`) and ONE lane-blind
  reachability statement each (`object_witness` / `version_readable` / `open_proposal_rows`), so both
  lanes share identical reachability SQL and the `open ∧ base == current` staleness predicate stays at
  its FIVE tracked copies; the index's proposal count delegates per skill to the SAME listing
  statement (count == list by construction — a deliberate O(skills) fan-out on a cold route). The
  read-time re-authorize guard re-gates on the caller's principal: a reclaimed object reads 404 and genuine
  corruption stays an Integrity alarm on BOTH lanes (both directions pinned by `tests/session_read.rs`,
  alongside the full miss-uniformity matrix, the staled-proposal list/count parity, the
  rejected-candidate 404 through both lanes, and the NULL-digest-under-current Integrity probe). Reads
  mint nothing durable. The gate→reach two-statement window (a principal revoked between them completes
  one in-flight read) is the same accepted posture as the authorize-then-fetch TOCTOU.
- **The DEVICE-lane catalog read (`list_skills_device`) — an OSS HTTP route, unlike the session lane.**
  A public `Authority::list_skills_device(ws, credential, now)` that lets a member's
  **device** (not a web session) read the SAME workspace catalog `list_skills_session` returns: resolve
  the presented workspace credential to its non-revoked registry row (the lookup IS the authentication) →
  `confirmed_member`, then the shared
  `build_skill_index` (the session lane's index build, factored out and shared verbatim). Every failure
  folds to the one uniform `NotFound` (a corrupt stored principal stays `Integrity`). It takes **no
  `DeploymentMode`** and applies **no self-host denial** — device auth IS the self-host membership story,
  so this lane serves the catalog on BOTH cloud and self-host (the property that unifies the OSS/cloud
  catalog-visibility split: catalog visibility == workspace membership on every lane; the lanes differ
  only in how the principal is authenticated — session email vs. presented device credential). Served by
  `topos-plane`'s `GET /v1/workspaces/{ws}/skills` (the FIRST HTTP-routed member-scoped read; the session
  reads stay lib-only). Driven by `src/tests/session_read.rs`'s device-lane suite (member reads the
  catalog; a cross-workspace device selector, revoked device, and non-member all `NotFound`; and the key
  contrast — the device lane SERVES a member on self-host where the session lane denies).
- **The workspace credential — ONE membership credential per (principal × workspace ×
  device), authenticating EVERYTHING on the device lane.** The per-skill read tokens and the interim
  non-secret `device_key_id` authentication are both GONE (migration `0014`: `credential_sha256` on
  `device_registry` + `DROP TABLE read_token, enrollment_grant_skill`). The credential is a bearer
  secret (HMAC domain `b"wscred"` over the grant/claim sha256 — deterministic per redemption door, so
  lost-ack replays re-return the identical plaintext), stored ONLY as its sha256 ON the device's
  registry row (one row, one device, one credential; the partial-unique index is the resolver's O(1)
  probe, workspace-bound so a credential never crosses workspaces). Presentation is the
  `Authorization: Bearer` header — never a body field, so the secret never enters a receipt request
  identity or the client's persisted op-WAL, and a rotation between retries can't break byte-identical
  replay. The pointer-move + reject transactions authenticate at a new step (0) — the in-transaction
  resolve, BEFORE any probe or durable write (an unknown credential mints nothing, closing the old
  pre-txn hole where unauthenticated callers could grow `op_receipts` via the review-gate preflight) —
  with the revoked check deferred past the replay probe (a since-revoked device still replays its
  stored OK; its fresh work is denied). Authorization on every lane is the ONE membership predicate
  (`confirmed_member`); the `WriteActor::Device` carries `(credential_sha256, pool-resolved
  device_key_id)` and the txn asserts the in-txn resolution names exactly that device. Revocation is a
  directory row-write, effective in-transaction: a device revoke flips `revoked` (the row + hash stay —
  replay survives, fresh work dies, re-enrollment is refused); a member removal deletes the
  `workspace_member` row (every read/write gate joins it — access dies the moment it commits) and runs
  the lapse-detach reconcile (the person's devices' fleet rows get their final detach records — the
  removed-member blind-spot the fleet page names). Rotation = re-enrollment (a fresh grant derives a fresh
  credential and the register upsert replaces the column). NO expiry (journaled: revoke + re-enroll is
  the rotation).
- **The web-session REVIEW leg (real, but basic).** Three PRIVILEGED lib-level ops (no OSS HTTP route —
  a hosted composition's authenticated admin routes call them; self-host uniformly denied in-op):
  **`review_approve_session`** / **`review_reject_session`** (approve / reject an OPEN proposal from a
  verified session) + **`read_proposal_detail_session`** (the review surface's read). The write
  TERMINATES in the SAME serializable pointer-move transaction the device lane runs (`db/set_current.rs`'s
  `run` and the reject transaction) — one approve predicate, one `(epoch,seq)` CAS, one moved
  pointer, one four-eyes gate — branching on the new `WriteActor` (Device|Session; `actor.rs`) ONLY at the
  authorization step: the device arm authenticates by the in-transaction credential resolve + the
  membership gate, and the session arm is an
  in-transaction confirmed **owner|reviewer** workspace-seat gate — **the FIRST enforcement of the
  reviewer role** (the remaining lane asymmetry is ROLE alone — deliberate, for now:
  CLI approve/reject takes any confirmed member + four-eyes; finer role gating is later work). Orchestration
  (`session_review.rs`) mirrors the roster leg's trust shape: uniform self-host deny, a canonical-UUID
  `request_id` idempotency under a fresh `TOPOS_SESSION_REVIEW_V1` domain tag (distinct from every kernel
  and roster tag, so no stored identity from another domain can byte-match a review request), a
  POOL-LEVEL confirmed-member pre-gate BEFORE any proposal/digest/render work (the in-txn role gate stays
  the authority), and a MANDATORY non-empty reject reason. **The recording rule**: an unproven caller's
  refusal is SYNTHESIZED, never persisted (a web-verified email proves nothing about membership in the
  target workspace — it must not grow `op_receipts` or squat op-id slots), while a CONFIRMED plain
  member's role refusal is a DURABLE typed `REVIEWER_ROLE_REQUIRED` denial (a member is entitled to a
  recorded, replayable answer). Migration `0012` renames `op_receipts.device_key_id → actor` (the slot
  always held the acting identity — a device key id, or now the session's verified EMAIL),
  adds the `method` discriminant (`device` | `web_session`, after 0013's `device_signed → device` rename) +
  `request_sha256` (the session lane's
  full-request identity; NULL on the device lane, whose identity is the resolved device key id) + a
  reserved `step_up_attestation` column + the `(workspace_id, op_id)` index, and adds
  `proposals.resolved_reason` + `resolved_at` (both lanes REQUIRE the reason now — the device lane
  caught up in the verb reshape).
  The receipt replay probe is now **lane-blind** per `(workspace, op_id)`: cross-lane id reuse fails
  closed in BOTH directions (a device op id and a session request id never replay each other), while each
  lane's own slot still replays byte-identically on a full `(method, actor, request_sha256)` match — the
  per-device slots are preserved. `read_proposal_detail_session` (its read sibling, in `session_read.rs`
  over the shared member gate) discloses the proposer + resolution + `review_required` policy at read time
  — **proposer disclosure on the session lane only** (the thin `/v1` proposals listing stays
  proposer-free and byte-unchanged). Consent stays end-to-end: followers re-verify
  bytes against the approved digest (nothing signs the moved pointer) — the receipt's `method`/`actor` is
  the audit trail for which leg
  acted. Public Authority ops `review_approve_session` / `review_reject_session` /
  `read_proposal_detail_session` — **and `revert_session`** (the web one-click "roll back to this
  version"): the SAME confirmed owner|reviewer gate on the shared pointer-move transaction, but a
  **forward promote** that bypasses the review gate + four-eyes by design (the safety net). It
  actor-parameterizes `set_current::revert` (the device lane keeps `Authority::revert` byte-identical) and
  the txn's Session arm now admits `Revert` as well as `ReviewApprove`. Because a revert CONSTRUCTS a
  forward commit before the txn, its idempotency is a session twin of `replay_revert`
  (`replay_revert_session` — keyed on acting email + `request_sha256` under a fresh
  `TOPOS_SESSION_REVERT_V1` tag, since the forward commit id re-parents on live `current` and changes per
  retry), and a **cheap pre-stage owner|reviewer fence** turns a plain member away BEFORE the staging
  (synthesized, never persisted — the pre-stage variant of the recording rule; the in-txn gate stays
  authoritative). A concurrent duplicate that re-stages a lease past the in-txn replay HIT now releases it
  (the strand fix, mirrored to the Mismatch arm). Driven in-process by `src/tests/session_review.rs` (the role-gate +
  recording-rule matrix, cross-lane four-eyes both directions, stale/ABA CONFLICTs through the session
  lane, cross-lane id reuse in all four directions, the divergent-reason tripwire, both concurrency
  races, the detail read incl. the open-row preference, **and the revert leg: reviewer happy path +
  byte-identical replay, the owner|reviewer/member/stranger role matrix with the member's synthesized
  refusal, the not-accepted-target refusal, the stale CAS CONFLICT, cross-lane op-id closure, and
  self-host deny**) and `src/tests/receipts_migration.rs` (the 0012
  probe: rename/backfill/CHECKs/index), plus the request-identity unit tests (`session_review.rs`) and
  the wrapper classification-table test (`topos-plane`).
- **Canonical principal form — one mailbox, one identity.** `Principal::parse` folds every principal
  to the kernel's ASCII-lowercase form (`topos_core::identity::canonical_principal` — the same fold the
  kernel applies to every email-valued identifier, so one mailbox is one identity at every gate),
  which makes every roster gate, seat write, idempotency hash, and the
  owned-workspace cap case-insensitive for one human's mailbox: a lowercased invite seat now matches
  a mixed-case device-confirmed principal at the redeem gate ("invited but can't join" is dead), and
  a mixed-case owner seat accepts its lowercased web session. Migration `0010` folds the durable
  rows that predate the rule — deduping case-variant duplicates deterministically first (`roster`
  losslessly; `workspace_member` keeps the strongest seat: confirmed > invited, then owner >
  reviewer > member, then earliest `added_at`) — and pins the invariant with
  `lower(… COLLATE "C")` CHECKs on `workspace_member` + `roster`. Ephemeral flow tables and the
  audit ledger are deliberately not rewritten (an in-flight mixed-case enrollment crossing the
  deploy re-runs fresh; history stays as recorded). Driven by the mixed-case redeem/session/cap
  tests in `src/tests/enrollment_governance.rs` + `src/tests/session_roster.rs` and the
  migration-logic probe in `src/tests/canonical_migration.rs`.

- **The channels model: the catalog + channels + person-scoped subscriptions + the
  delivery predicate + the skill lifecycle.** Migration `0015` (see the schema bullet) puts EVERY policy
  decision in guarded `topos_*` SQL functions — the one implementation Rust calls today and the web tier
  calls at the door cutover — with the channel audit TRIGGER-emitted so no write path can skip it, and
  `everyone` structural (builtin row; membership derived from the confirmed roster; undeletable,
  unrenameable, unjoinable/unleavable — DB-held invariants). **Delivery is ONE SQL home**
  (`topos_entitled_skills`, extending the confirmed-membership predicate): DISTINCT union of
  roster-derived `everyone` ∪ followed channels ∪ direct follows − unfollowed skills − this device's
  exclusions, active + current-holding skills only, with `via` attribution and the resolved protection —
  served by `Authority::delivery` (+ the person's detached set, the unacked notices feed, the
  open-proposal count) and written back by `Authority::report_applied` (snapshot upsert; detach records
  immutable; `last_report_at` the staleness clock). The WHO-ACTS placement is server-legible: unfollow /
  channel-leave / member-removal run the lapse-detach reconcile (final per-device detach records,
  reference-counted via the union); `follow`/join re-attach. Curation is member-level on `open` channels,
  reviewer+ on `curated`; `protect` tightens at reviewer+ and loosens only at owner, per kind. The
  LIFECYCLE session ops (owner; self-host denied like every session op): archive renames
  (`<name>-archived-<date>`, counter on repeats) FREEING the base name (id-keyed references make a reused
  name a new identity), unplaces everywhere, auto-closes open proposals with author NOTICES; unarchive
  renames back or refuses typed (`NameTaken`); delete (archive-first) tombstones the catalog row and
  un-roots all content for the shipped GC; purge un-roots ONE version's bytes (refused while `current`;
  the hash stays as a who/when tombstone; only blobs unreachable from live versions drop — no
  object-denylist, content-addressed bytes may legitimately reappear). Verdict + closure notices are
  person-scoped rows written IN the deciding transaction. Driven by `src/tests/channels_*.rs` + the
  adapted `set_current`/`contribute`/`session_*` suites.

## Backend shape (Postgres-only)

`Authority` holds a concrete `db::Db` directly — no trait, no `sqlx::Any`, no dialect enum: SQLite was
removed, and Postgres is the single backend. The load-bearing invariant is that **no `sqlx` type ever
crosses the `db` module boundary**: every method there takes the id newtypes + data and returns plain domain
values, so the authority code above it is storage-shaped, never SQL-shaped.

## Planned (lands later)

The large-object store's **S3-compatible remote backend** (a second `LargeObjectStore` impl + a
`large-remote` `location` arm — a no-op extraction) and its **idempotent online backfill** (copy → verify →
flip `location` → `git repack`), both additive + client-invisible; **multi-reviewer governance**
(`min_approvers` / N-approver / queues / a rendered diff UI — single-approver only today; the reviewer ROLE
is now enforced as the session-review acting gate (a confirmed owner|reviewer seat), but multi-approver
flows and role-scoped queues stay planned; the client contribute loop + the proposals-listing read route
that feeds it are now BUILT); the
**HTTP plane's still-to-come surface** over the issuance core (the audit outbox — the enrollment +
governance request/response DTOs, the mailer, and one generic OSS OIDC connector all landed in
`topos-plane` earlier, and the workspace-policy
mutation route is now BUILT there as the admin-token `PUT …/policy/review-required`; verification-page HTML
is a composing web layer's surface, never this repo's — the JSON routes + the `topos-plane` lib wrappers
are the seam, and hosted compositions serve their own pages over them); **active credential
rotation** in the `current` path (the workspace credential has NO expiry by decision — revoke +
re-enroll IS the rotation; an in-place rotate-without-re-enroll op is later work if ever needed);
domain-ownership **verification** (`verified_domain_status` is operator-asserted);
**at-rest encryption / KMS of the enrollment secret** (a plaintext `0600` seed for
now); the `purge`/lifecycle WEB CEREMONIES (the authority ops + guarded functions are BUILT; the step-up
pages are the web tier's); two-parent author merges; per-skill encryption-at-rest. (Notices ACK left
this list — `topos_notices_ack` + `Authority::ack_notices` are BUILT.)

## Build note

`sqlx`-postgres is **pure Rust** — no bundled C library — so building the server crate (and the plane
binary) needs **no C toolchain**; the old `libsqlite3-sys` build edge is gone from the tree entirely. The
**client never gets a `plane-store` / `sqlx` edge** — `cargo run -p xtask -- check-arch` asserts `topos`
depends on neither.

Dependencies: `topos-core`, `topos-types`, `topos-gitstore`, `thiserror`, raw `sqlx` (postgres,
runtime-tokio, macros, migrate, tls-rustls-ring-native-roots); `tokio` with the `time` + `rt` features is a
**normal** dependency (`time`: the migrate deleting-wait uses a bounded-backoff sleep while it polls outside
any write transaction; `rt`: `spawn_blocking` isolates the fsync-heavy/verify-on-read store sections onto
the blocking pool) — arch-clean because the client takes no edge to `plane-store`; `tokio`'s `macros` is
dev-only (to drive `#[tokio::test]`). The async runtime itself is still the caller's, via sqlx's
`runtime-tokio` feature.

**No `ed25519-dalek` edge, and no signer:** nothing in this crate signs. Authentication is credential
lookup against live directory rows, and the stored pointer/receipt is the **unsigned `WireCurrentRecord`**
(`serde_json` serializes it into the stored `BYTEA`). The device keypair is a *presented identity*: the
client (`bins/topos`) keeps the keygen-only `ed25519-dalek` dependency to generate it, and `check-arch`
forbids the `topos → plane-store` edge, so the client reaches none of this crate.

The **enrollment issuance core** + the `0600` secret custody add, to **this crate only** (the client
reaches none of it): `hmac` + `sha2` (HMAC-SHA256 — the deterministic opaque-credential derivation over the
`0600` enrollment secret; `sha2`'s `Sha256` is the HMAC backend, the same `default-features = false` 0.10
pin `topos-core` uses), `getrandom` (the OS CSPRNG for fresh device-code / user-code / passcode values and
the first-run secret seed), `zeroize` (the `Zeroizing` custody around the raw secret seed), `base64` (the
credential codec), and `uuid` (validating the canonical lowercase-hyphenated op-id spelling the receipt slot
is keyed on). The enrollment secret never reaches the client.
