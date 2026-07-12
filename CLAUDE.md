# Topos — the OSS repo (the `topos` CLI + the self-hostable plane + the web app)

Topos is a layer for AI agents to share their **behaviors** within a team or organization — so every agent
stays current with company processes and everyone gets a consistent experience. A *behavior* (a "skill")
is a bundle of files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

**This repository is three programs — two in one Apache-2.0 Cargo workspace, plus a TypeScript app:**

- **`topos`** (`bins/topos`) — the local CLI an agent drives non-interactively to add, follow, publish, and
  update behaviors across harnesses (Claude Code, OpenClaw, Hermes).
- **`topos-plane`** (`bins/topos-plane`) — the self-hostable sharing server (a library + a thin binary).
- **`@topos/web`** (`web/`) — the product web app (React Router, bun): sign-in, device approval, the
  workspace dashboard, the rendered review UI. Its own toolchain and gates; see `web/CLAUDE.md`.

The two Rust programs share one trust kernel (`topos-core`) — the single, auditable implementation of the
byte-exact digest, consent, content-addressed identity, and sync algorithm. Nothing proprietary lives here.

## Status — real but early (living status)

Both loops work **end-to-end today**, proven by loopback-HTTP e2e tests: **distribute** (an author
publishes; a follower's real two-call `follow` by workspace address arms the harness currency trigger and
every subsequent `update` lands the team's `current` byte-exact) and **contribute** (`publish --propose` → a four-eyes
`review --approve` → followers apply; `revert --to` rolls the team forward to older bytes) — plus a
self-hostable compose stack, a checksummed installer, and a tag-triggered release pipeline. Deferred,
honestly: TLS terminates at a reverse proxy by default (an EXPERIMENTAL, default-off built-in ACME
listener exists but is unproven on a real box) and the large-object store's S3-compatible remote backend.
The per-area detail lives in the owning `CLAUDE.md`s:

| Area | Current state (one line) | Detail |
|---|---|---|
| Wire + persisted contracts | Frozen + generated: JSON-Schema per wire type / verb payload / persisted doc (incl. the unsigned `WireCurrentRecord` pointer body + the adopted describe/apply payloads + the member-lane wire bodies), golden `--json` fixtures (validated positive **and** negative), the plane OpenAPI, and the generated CLI reference `docs/cli.md` (rendered from the real clap tree) — all drift-gated. | `contracts/CLAUDE.md` |
| Trust kernel | Complete + pure (no_std, no I/O): byte-exact digest + reject rules, consent truth-table, the content-addressed identity derivations (`commit_id` / `device_key_id` / `canonical_principal` — no keys, nothing signs), the four-state sync transition (no floor, no alarm — the served pointer is the sync target, re-verified by digest on apply), the author-merge policy. | `crates/topos-core/CLAUDE.md` |
| Git object layer + large objects | Built: verify-on-read object mechanics, diff/diff3 execution (pinned engines), the lifecycle-fence byte primitives, per-version durability batches, the size-routed local large-object store. | `crates/topos-gitstore/CLAUDE.md` |
| Harness adapters | The `HarnessAdapter` port + its three impls: the **Claude Code reference** (discover, adopt-in-place, idempotent `settings.json` session-start hook, clean uninstall) plus **OpenClaw** and **Hermes** (their concrete config bytes / per-turn-injection claims stay provisional behind the pilot readiness probes). | `crates/topos-harness/CLAUDE.md` |
| Server authority (`plane-store`) | Built, Postgres-only, split into **custody** (bytes/versions/pointers/GC) and **directory** (identity/policy) — custody consults access ONLY through the in-transaction **access-witness** trait the directory implements (a directory row-write is instantly effective against byte ops: revoke-blocks-promotion; the seam enforced by `check-arch`). **The workspace credential: ONE bearer membership credential per (workspace × device)** — minted deterministically at redeem/claim (HMAC `b"wscred"`), stored only as its sha256 ON the `device_registry` row (migration 0014, which also DROPS `read_token` + `enrollment_grant_skill` — a deliberate pre-1.0 clean break) — **authenticates every device-lane read AND write AND governance op** via `Authorization: Bearer`; authorization is the ONE membership predicate (a CONFIRMED `workspace_member` seat) on every lane (the per-skill `roster` table is GONE — 0015 lifted its interim follow-state into person-scoped `skill_follows`): membership-scoped reads (404-not-403, never by bare hash), the quarantine/lease/GC lifecycle fence + recovery/janitor, the SERIALIZABLE `(epoch,seq)` CAS pointer-move writing the **unsigned `WireCurrentRecord`** + all-outcome receipts (in-txn credential resolve FIRST — an unauthenticated caller mints nothing durable — then replay, then the revoked check, so a since-revoked device still replays its stored OK; **nothing signs**), propose → review (shared keep-set == read-surface predicate; four-eyes across lanes), enrollment issuance (deterministic HMAC credentials; the grant a bearer bound to the presented pubkey; redeem mints the ONE workspace credential — no per-skill tokens, no roster writes) + governance ops (resolve → request identity bound to the RESOLVED `device_key_id` under `TOPOS_DEVICE_GOVERNANCE_V1` → replay → revoked → role; a device revoke flips the row, a member removal deletes the seat every gate joins), the workspace-standup genesis (the one-time claim mint/redeem with lost-200 replay, `create_workspace`, the standup session + `approve_standup` — one shared genesis seat, per-identity capped), the web-session roster leg (privileged lib-level invite / remove / roster read — confirmed-owner acting gate, method-discriminated receipts, identical on both postures), one canonical principal form (parse-boundary ASCII fold + the migration that folded/deduped what predates it — one mailbox is one identity at every gate), and the web-session READ lane (five privileged lib-level member-scoped reads — skill index / current / metadata / bytes / proposals — on the gate/reach split, now the SAME membership gate the device lane runs, identical on a self-host plane and a hosted one (the mode no longer gates the session READ/roster/review/lifecycle legs — the product app serves self-hosted deployments through this session lane; the channel join/leave twins likewise call the same guarded functions the device lane does, and self-host runs the whole loop), every pre-gate miss one uniform not-found), and the web-session REVIEW leg (privileged lib-level approve/reject/**revert** on the shared pointer-move transaction — a confirmed owner\|reviewer acting gate; the remaining lane asymmetry is ROLE alone, deliberate for now: CLI review takes any confirmed member + four-eyes; finer role gating is later work; method/actor/request_sha256-discriminated lane-blind receipts via migration 0012, plus a proposer-disclosing proposal-detail read; the web one-click revert is a forward promote that bypasses the review gate, actor-parameterizing `set_current::revert` with a session-twin idempotency + a pre-stage owner\|reviewer fence), the **DEVICE-lane catalog read** (`list_skills_device` — credential resolve → confirmed-member, **no self-host denial**), and **the CHANNELS model** (migration 0015): the `catalog` (name→skill; `skill_id` stays the immutable custody key, so rename-on-archive touches only catalog rows), `channels` + references + person-scoped membership with a structural `everyone`, person-scoped follows/unfollows + per-device exclusions + person-scoped notices with read-state, EVERY policy write a guarded `topos_*` SQL function (curation member-on-open/reviewer-on-curated, protect tighten-reviewer/loosen-owner, the owner lifecycle: archive-renames-and-frees-the-name / unarchive / delete / version purge — proposals auto-close with author notices), the ONE entitlement predicate (`topos_entitled_skills`: everyone ∪ channels ∪ direct follows − unfollows − exclusions, active + current-holding only) serving `Authority::delivery` + the fleet's `report_applied` (a report is client-asserted, so every named skill is re-checked against the server's own entitlement predicate; member removal runs the lapse-detach reconcile, and the who-acts detach record is person-scoped + event-exact), and the protection gate REROUTING a member's direct publish/revert on a reviewed bundle into a proposal (NEEDS_REVIEW + `downgraded`; APPROVAL_REQUIRED deleted; four-eyes keys on the per-bundle cascade), and **the adopted verb surface** (migration 0016): a workspace `name` slug (`validate_workspace_name` + a reserved-name guard, backfilled from the display name) so joining and sharing speak ADDRESSES; **token-less** enrollment (the enroll flow resolves an address — no invite token; the `/i/`-link invite path deleted) + `redeem_login` (one credential per confirmed seat, deterministic per (grant, workspace); a revoked seat reports `blocked`); the row-op guarded fns (`topos_invite` as a never-demote roster write, `topos_notices_ack`, `topos_set_staleness_window`, `topos_set_invite_policy`); proposal supersede / author-withdraw / a REQUIRED reject reason on both lanes; a purged-target revert refusal; and the delivery's open-proposal count folded from N+1 into ONE aggregate (predicate-equivalence tested). | `crates/plane-store/CLAUDE.md` |
| HTTP plane (lib + thin bin) | Built: composable `router(state)`, the frozen read/write routes — every device-lane route authenticated by the ONE workspace credential in the `Authorization: Bearer` header (bodies carry no credential material; the `current` read is `GET /v1/workspaces/{ws}/skills/{skill}/current`, the token-in-path shape retired) — (200-for-all-outcomes writes, commit-sensitive 304 reads), enrollment + governance routes (+ default-off OIDC; `device/authorize` intent-dispatches enroll / standup / **login**, and joining is now by workspace ADDRESS — `/i/` serves admin CLAIM links only; the token-less enroll body carries an optional `workspace` address, and a constant **protocol card** answers any unmatched path, content-negotiated: JSON `WireProtocolCard` for a client, a markdown agent-instruction document for everything else; minted links ride the new `link_base_url`), `POST /v1/login` (re-mints one workspace credential per confirmed seat), the admin-token policy route, in-process rate limiter, the maintenance scheduler (`spawn_maintenance`), request tracing, the backup-restore epoch bump (`restore-bump-epoch`) + the `mint-claim` subcommand, the leak-free `PlaneConfig`/`PlaneState::open` composition seam + the lib-only standup wrappers (`mint_admin_claim`/`create_workspace`/`approve_standup`/`approve_session`), session-roster wrappers (`invite_members` — now returning the workspace address — / `remove_member` / `read_roster`; the join-link rotation retired), session-read wrappers (`list_skills_session`/`read_current_session`/`read_version_session`/`read_object_session`/`list_proposals_session` — pre-serialized `/v1`-parity wire bodies), and session-review wrappers (`review_approve_session`/`review_reject_session`/`read_proposal_session` — typed approve/reject/detail summaries), and the workspace-catalog route `GET /v1/workspaces/{ws}/skills` (`list_skills_device` → `WireSkillIndex`, now carrying catalog name + status), plus the **delivery read** `GET /v1/workspaces/{ws}/delivery` (`WireDelivery`: the entitled set + via attribution + the person's detached ids + the unacked notices feed + the proposal count; no-store) and the **fleet report** `PUT /v1/workspaces/{ws}/report` (`WireAppliedReport` → 204), the adopted **member-lane** routes (`POST /v1/workspaces/{ws}/invitations` — invitation as a roster write; `GET /me` · `GET /channels` · `GET /proposals` · `GET /skills/{skill}/log` · `GET /skills/{skill}/reach` describe reads; `POST …/notices/ack`; `PUT …/skills/{skill}/protection` and `PUT …/channels/{ch}/protection`), and `WireDelivery` now also carrying the workspace `staleness_window_ms`. | `bins/topos-plane/CLAUDE.md` |
| Client CLI (14 verbs + 2 maintenance groups) | Built: the accountless local core + crash-safe sidecar, the pull engine (anti-rollback, atomic dir-swap materialization, diff3 draft resolution, a fast-degrade circuit breaker), device key (keygen-only identity); `follow` enrolls by workspace ADDRESS (the constant protocol card re-roots onto the bootstrap-declared API base) and `invite` is a roster write; the **workspace-credential** model: enrollment mints ONE Bearer credential per (workspace × device) into `identity/credentials.json` (a `0600` secret) that authenticates EVERY plane request — reads AND writes AND governance; `follows.json` is pure subscription state (a first-run migration scrubs any legacy `read_token`); the write verbs with op-WAL idempotent retry, the workspace-standup client (an un-enrolled `publish` that stands the workspace up via the sign-in device flow + the one-shot `follow <claim-link>` bearer door), `INVALID_ARGUMENT`/io-kind-honest error codes, `TOPOS_DEBUG=1` + `~/.topos/log.jsonl` diagnostics, `list` **untracked discovery** across a baked ~72-harness registry (`--tracked` suppresses it) the credentialed **`--remote` catalog** read merged with local follow-state, and **the delivery-driven reconcile**: the bare enrolled sweep runs ONE delivery call per workspace and converges (install new arrivals under their catalog names through the kernel's I-TOFU offer; update against the pre-resolved target; the undelivered remainder splits by WHO ACTED — the person's detached set freezes in place, an upstream withdrawal snapshots any draft and cleans the agent dirs keeping every sidecar byte; a whole-workspace 404 freezes everything, never a clean), then reports the applied snapshot for the fleet page; `publish --to` places channel references; the ancestor backfill shallow-stops at purged history so fresh installs of live descendants survive a purge. **The adopted verb surface** runs ONE resolution grammar (addresses → qualified paths → bare-when-unique) + a two-phase describe/`--yes` consent over the built directory ops: `channel add\|remove`, `protect`, `remove` (a per-device exclusion), `unfollow` (the person-scoped detach), the `review` inbox + target describe (supersede / author-withdraw / a required reject reason), `invite` (a bare `/me` read, else the roster write), `publish`'s enrolled describe (gate outcome · reach · share line · undo), `update --reset`, `log`'s purge tombstones, `list`'s source/status/cause columns, and the `auth login\|logout\|status` group; notices ACK by id after an interactive `update` (the quiet hook fetches without acking); the sidecar records per-workspace `sync_status.json` freshness against the served `staleness_window_ms`; `keep-as-yours` re-forks a retained withdrawn copy; the whole surface is documented by the generated `docs/cli.md`. | `bins/topos/CLAUDE.md` |
| End-to-end proof | Loopback-HTTP suites green: the distribute HERO (table-driven across the Claude Code, OpenClaw, and Hermes adapters), the real `follow` enrollment, the contribute write verbs, the restore roll-forward (epoch-bump) suite, the workspace-standup full chain (both cloud doors + the self-host claim + the adversarial witnesses), and the **channels acceptance suite** (genesis→`everyone`→a fresh member installs; channel add/remove propagation with draft-snapshotting withdrawal; two channels one copy; leave keeps a still-referenced skill while unfollow freezes in place; the downgrade→approve→verdict-notice loop; the fleet report; archive; purge; a removed member frozen, never cleaned), and the **adopted-verb-surface acceptance suite** (address enrollment + the login re-mint; the two-phase describe/`--yes` verbs; unfollow cross-device + remove-no-return; the contribute loop with `-m`, a stale-base re-propose, and a required reject reason; the protocol card identical on every path; `invite` printing the address with no SMTP; `log` showing a purge tombstone). | `tests/CLAUDE.md` |
| Web app | Built (real but early): the product app on React Router 8 (bun) — sign-in (email+password, zero delivery dependency), the device-approval verification page, workspace create/index/dashboard, the skill pages (catalog-name-keyed, old names resolving via rename hints: current files, first-parent history, proposals), the full rendered review UI (diff plan + budgets + sanitized `@pierre/diffs` SSR, approve/reject/withdraw with four-eyes, comments, one-click revert), the `/i/` claim passthrough + `/install` — and the ADMIN surfaces: the roster page in full (invite via `topos_invite`; role change / remove / self-serve leave with the sole-owner lockouts), the skill lifecycle ceremonies (archive / unarchive / delete / purge / rename-with-redirect over the internal lane, id-keyed), channel existence-admin + the channel history page, the workspace policy page (review default · invite policy · staleness window), the fleet page (the ONE staleness clock; detached / removed-upstream / stale blind spots NAMED), the "your devices" self-service list, the resource addresses + the CONSTANT protocol card (no path echo, no existence oracle), and the first-run claim. Every admin ceremony is STEP-UP gated (stateless password re-entry via better-auth's own hasher; type-the-name on the destructive ones; every attempt lands an `admin_event` audit row). Four composition seams for a downstream superset build (`routes()` · nav registry · entitlements · auth config). Row reads = Drizzle SELECTs on schema `plane`; row writes = the guarded `topos_*` functions; byte/pointer/lifecycle ops = the plane's internal session lane. Gated by biome/tsc + the boundary/token/contract checks, 238 unit tests, and a 73-test Playwright e2e (fixture vault + real Postgres). NOT yet: the door cutover (the app does not front `/api/v1`; the plane's public listener stands), compose packaging. | `web/CLAUDE.md` |
| Gates + packaging | `cargo xtask ci` = the non-DB CI gates in order; `check-arch` enforces the layering, the leaf-crate leanness, the custody↛directory seam, the OIDC default-off claim, and the Dockerfile/toolchain pin pair; a stateless Docker image + compose + smoke script, the checksummed echo-then-match installer, and the tag-triggered release pipeline (`xtask dist`) ship the self-host. CI runs the Rust gates + suites and the web job (checks, vitest, Playwright). | `xtask/CLAUDE.md`, `README.md` |

**Still to come:** the large-object store's **S3-compatible remote backend + online backfill** (additive,
client-invisible); **SSO breadth** (managed multi-IdP / HRD / SAML / SCIM — one generic OIDC
connector ships feature-gated); **magic-link** as a primary rung; **in-place credential rotation** (the
workspace credential has NO expiry by decision — per-device revoke + re-enrollment IS the rotation; a
rotate-without-re-enroll op is later work if ever needed); the **credential-authenticated `PUT /policy`
variant** (the self-host admin-token route is built; a workspace-credential-authenticated governance
route over the same policy is later work — no kernel frame
needed); the built-in ACME TLS path's
**real-estate rehearsal** (public DNS · Let's Encrypt staging→prod · rate limits · renewal timing — the
experimental label stands and no one-command auto-TLS claim is made until it passes; a reverse proxy
remains the documented default); the **audit outbox**; at-rest encryption of the enrollment secret (a
plaintext `0600` seed for now); the two **pilot-build readiness probes**
(both sibling adapters are built — see above; OpenClaw's concrete config bytes and Hermes's per-turn
injection + consent flow stay provisional until each pilot's exact build is probed); and harness
*selection* in the client's composition root (v0 constructs Claude Code only; the TTY receipt copy
already branches on the report's `CurrencyKind`). The verification / workspace-create / review **web
pages** now live in this repo's `web/` app, which drives the lib-only session wrappers over the plane's
internal session lane; a hosted composition builds a superset of the same app rather than serving its
own pages (the Rust plane itself still ships no page HTML — the app is the page layer).

**Keep this status honest (no stale docs).** This table — and the per-folder `CLAUDE.md` "Implemented /
Planned" lists — are *living status*: update them in the **same change** that lands, removes, or alters what
they describe. A `CLAUDE.md` that still calls landed work "planned" (or planned work "landed") is a bug, not
just drift. The code is the source of truth; when this summary and the tree disagree, `cargo test` + the
crate's own `CLAUDE.md` win — fix the prose to match. Shipped-increment *narrative* belongs in the commit
history, never re-accreted here.

## Progressive disclosure — read the CLAUDE.md in the folder you're working in

This file is the map; each folder carries its own `CLAUDE.md` with that unit's contract. Read it when you
enter the folder:

- `crates/` — the five library crates (the trust kernel + storage + the ports).
- `bins/` — the two Rust programs (the CLI; the plane).
- `web/` — the product web app (TypeScript / React Router on bun; its own gates + suites).
- `xtask/` — codegen + the invariant gates (`ci`, `check-arch`, the drift gates).
- `contracts/` — the generated, committed cross-language contract (JSON-Schema + fixtures + OpenAPI).
- `tests/` — the workspace-level loopback-HTTP e2e suites.

`AGENTS.md` in each folder is a symlink to that folder's `CLAUDE.md` (for agents that read `AGENTS.md`).

## Build / test / lint

```sh
cargo build
cargo test           # requires a Postgres via DATABASE_URL (see below)
cargo xtask ci       # ALL the non-DB CI gates, in CI's order: fmt --check, clippy -D warnings,
                     # doc -D warnings, gen-schema --check, gen-fixtures --check, gen-cli-ref --check,
                     # check-arch
```

`cargo xtask ci` is the pre-push loop — one command that matches the CI `gate` job exactly (the `xtask`
alias lives in the committed `.cargo/config.toml`). The individual gates remain runnable one at a time —
see `xtask/CLAUDE.md`.

`cargo test` requires a Postgres reachable via `DATABASE_URL` — the suite provisions a fresh database per
test (`#[sqlx::test]`). Compilation itself is offline: the committed `.cargo/config.toml` defaults
`SQLX_OFFLINE=true` (non-forced — your own environment wins), so the compile-time-checked queries read the
committed `crates/plane-store/.sqlx` and `cargo build`, `clippy`, and `doc` need no database — only running
the tests does.

Toolchain is pinned in `rust-toolchain.toml` (stable 1.96, edition 2024). `unsafe_code` is forbidden
workspace-wide; clippy `all` = warn.

## The crate graph (acyclic)

```
topos-types  ◄── the app libs + every fixture (the shared WIRE DTOs; NOT a dep of topos-core)
topos-core   the PURE trust kernel — no I/O, no traits, no clock/RNG. Owns digest, consent, the sync
   ▲   ▲     transition, diff3 policy, the content-addressed identity derivations. Tested in-crate.
   │   ├── topos-gitstore ──► topos-core   (gix object mechanics; the large-object store)
   │   └── topos-harness  ──► topos-core, topos-types   (the one client-side port; the three harness impls)
   │
plane-store  ──► topos-core, topos-types, topos-gitstore   (the server authority: private SQL + authz + txn)
topos-plane  ──► plane-store, topos-core, topos-types      (the OSS plane: lib + thin bin)
topos        ──► topos-core, topos-types, topos-gitstore, topos-harness   (the CLI)
              └── NO edge to plane-store / sqlx   ◄── architectural layering
```

Heavy-dependency placement, enforced by `cargo xtask check-arch`: `sqlx` is referenced by `plane-store`
only (and kept out of the client build); `axum` powers the OSS plane's HTTP server, `ureq` the client's
blocking transport, and `lettre` the plane's passcode mailer. The OIDC stack (`oauth2`/`openidconnect`,
with their `reqwest`) is feature-gated **default-off** in `topos-plane` — a production-tree check asserts
a default build resolves none of it.

## Principles that constrain this code

- **One trust implementation.** Every trust decision — digest, consent, the sync transition, diff3, the
  content-addressed identity derivations — is written ONCE, in `topos-core`, the only crate with no I/O. The plane, the CLI,
  the fixtures, and the tests all link it, so no second implementation can drift. (Named exception, for
  now: the `(epoch,seq)` compare-and-set *decision* lives in `plane-store`'s SQL — its kernel extraction
  is on `topos-core`'s own planned list.)
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
- Unit tests live inline (`#[cfg(test)] mod tests`); multi-file suites live in `src/tests/`.
- Keep `topos-core` pure: no I/O, no `tokio`/`sqlx`/`axum`/`gix`/`std::fs`, no ambient clock or RNG (time is
  a `now` parameter; keys/signatures are byte parameters).
- `plane-store` keeps raw SQL + raw git reads private (`pub(crate)`); only authorized authority operations
  are public — that privacy boundary is what makes every object read go through the access check.

## License

Apache-2.0 — see `LICENSE`.
