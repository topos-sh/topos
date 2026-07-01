# `plane-store` — the server authority boundary

**A crate so that raw access is private.** It owns the plane's per-workspace SQL — **raw `sqlx`, no
ORM** — and per-workspace git-object storage, and it is the single place the **skill-scoped access rule**
is enforced. The pool, every transaction, every raw SQL statement, and every raw git-object read are
`pub(crate)`-private; the **only** public surface is authorized authority operations on `Authority`.

## The privacy boundary IS the security mechanism

No code outside this crate can run an unbound query or read a bare object — that is unbypassable by
construction. (This is misuse-prevention by encapsulation; it is not isolation against malicious
same-process code.) The error type holds this line too: internal faults carry a **boxed** source, so no
`sqlx` or git-store type appears in any public signature.

## Implemented (each behind a test in `src/tests.rs` + the module unit tests)

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
  **gated** retention/read root for a pending proposal), and `approvals` (the audit log).
- **`Authority::read_object`** — the skill-scoped read. One join authorizes on rostered ∧ reachable —
  reachable through EITHER the accepted trunk (`commit_object`) OR an **open, non-stale proposal**
  (`proposal_object`), the latter gated on the **same** `open ∧ base == current` predicate the GC keep-set
  uses, so **keep-set == read surface** — and yields a witness commit; the bytes are then read + re-verified.
  Every not-entitled/not-found case returns one indistinguishable `NotFound`; a store failure on an
  already-authorized object is a separate `Integrity` fault (corruption), never a not-found. A post-authz
  fetch miss **re-authorizes** (the read-time TOCTOU guard): a proposal that staled — and whose unique bytes
  a GC reclaimed — between the authorize and the fetch reads **404, never Integrity**. **No object is served
  by bare hash.**
- **The network read surface (what the HTTP plane composes over).** `resolve_read_token` maps an opaque
  per-skill read token (stored only as its **sha256**) to a `ReadScope` whose `(workspace, skill, principal)`
  are built from the trusted row — **never** a caller-asserted id — a miss being the same indistinguishable
  `NotFound`. Over it: `read_current` (the signed-`current` record + its generation/version, for the
  conditional-GET/ETag/304 read), `serve_object` (the bundle read — a scope/path mismatch or a malformed id
  is `NotFound`, then the same `read_object`), `read_version_metadata` (a version's
  parents/author/message/digest/file-list — **no blob bytes** — for the client's reassembly walk), and
  `list_open_proposals` (the OPEN proposals on a rostered skill as `{version_id, base, created_at}` —
  **count + handles only, no bytes, no roles**: the reviewer's discovery surface for `proposals_awaiting` /
  `list <skill>`; reuses the SAME `open ∧ base==current` staleness clause verbatim — the **fifth** tracked
  copy — so a staled proposal vanishes [keep==read==list], and folds not-rostered into an empty list via the
  roster join, never a 403/oracle). The version-metadata read is R1-scoped by `authorize_version_read`, which
  **mirrors `read_object`'s predicate** (rostered ∧ accepted-trunk-or-open-non-stale-proposal), so an
  unaccepted/rejected proposal version is the indistinguishable `NotFound`; `list_open_proposals` applies the
  same scope/path assert first (a cross-skill/workspace token ⇒ `NotFound`). Commit metadata comes from gitstore's exact one-commit `read_commit_meta`
  (fails closed on an unmapped parent, never the lossy `log`). `read_signed_record` is now `pub(crate)` (the
  public authenticated read is `read_current`). `SetCurrentReceipt` is enriched (command/skill/version/digest/
  expected/created_at — all already persisted) so the network layer builds the canonical all-outcome receipt
  and replays it byte-for-byte. A feature-gated **`test-fixtures`** surface (roster / device / read-token / a
  published genesis + child + a signature-tamper helper) lets an out-of-crate test drive a loopback plane; it
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
  (SQLSTATE `40001`) or deadlock (`40P01`), whether raised by a statement or by `COMMIT`. It is a **macro**,
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
  server MUST schedule (startup + periodic; this library holds no scheduler); their futures are `Send`
  (compile-pinned), the GC's advisory candidate scan anti-joins the keep-set in SQL (a pass is O(garbage),
  the guarded per-object claim stays the sole authority), and the server clock is one unit throughout —
  epoch **milliseconds** (the TTL constants are `*_MS`). The in-crate tests drive it (deterministic
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
  created now **moves**, **signed**, in **one `run_serializable!` (`SERIALIZABLE` + retry) pure-DB transaction** (no filesystem op
  inside it): receipt-replay → in-transaction authoritative authz (a device-op signature verified against the
  registry's **non-revoked** public key bound to a **rostered** principal — a revoke committed before the
  promotion blocks it) → **compare-and-set on the whole `(epoch, seq)` pair** (CONFLICT carries the live
  generation; a restore that bumps `epoch` while reusing `seq` is caught) → availability (every candidate
  object `present` + not tombstoned) + a **lease-completion gate** (the committed lease proves the migrate
  finished) → same-skill lineage + the **first-parent assert** → provenance + reachability written **before**
  the pointer advance (the immediate FK) and **before** the lease release (so the GC keep-set covers the
  objects continuously across the re-root — no reclaim window) → an **in-process Ed25519 signer** (the only
  private-key holder; load-or-generate `0600`; signs the JCS pointer preimage) → a durable **all-outcome
  receipt** keyed `(workspace_id, device_key_id, op_id)` (a lost-ack retry replays it byte-for-byte) — with
  ONE carve-out: a **pre-authentication** DENIED (unknown/revoked device, invalid signature) is synthesized,
  **never persisted** (mirroring the governance preamble: an unauthenticated client must not mint durable
  attacker-keyed rows), its lease still released; a corrected same-op_id retry proceeds fresh. A
  candidate is re-verified **renderable** before the txn (the migrate path defers that re-check to here).
  **`revert --to <good>`** is a **forward** commit `{tree: good.tree, parents: [current]}` (`seq` advances,
  the pointer never moves backward); good's tree digest is read from its provenance row (migration `0003`
  added a `bundle_digest` column — the git commit does not persist it). The **review-required typed-fail
  gate** is built (a direct publish under the policy short-circuits to `APPROVAL_REQUIRED` having ingested
  nothing; genesis + revert bypass it); the policy is set by the public **`Authority::set_review_required(ws,
  bool)`** (a `workspace_policy` upsert — the test-only `seed_review_required` now delegates to it; the
  device-signed `PUT /policy` governance route over it is later work). The cross-skill lineage predicate is now
  **enforced transactionally** here. Migration `0003` adds `op_receipts` + `workspace_policy` + a fixture-seeded
  `device_registry`. Two-parent author merges are rejected wholesale (a later increment). Driven in-process
  by the interleaving tests (concurrent-publish → one OK + one stable CONFLICT; the ABA traps; lost-ack
  replay; revoke-blocks-promotion; post-promote GC-reachability; genesis; first-parent) — **no HTTP, no
  client**.
- **The contribute authority (`publish --propose` · `review --approve | --reject`).** The *contribute* motion,
  on the SAME shared write core (no second trust path). **`propose`** ingests + migrates a candidate like
  publish, then — in `run`'s propose arm, AFTER the shared CAS/availability/lineage/first-parent body — opens
  a `proposals` row + roots the bytes through `proposal_object` and releases the migrate lease, **without
  moving `current` or signing anything** (`NEEDS_REVIEW`; born non-stale). A proposal's bytes are retained +
  readable ONLY while `open ∧ base == current`, **one derived predicate shared verbatim by the read arm and
  both GC-claim arms** — so keep == read across the eventless stale transition (no `commit_parent`, no
  backfill, no reaper), and the instant a publish stales it the unique objects drop out of both. **`approve`**
  uploads/leases nothing; it runs the shared body (a stale base ⇒ `CONFLICT` *before* availability), then
  locks the open proposal, enforces **four-eyes under `review_required`** (the proposer may not self-approve),
  records an `approvals` row, and reuses the SAME promote — whose `commit_object` write is the
  **`proposal_object → commit_object` handoff** to the permanent trunk root — then flips the status to
  `accepted` (sideways `seq += 1`, signed). **`reject`/withdraw** is a small standalone status-flip txn (no
  pointer move, nothing signed); the gate then stops matching and ordinary GC reclaims the unique bytes. All
  outcomes are op_id'd + receipted (lost-ack replay is byte-identical); a reclaimed object reads **404, never
  Integrity** (the read-time re-authorize guard). Driven in-process by the **stale-approve** interleaving
  (approve@stale ⇒ CONFLICT → rebase + re-propose → approve@new ⇒ OK) and the **ABA** interleaving (a `revert`
  makes `current.tree == the proposal's base tree`, yet the whole-`(epoch,seq)` CAS still ⇒ CONFLICT) —
  **no HTTP, no client**.
- **The operator backup/restore epoch bump** (`Authority::restore_bump_epochs`). One `SERIALIZABLE`
  transaction locks the selected `current` rows (`FOR UPDATE`), re-signs each pointer at `max(epoch + 1,
  epoch_at_least)` — SAME commit, SAME seq, via the same `PlaneSigner` + frozen JCS preimage — and updates
  ONLY the `current` table (no receipt/provenance/proposal change; an envelope-parity test pins the rebuilt
  signed-record DTO to the promote path's), so after a database restore every follower's next record is
  strictly higher and ordinary forward sync resumes instead of a reused-tuple ALARM. At-rest key encryption
  stays Planned.
- **The enrollment + governance issuance core (real, but basic).** The fixture-seeded device/roster/read-token
  era is over: this layer now **mints real credentials**. Migration `0006` adds the standalone `workspace`
  (deployment posture), the workspace-level RBAC `workspace_member` roster (DISTINCT from the per-skill
  `roster`), the opaque `invites` (+ `invite_skill`), `enrollment_grants` (+ `enrollment_grant_skill`),
  `device_auth_sessions`, `passcodes`, `admin_claim`, and the `workspace_events` governance audit + op_id
  idempotency store — all ws-scoped, 32-byte BYTEA width-checked, with NO foreign key
  onto the standalone `workspace` (so the existing publish/read tests, which seed no workspace, stay green);
  it also adds nullable `device_key_id` + `expires_at` to `read_token`. **Every opaque credential is
  deterministically HMAC-derived** (`hmac`/`sha2` over a `0600` enrollment secret loaded with the plane key's
  exact custody) and **stored ONLY as its sha256** — so a lost-ack retry re-derives the IDENTICAL credential, a
  consumed grant re-derives the SAME read tokens (naturally idempotent redeem), and a revoke is an instant row
  flip; `device_code`/`user_code`/`passcode` are fresh `getrandom`. The ops, all decided IN-Authority against
  server-trusted rows (never a client-asserted id): **`create_invite`** (owner-signed; mints the `/i/<token>`
  link, seeds the invited members, op_id-idempotent), **`read_invite_bootstrap`** (the no-bytes, no-role
  payload + the plane signing root), **`start_device_auth`** (RFC-8628-shaped; the device key id is
  **server-derived** `dk_<…>` from the public key, never client-asserted; cloud sessions are `pending`,
  self-host born `confirmed` device-rooted), **`poll_device_auth`** (pending/slow-down/denied/expired/granted;
  the grant is deterministic so a re-poll re-issues the SAME one), **`start_passcode`**/**`complete_passcode`**
  (the email parsed INSIDE the op, a constant-shaped ack, brute-force locked after a cap), the central
  **`redeem_enrollment`** (ONE `run_serializable!` txn: a possession proof via `topos_core::sign::verify_enroll`
  against the GRANT's bound key → the deployment-mode roster gate [cloud requires a confirmed, already-rostered
  identity; self-host grants membership from the bearer] → device registry register with anti-squat → per-skill
  roster + **minted read tokens, NEVER a user token**), and **`admin_claim`** (self-host first-boot standup).
  The **governance** mutations (`roster_set`/`roster_remove`/`revoke_device`) verify a
  `topos_core::sign::verify_governance_op` signature in-transaction against the signer's non-revoked registered
  device, enforce the role matrix (owner-only for invite/roster; owner-or-self for revoke) + a
  last-owner-lockout guard, are op_id-idempotent via `workspace_events` (a same-op_id retry with a matching
  `request_sha256` replays; a different one is a denied key-reuse), and revoke is **instant** (flip `revoked` +
  drop the device's read tokens in one txn). `resolve_read_token` now takes `now` and enforces the token's
  `expires_at`. Two more read/confirm ops feed the verification surface: **`read_verification_context`** (the
  RFC-8628 confused-deputy disclosure — resolve a LIVE, non-expired session by `user_code` and return the
  machine name + device fingerprint, the workspace identity, and the offered skills; no secret; a miss/expiry
  is the one indistinguishable `NotFound`) and **`confirm_external_identity`** (the OIDC callback's
  in-Authority half — set a live session's `confirmed_principal` + status `confirmed` from an
  already-proven email, the email parsed INSIDE the op; `complete_passcode`'s confirm minus the code check).
  Driven in-process by the device-flow→grant→redeem happy path, the possession-proof teeth (a leaked grant on
  a different key ⇒ DENIED), deterministic redeem idempotency, the cloud roster gate, self-host SMTP-free
  membership, instant revoke, the governance role matrix, server-derived device ids, the verification-context
  disclosure, and the external-identity confirm-then-grant — **no HTTP** (the verification-page HTML, the
  OIDC/magic-link transport + the mailer, and active read-token rotation land in `topos-plane`). Test-fixture
  shims gain `seed_workspace` / `seed_workspace_member`.

## Backend shape (Postgres-only)

`Authority` holds a concrete `db::Db` directly — no trait, no `sqlx::Any`, no dialect enum: SQLite was
removed, and Postgres is the single backend. The load-bearing invariant is that **no `sqlx` type ever
crosses the `db` module boundary**: every method there takes the id newtypes + data and returns plain domain
values, so the authority code above it is storage-shaped, never SQL-shaped.

## Planned (lands later)

The large-object store's **S3-compatible remote backend** (a second `LargeObjectStore` impl + a
`large-remote` `location` arm — a no-op extraction) and its **idempotent online backfill** (copy → verify →
flip `location` → `git repack`), both additive + client-invisible; **multi-reviewer governance**
(`min_approvers` / N-approver / reviewer roles / queues / a rendered diff UI — single-approver only today, no
role column; the client contribute loop + the proposals-listing read route that feeds it are now BUILT); the
**HTTP plane's still-to-come surfaces** over the issuance
core (the verification-page HTML and the audit outbox — the enrollment + governance request/response DTOs,
the mailer, and one generic OSS OIDC connector all landed in `topos-plane` earlier, and the workspace-policy
mutation route is now BUILT there as the admin-token `PUT …/policy/review-required`; the two remain
unbuilt); **active read-token
rotation** (redeem
mints non-expiring, device-bound read tokens today — `expires_at` is enforced but minted NULL, with per-device
revoke as the kill switch); domain-ownership **verification** (`verified_domain_status` is operator-asserted);
**at-rest key encryption / KMS** (the plane signing key + the enrollment secret are plaintext `0600` seeds for
now); the `purge` verb + force-unlink (the tombstones table + denylist check already exist); two-parent
author merges; per-skill encryption-at-rest.

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

The **pointer-move signer** adds, to **this crate only** (never the client — `check-arch` forbids the
`topos → plane-store` edge, so none of these reach the CLI): `ed25519-dalek` with `std` + `zeroize` (the
shared workspace pin stays `default-features = false` so `topos-core` keeps its verify-only `no_std` path; the
`zeroize` feature restores `SigningKey`'s zero-on-drop that the stripped default would lose), `zeroize`
(wiping the raw seed buffer around `from_bytes`), `getrandom` (the OS CSPRNG for first-run key generation),
`base64` (base64url-unpadded for the signed pointer's signature value), `uuid` (parsing the canonical op id
into the 16 bytes the device-op signature binds), and `serde_json` (serializing the signed-`current` record
DTO into the stored `BYTEA`). The plane private key lives **only** here; `topos-core` stays no-key verify-only.

The **enrollment issuance core** adds, to **this crate only** (likewise client-unreachable): `hmac` + `sha2`
(HMAC-SHA256 — the deterministic opaque-credential derivation over the `0600` enrollment secret, which reuses
the plane key's exact load-or-generate custody; `sha2`'s `Sha256` is the HMAC backend, the same
`default-features = false` 0.10 pin `topos-core` uses), reusing the already-present `getrandom` (fresh
device-code / user-code / passcode), `base64` (the credential codec), and `uuid` (the governance op id). The
enrollment secret never reaches the client.
