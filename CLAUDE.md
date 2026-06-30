# Topos — the OSS repo (the `topos` CLI + the self-hostable plane)

Topos is a layer for AI agents to share their **behaviors** within a team or organization — so every agent
stays current with company processes and everyone gets a consistent experience. A *behavior* (a "skill")
is a bundle of files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

**This repository is two programs in one Apache-2.0 Cargo workspace:**

- **`topos`** (`bins/topos`) — the local CLI an agent drives non-interactively to add, follow, publish, and
  update behaviors across harnesses (Claude Code, OpenClaw, Hermes).
- **`topos-plane`** (`bins/topos-plane`) — the self-hostable sharing server (a library + a thin binary).

They share one trust kernel (`topos-core`) — the single, auditable implementation of the byte-exact digest,
consent, signing, and sync algorithm. Nothing proprietary lives here.

> **Status — early scaffold (contracts frozen; trust kernel complete).** The boundary contracts are frozen and
> schema-generated (the `--json` envelope, the outcome/receipt/error/action-code shapes, the closed
> signature-alg + signed pointer, all 12 per-verb `data` payloads, and the four load-bearing client documents
> — sync/lock/map/op), with golden `--json` fixtures (pull/add/list/diff/log/publish) validated positive **and**
> negative against the schemas. The pure trust kernel (`topos-core`) implements the **byte-exact digest**, the
> **consent truth-table**, and the frozen **signing/commit byte-encodings** — the `commit_id` construction, the
> **Ed25519** device-op signature frame, and the JCS `current`-pointer preimage, all with verify and all behind
> known-answer vectors. The **local, accountless core** is built: the embedded-git sidecar (`topos-gitstore`
> over `gix`, with verify-on-read), the crash-safe document protocol, the bundle scanner, and the local verbs
> (`add`/`list`/`diff`/`log`/`pull`/`uninstall`). The **Claude Code harness adapter** is built too: discovery,
> adopt-in-place (track a skill where it sits, writing nothing into it), the idempotent content-blind
> session-start currency hook in `settings.json`, and a clean (skill-byte-preserving) uninstall. The
> **client pull/apply sync engine** is now built too: the `checkForUpdates → plan → apply` machine over the
> pure four-state currency transition (in `topos-core`), reading a signed `current` pointer through a
> source seam, authenticating its signature + workspace/skill scope, holding the **anti-rollback floor**
> (`observed` rises only on a verified strictly-higher record; a record at or below the floor is never
> auto-applied, and one naming a different commit than recorded raises a loud ALARM), snapshotting a local
> draft before any decision (never clobbered — a divergence is detected + surfaced, not merged), fetching +
> re-verifying the bytes (digest == tree == `commit_id`) and recording them durably (backfilling missing
> ancestors) before a **crash-safe, namespace-atomic byte-writing materialization** (a sibling staging dir
> → fsync → atomic directory swap → fsync parent → `map → lock → sync` commit, so a fault at any boundary
> leaves the placement holding old-or-new complete bytes and `applied` advances only after the swap; a
> crash-after-swap heals forward rather than showing a false divergence). `pull <skill>` accepts a pending
> update and `pull <skill>@<hash>` goes back to a version locally (a `held` pin that never lowers the
> floor). Consent stays the kernel's one `decide()` policy. The plane response + follow-state are
> **fixture-fed** in-process this increment (no HTTP, no `plane-store` edge — `check-arch` holds the line;
> production follows nothing, so the bare `pull` is an honest no-op). The **plane's storage + read authority**
> (`plane-store`) is now built behind its privacy boundary: per-workspace SQLite + git-object storage; the
> skill-scoped object-read access rule (rostered ∧ reachable, one indistinguishable not-found, never served by
> bare hash); full-tree upload with server rehash that records provenance + reachability only after an
> authoritative roster check; and the cross-skill lineage predicate — all directly tested against a real
> database + git store (it moves no pointer and signs nothing yet). The **DB-authoritative object-lifecycle /
> garbage-collection fence** over that store is built too: a GC-excluded upload **quarantine**;
> the fenced **`object_presence`** state machine (`present`/`deleting`/`absent`/`unavailable`) whose
> guarded compare-and-swaps make a `deleting` object non-resurrectable; **promotion leases** that root a
> commit's full object set before any byte migrates; **migrate** (lease-before-migrate, server-side
> dedup, durable install — now size-routing to git or the large-object store) recording a real version;
> **transactional mark-then-claim GC** (claim → unlink →
> finalize, the unlink outside any transaction, the keep-set exactly the read-authorization surface) with a
> recovery sweep + a quarantine janitor; and the **tombstones denylist** (no `purge` verb yet). The
> **size-routed large-object store** is now built on that fence: at migrate, a file blob ≥ a configurable
> ~1 MiB threshold is physically offloaded to a **per-workspace content-addressed side store** (the local
> filesystem, `object_presence.location = large-local`) keyed by the **same `blob_id`**, smaller blobs stay
> in git, and a ~100 MiB per-blob reject cap fails typed at ingest. Identity stays **placement-independent**
> (the same `version_id`/`bundle_digest` whichever store holds the bytes — **no pointer files**); reads
> (single-object and whole-bundle render) and the GC unlink **dispatch on `location`**, still through the
> skill-scoped access rule (404-not-403, never by bare hash); and there is **no cross-workspace dedup**. The
> database leads and the filesystem trails throughout. The **pointer-move write** (`set-current`: publish ·
> genesis · revert) that *moves* the `current` pointer this layer only created is now built too: **one
> `BEGIN IMMEDIATE` pure-DB transaction** (no filesystem op inside it) does receipt-replay → in-transaction
> authoritative device authz (a device-op signature against a **non-revoked** registered key bound to a
> **rostered** principal — a revoke committed first blocks the move) → a **compare-and-set on the whole
> `(epoch,seq)` pair** (CONFLICT carries the live generation; a restore that bumps `epoch` while reusing `seq`
> is caught) → object-availability + a lease-completion gate → same-skill lineage + the **first-parent
> assert** → provenance + reachability written **before** the pointer advance and the lease release (so a
> concurrent GC never has a window to reclaim the freshly-current bytes) → an **in-process Ed25519 signer**
> (the only private-key holder; load-or-generate `0600`) → a durable **all-outcome receipt** keyed
> `(workspace, device_key_id, op_id)` (a lost-ack retry replays it byte-for-byte). `revert` is a **forward**
> commit (`seq` advances; the pointer never moves backward); the **review-required typed-fail gate** fails a
> direct publish closed (`APPROVAL_REQUIRED`, ingesting nothing). It is exercised **in-process** (no HTTP, no
> client) by deterministic interleaving tests. The **author-side three-way (diff3) merge** that *resolves*
> a DIVERGED draft (the prior layer only detected + snapshotted + refused) is now built too: a pure kernel
> **policy** (file-set reconciliation over `(path, mode, content-id)` → a plan + an outcome, plus the
> presence-based **publish guard**), a `topos-gitstore` **execution** (`diffy`, pinned exact + golden — its
> conflict bytes are a consent artifact; non-UTF-8 is never line-merged; client-side size caps before
> allocation), and a client **resolution** reachable only through a `DivergedWitness` capability token (the
> structural author-only gate — followers never merge). A **clean** merge lands a **draft-on-current**
> (forward 1-parent commit on `current`; `applied = observed`); a **conflict** materializes the complete
> marker tree (both sides kept via `.topos-mine` sidecars where there are no merge bytes) plus a durable
> **`conflict.json`** that is both the publish-block fact and a **pre-swap recovery journal** (a crash
> mid-materialize re-renders the recorded result, never re-merging on-disk markers; a clean re-run always
> converges — proven by a fault-injection sweep). The disclosed **escape** (`pull <skill> --onto-current`)
> commits the author's bytes on `current` with a drop-diff (always available — no deadlock); unrelated
> histories fall back to a **2-way** manual choice, never a silent merge. The **contribute authority**
> (`publish --propose` · `review --approve | --reject`) — the *contribute* motion's server half — is now built
> on that same shared write: `propose` ingests + migrates a candidate like publish, then opens a `proposals`
> row and roots its bytes through a **gated `proposal_object`** root (NOT `commit_object`) **without moving
> `current` or signing** (`NEEDS_REVIEW`); a proposal's bytes are retained AND readable only while `open ∧
> base == current`, **one derived predicate shared verbatim by the read-authorization join and both GC-claim
> queries** — so keep-set == read surface holds across the eventless "stale" transition (no commit-parent
> table, no backfill, no reaper), with a read-time re-authorize guard so a reclaimed object reads **404, never
> `Integrity`**. `review --approve` uploads/leases nothing, runs the shared `(epoch,seq)` CAS (a stale base ⇒
> CONFLICT), enforces **four-eyes under `review_required`**, then reuses the SAME promote — whose edge-write
> is the **`proposal_object → commit_object` handoff** to the permanent trunk root — and flips the proposal
> `accepted` (sideways, signed); `review --reject`/withdraw is a standalone status-flip (nothing signed),
> after which GC reclaims the now-unrooted unique bytes. The legacy standalone `upload_candidate` path was
> **retired** — every write now goes through the shared ingest, so a `commit_object` edge means
> accepted-trunk by construction. Exercised **in-process** by the stale-approve + ABA interleavings (its HTTP
> write routes + per-route tests land with the plane below).
>
> The **HTTP plane** now exposes that authority over the network — the seam the two built halves meet at. The
> OSS `topos-plane` is a `router(state)` **library** (the single surface a downstream plane composes — **no
> extension/fork hook**) plus a thin `axum` bin; every handler is thin (parse the wire DTO → call the
> authority → serialize; **no trust decision, no raw object read, no client-asserted principal** in a
> handler). The frozen routes: the conditional-GET signed-`current` read (`ETag = "<epoch>.<seq>"`, a
> **commit-sensitive 304**), the skill-scoped object read and a version-metadata read (both **404-not-403**,
> never by bare hash, behind an opaque per-skill read credential resolved inside the authority), and the
> device-signed writes (`publish`/`proposals`/`reverts`/`reviews`). **Every terminal protocol outcome is an
> HTTP 200** carrying the canonical all-outcome receipt (a failure adds the flat wire error + `next_actions`;
> non-2xx is reserved for transport/auth/integrity faults), an `op_id` retry replays it byte-identically, a
> minimal **in-process rate limiter** freezes the 429 shape, and a generated **OpenAPI** lands under
> `contracts/openapi/` (drift-gated). The **client's real transport** is wired too: a blocking `ureq`
> `PlaneSource` conditional-GETs the signed pointer and reassembles a version (metadata + per-blob
> content-addressed GETs, **re-verifying each sha256**) to feed the **existing** pull engine — the read
> credential + the pinned plane key come from on-disk `instance.json`/`follows.json` (**fixture-seeded**; the
> enrollment that mints them lands later, so production still follows nothing and the bare `pull` stays an
> honest no-op). The whole distribute loop is proven **end-to-end over loopback HTTP** (a first pull
> fast-forwards byte-exact incl. the exec bit, a second is a 304 no-op, a tampered signature is refused with
> last-known-good retained). The client gains **no `plane-store`/`sqlx`/`tokio` edge** (`check-arch` holds the
> line), and the test-only seeding shims are feature-gated out of the production build (a check-arch guard
> proves it).
>
> Still to come: the large-object store's **S3-compatible remote backend + online backfill** (additive,
> client-invisible); the **client contribute loop** (the `publish --propose` / `review` / `diff` CLI verbs +
> the device-key signer, the plane-sourced diff, rebase orchestration — the server authority + its HTTP routes
> are built, the client UX + multi-reviewer governance are not); **enrollment** (device-flow / passcode /
> magic-link / OIDC, invite + read-credential minting) and `follow`/identity/roster/device issuance; the
> **governance mutation routes** (roster / policy); **TLS termination** at the plane (loopback HTTP today —
> terminate at a reverse proxy); the **audit outbox**; at-rest key encryption; the OpenClaw/Hermes adapters;
> and Postgres. `sqlx` is referenced by `plane-store` (and kept out of the client build — `check-arch` forbids
> that edge); `axum` now powers the OSS plane's HTTP server and `ureq` the client transport.
>
> **Keep this status honest (no stale docs).** This block — and the per-folder `CLAUDE.md` "Implemented /
> Planned" lists — are *living status*: update them in the **same change** that lands, removes, or alters what
> they describe. A `CLAUDE.md` that still calls landed work "planned" (or planned work "landed") is a bug, not
> just drift. The code is the source of truth; when this summary and the tree disagree, `cargo test` + the
> crate's own `CLAUDE.md` win — fix the prose to match.

## Progressive disclosure — read the CLAUDE.md in the folder you're working in

This file is the map; each folder carries its own `CLAUDE.md` with that unit's contract. Read it when you
enter the folder:

- `crates/` — the five library crates (the trust kernel + storage + the ports).
- `bins/` — the two programs (the CLI; the plane).
- `xtask/` — codegen + the schema drift gate.
- `contracts/` — the generated, committed cross-language contract (JSON-Schema + fixtures).
- `tests/` — the integration oracle stack.

`AGENTS.md` in each folder is a symlink to that folder's `CLAUDE.md` (for agents that read `AGENTS.md`).

## Build / test / lint

```sh
cargo build
cargo test
cargo run -p xtask -- gen-schema --check   # the schema drift gate (regenerate → assert no diff)
cargo fmt --all
cargo clippy --all-targets
```

Toolchain is pinned in `rust-toolchain.toml` (stable 1.96, edition 2024). `unsafe_code` is forbidden
workspace-wide; clippy `all` = warn.

## The crate graph (acyclic)

```
topos-types  ◄── the app libs + every fixture (the shared WIRE DTOs; NOT a dep of topos-core)
topos-core   the PURE trust kernel — no I/O, no traits, no clock/RNG. Owns digest, consent, the CAS
   ▲   ▲     decision, the sync transition, diff3, Ed25519 sign-preimage + verify. Tested in-crate.
   │   ├── topos-gitstore ──► topos-core, topos-types   (gix object mechanics; the large-object store)
   │   └── topos-harness  ──► topos-core, topos-types   (the one client-side port; 3 impls)
   │
plane-store  ──► topos-core, topos-types, topos-gitstore   (the server authority: private SQL + authz + txn)
topos-plane  ──► plane-store, topos-core, topos-types      (the OSS plane: lib + thin bin)
topos        ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx / libsqlite3-sys   ◄── architectural layering
```

## Principles that constrain this code

- **One trust implementation.** Every trust decision — digest, consent, the CAS decision, the sync
  transition, diff3, the signing-preimage — is written ONCE, in `topos-core`, the only crate with no I/O.
  The plane, the CLI, the fixtures, and the tests all link it, so no second implementation can drift.
- **The client is never an authority.** `bins/topos` takes no dependency on `plane-store`, `sqlx`, or a SQL
  driver — it is a thin sync tool. The dependency graph enforces this.
- **The plane is a library, composed — not a framework with holes.** `topos-plane`'s lib exposes clean
  authority operations + a `router(state)` builder; it has **no** extension/callback hook. (A separate
  private product imports and composes this library; this repo never imports it.)
- **Contracts are generated, never hand-written.** `contracts/schemas/*.json` are generated from
  `topos-types` by `xtask`. Change the Rust types, regenerate, review the diff. The drift gate must stay
  green.
- **Disclosure + integrity, not a second permission system.** The tool guarantees nothing lands that wasn't
  disclosed and pinned (the byte-exact bundle digest is what a human approves). How much a human sits in the
  loop is the agent/harness's job — never this tool's.
- **Simplicity-first.** No new primitives without a mainstream precedent (git, npm, signed links); reuse
  existing mechanisms.

## Conventions

- Match the surrounding code's idiom, comment density, and naming.
- Keep `topos-core` pure: no I/O, no `tokio`/`sqlx`/`axum`/`gix`/`std::fs`, no ambient clock or RNG (time is
  a `now` parameter; keys/signatures are byte parameters).
- `plane-store` keeps raw SQL + raw git reads private (`pub(crate)`); only authorized authority operations
  are public — that privacy boundary is what makes every object read go through the access check.

## License

Apache-2.0 — see `LICENSE`.
