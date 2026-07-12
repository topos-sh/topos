# `tests/` — the workspace-level end-to-end suite

One workspace member (`topos-e2e`) holding the loopback-HTTP end-to-end tests: the GENUINE client engine
(the real `ureq` transport, the real pull/write verbs) against the GENUINE plane (`topos_plane::router`
over a real `plane-store::Authority`) on a real `127.0.0.1:0` socket. Per-crate unit + generative tests
live in their crates (every trust invariant is a unit/seeded-generative test in `topos-core`); this
directory is for what only a cross-crate loopback run can prove.

## Layout (what actually exists)

- **`src/lib.rs`** — an intentionally-empty anchor so the package is a real workspace member that
  `cargo test --workspace` discovers.
- **`tests/common/`** — the shared harness: per-test Postgres provisioning (`provision_pg` creates a
  uniquely-named database on `$DATABASE_URL` and runs the production migrations) plus the loopback-plane
  scaffold every suite stands on — `Scratch` / `Plane` / `start_plane` (bind-first, optional enrollment
  config, then serve `router(state)`; `start_plane_mode` picks the deployment posture, and the `Plane`
  keeps its per-test pool for row-level witnesses), the shared seeding helpers (`seed_member` — register a
  device WITH its workspace Bearer credential + seat its principal as a confirmed member, the whole
  authorization under the credential model; `seed_genesis_plane`; `invite_member` — the REAL invitation op
  (`Authority::invite`, the member-lane roster write the reshaped `invite` verb drives; an invitation IS a
  roster row, there is no link); `ws_address`/`WS_NAME` — the workspace ADDRESS a `follow` targets; and
  `begin_address_enroll` — drive the real client `follow <address>` call 1 over the wire and complete the
  identity leg in-process via `confirm_external_identity`), and the placement-expectation builders. Each
  suite keeps only its scenario-specific seeding (a seed closure handed to `start_plane`). Each e2e runs a
  blocking `ureq` client on a plain thread beside a live `axum` server on a self-owned **multi-thread**
  runtime — which is why these tests cannot use `#[sqlx::test]` (its current-thread runtime would deadlock).
- **`tests/hero.rs`** — the distribute HERO: the real pull engine over loopback HTTP. First pull
  fast-forwards byte-exact (incl. the executable bit); a second is a commit-sensitive 304 no-op; a forward
  move to v2 (an ordinary UNSIGNED advanced record) applies byte-exact on the next pull — no signature, no
  client-side verification, only the served record + the content-addressed digest re-check on apply.
- **`tests/hero_claude.rs`** — the HERO on the REAL Claude Code adapter, on real client verbs: an author
  genesis-publishes over the wire; a follower's real `follow <address>` (enroll + `--yes` apply) arms the
  actual `settings.json` SessionStart hook (asserted byte-exact) into a temp stand-in `$CLAUDE_CONFIG_DIR`
  and lands the `everyone` genesis; update / `revert --to` land on subsequent bare sweeps; a drafting
  confirm-each follower is never clobbered (the `--manual` intent is re-recorded through the doc-level
  `set_follow_mode` bridge until the reconcile threads the WAL mode into first-receive installs — the
  sweeps themselves run the genuine engine). The suite's skill id is slug-clean (`s-deploy`) so the
  catalog name the plane mints equals the id and every adapter's placement dir reads back uniformly.
  Table-driven so a sibling harness adapter is one case row + one test. Its module doc states the honest
  ceiling: hook-installed + bytes-materialized is asserted; that a live session's hook output reaches
  model context is a documented manual MUST-VERIFY.
- **`tests/follow_e2e.rs`** — the real `topos follow <address>` enrollment loop (invite links are dead;
  the invitation op seats the roster row and the ADDRESS is the door): the constant protocol card is
  fetched over the real socket and asserted on BOTH faces (JSON carries `api_base_url`; markdown is the
  constant agent hand-off, no path echo) → device authorize toward the address NAME → the identity leg
  in-process (`confirm_external_identity`) → resume redeems over the wire (the grant is the bearer
  credential; the server checks the redeem body's device public key against the grant's bound key — a
  binding check, nothing signed) → the two-phase DESCRIBE (installs + via attribution + the all-devices /
  fleet-reporting disclosures) → `--yes` lands the `everyone` genesis byte-exact. An off-roster identity
  is the ONE uniform denial (`REQUEST_ACCESS` — indistinguishable from an unresolved address). Plus the
  hosted main-domain shape on one listener (`start_plane_split`): the card + address ride
  `http://localhost:<port>`, the API stays `http://127.0.0.1:<port>` — the JSON card on the web origin
  declares the API base and the client re-roots; only the card GET touches the web host.
- **`tests/contribute_e2e.rs`** — the client write verbs (`publish` / `review` / `revert` / the
  plane-sourced `diff`) over loopback HTTP — the acting device rides the request's workspace **Bearer
  credential** (never a body field; the op kind rides the route, nothing is signed; the plane resolves the
  credential to its registry row), with a separate follower receiving the shipped bytes byte-exact; covers
  op-id idempotent retry and the four-eyes / review flow (a DISTINCT confirmed-member reviewer credential).
- **`tests/catalog_e2e.rs`** — `list --remote` end to end: the client's device-credential catalog transport
  (the workspace **Bearer credential** from `credentials.json`, no signature — each reading rig `enroll`s to
  land it) against the plane's `GET /v1/workspaces/{ws}/skills` route — the happy-path round-trip (both
  skills' exact ids/digests), the real `list --remote` merge (Following / Available), the confirmed-member
  gate (a credential resolving to a non-member device AND a credential on a revoked device both 404 → empty,
  where a bad signature used to be the denial vector), and the self-host lane (catalog visibility ==
  membership on both cloud and self-host).
- **`tests/channels_e2e.rs`** — the DELIVERY-DRIVEN RECONCILE end to end: the real `ops::pull_reconcile`
  over the real `ureq` `DeliverySource` (`GET …/delivery` + `PUT …/report` under the workspace **Bearer
  credential**), driven through the `ReconcileHarness`. A genesis lands in the structural `everyone`
  (row-witnessed on `plane.pool`) and a fresh member INSTALLS it as a first-receive OFFER, then `accept`s
  it byte-exact; a channel placement installs and its removal WITHDRAWS (agent dir cleaned, sidecar +
  snapshotted draft retained, the subscription itself untouched — a withdrawal is a delivery change, not
  a subscription change, so a re-place re-delivers); two joined channels deliver ONE copy; leaving a
  channel keeps a still-referenced skill live while an `unfollow` DETACHES it (frozen in place, bytes
  intact); the `remove` verb's halves (server exclusion + local freeze) no-op on the next sweep and a
  `follow` lifts them; a member's direct publish DOWNGRADES to a proposal, a reviewer approves, and the
  follower lands v2 while the author collects a verdict notice; the reconcile REPORTS applied state to the
  fleet (`device_skill_state` + `last_report_at`); an archive withdraws the follower, frees the name, and
  auto-closes the open proposal with an author notice; a fresh follower installs v2 over a PURGED v1
  ancestor (the backfill shallow-stops); and a removed member gets `ACCESS_GONE` with every placement
  INTACT (never a clean), resuming when re-added. A brand-new arrival installs on ONE sweep: the reconcile
  binds each delivered skill to its workspace credential before any fetch (the per-skill credential map is
  derived from `follows.json`, which cannot yet name a skill this device has never held), and
  `install_then_offer` pins the clean, warning-free first sweep.
- **`tests/multi_workspace_e2e.rs`** — one install, one plane, TWO workspaces: a single `follow <address>`
  twice into the same sidecar (each an address enroll + `--yes` apply), both memberships retained, every
  verb scoped to the right workspace (an authoring `publish` moves only its skill's OWN workspace; `invite
  --workspace` seats the roster row in the named one — witnessed as an `invited` `workspace_member` row,
  the invitation being a roster write now), and same-name `--workspace` disambiguation on the publish path
  (two workspaces publish under the same catalog name; the reconcile declines the second same-named local
  copy by design, so the bare-ambiguity error path is web/`--prefix-dirname` territory, not this suite's).
- **`tests/restore_e2e.rs`** — the backup/restore rehearsal: a SQL-rewind "restore" over the real loopback
  plane. With the operator `restore-bump-epoch` helper (which REWRITES `current` one epoch forward —
  nothing re-signed; the bump report carries no key id), the follower silently rolls forward onto the
  restored older bytes with NO error and a subsequent publish proceeds at the bumped epoch; without the
  helper, the plane serves the restored pointer at its original LOWER generation and the follower silently
  rolls BACKWARD onto it — there is no anti-rollback ALARM anymore, a server restore is a team rollback the
  client applies toward whatever is served, backward included.
- **`tests/standup_e2e.rs`** — the workspace-standup full chain (the self-serve genesis release-blocker
  proof): **door 1** (an un-enrolled `publish` goes PENDING → the authority's `approve_standup` web leg →
  the SAME re-invoked publish enrolls + lands the genesis in one invocation, with ZERO operator ops —
  the `admin_claim` table stays empty — the receipt disclosing the workspace ADDRESS rooted on the minted
  `workspace.name` slug, and a follower pulling the object back byte-exact), **door 2**
  (`create_workspace` — returning the ADDRESS, replayed byte-identically — → the owner's `follow
  <address>` through the web-approve leg → genesis publish → a real `invite` (a roster write returning
  the address) → the member's address join flips `invited → confirmed` and the `--yes` apply lands the
  bytes exactly), the **self-host claim chain** (`mint_admin_claim` → the ONE-invocation `follow
  <claim-link>` → publish → invite → the member's address join through the SAME roster+identity gate as
  cloud → byte-exact placement — the born-confirmed bearer shortcut died with the invite token), and the
  adversarial witnesses: the off-roster leaked ADDRESS (the client's REQUEST_ACCESS ask-an-owner
  envelope), uniform approve-standup misses + idempotent double-approve (ONE workspace), the 4th-create
  cap, the reserved-address-name refusal at `create_workspace`, the standup-session intent guard (refused
  identity legs consume nothing), same-device claim replay vs different-device denial, and claim expiry
  (+ `/i/` NotFound). (The invite-token cross-species witnesses retired with the token itself; the claim
  door's own witnesses stand.)
- **`tests/revocation_e2e.rs`** — the credential-model revocation story: a follower joins by ADDRESS (the
  real `follow` + `--yes`) and lands v1; the owner **removes** the member (`Authority::roster_remove` under
  the owner's Bearer credential — the device-lane op the `DELETE …/roster/{email}` route composes); the
  owner ships v2 and the removed follower's next `pull` **fails closed** (the plane's uniform 404 for a
  non-member read maps to a silent no-op — nothing observed/fetched/applied, frozen at v1); the owner
  re-invites through the REAL invitation op, the follower re-runs `follow <address>` (its existing device
  re-redeems, the seat flips invited → confirmed and the workspace credential ROTATES — a
  `device_registry.credential_sha256` row-witness proves the old one is dead); the follower's next `pull`
  recovers to v2 byte-exact.
- **`tests/verb_reshape_e2e.rs`** — the reshaped verb surface's ACCEPTANCE suite: twelve scenarios, each
  a test, on the real client verbs over loopback HTTP, with row witnesses on `plane.pool` where the wire
  does not disclose (detachments, exclusions, supersede closures, notices ack state). (1) a fresh machine
  pastes the address and `everyone` lands; (2) `follow <ws>/channels/<name>` enrolls + joins + lands the
  channel set in one flow (the `channel_members` row witnessed); (3) `unfollow` on device A detaches
  device B too (person-scoped; the `skill_detachments` cause row; bytes frozen on BOTH); (4) `remove`
  excludes ONE device (the `device_exclusions` row; the other device keeps receiving; nothing returns on
  the next update) and `follow` lifts the exclusion (delivery resumes — the byte re-materialization after
  an exclusion is a NAMED client gap: `remove` skips the baseline reset `withdraw_upstream` documents as
  required, so the placement re-lands only after that lands); (5) the contribute loop — a member
  `publish -m` on a reviewed bundle DOWNGRADES to a proposal, the reviewer's bare `review` leads with the
  message, an approve after the base moved is refused AT THE READ SURFACE (a stale candidate leaves the
  shared `open ∧ base == current` predicate — the CAS CONFLICT is plane-store's in-crate proof) with the
  inbox raising `stale`, the re-propose SUPERSEDES the old proposal (`resolved_reason = 'superseded'`
  row-witnessed), the approve lands, and the author's next update narrates + ACKS the verdict notice
  (acked-at row-witnessed; a reject's `-m` reason rides into the notice verbatim); (6) `publish -m`
  messages show in the route-backed skill log, a purge leaves a who/when tombstone, a revert to the
  purged target is refused (the purged bytes left the read surface; the pointer never moves), and an
  archive frees the base name (`base_name`/successor facts); (7) a multi-`--skill` follow resolves
  ALL-OR-NONE (one bad name ⇒ zero `skill_follows` rows); (8) the protocol card is byte-identical on
  three different paths and its JSON face carries `api_base_url`; (9) `invite` on a relay-less plane
  answers the ADDRESS with the honest `mailed: false`, seats the invitee, and the invitee joins by
  address (the `mailed: true` half is the plane's in-crate mailer tests' — the capturing test mailer is
  deliberately crate-private); (10) `protect` tighten as reviewer works (catalog row witnessed), loosen
  as reviewer is refused NAMING the owner, and the describe carries the audience (reach); (11) ONE login
  session (one identity leg) mints credentials for TWO workspaces, `auth logout` keeps every byte, and
  `auth status` reports the per-workspace cause (`no credential` when signed out); (12) the hook posture
  — a removed member's `update --quiet` is exit-0 with the ONE freeze line (bytes intact), and an
  unreachable plane past the backdated staleness window warns "last synced <age> ago — server
  unreachable".

## Running it

The suite **requires a Postgres** reachable via `DATABASE_URL` (each test provisions its own fresh
database; provisioned databases are left behind — point it at a disposable server/container). Keep
`SQLX_OFFLINE=true` for compilation (the committed `.cargo/config.toml` defaults it).

```sh
export DATABASE_URL="postgres://topos:topos@localhost:5432/topos"
cargo test -p topos-e2e
```

Both sides are driven through their feature-gated `test-fixtures` surfaces (the client's `test_support`,
plane-store's seed shims) — dev-dependencies of this test-only member, never enabled in a production build
(`cargo xtask check-arch` asserts it).
