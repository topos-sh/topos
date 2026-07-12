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
  authorization under the credential model; `seed_genesis_plane`; and `mint_invite`, whose acting
  `owner_credential` the plane resolves to its registry row → owner-role gate, no signature), and the
  placement-expectation builders. Each suite keeps only its
  scenario-specific seeding (a seed closure handed to `start_plane`). Each e2e runs a blocking
  `ureq` client on a plain thread beside a live `axum` server on a self-owned **multi-thread** runtime —
  which is why these tests cannot use `#[sqlx::test]` (its current-thread runtime would deadlock).
- **`tests/hero.rs`** — the distribute HERO: the real pull engine over loopback HTTP. First pull
  fast-forwards byte-exact (incl. the executable bit); a second is a commit-sensitive 304 no-op; a forward
  move to v2 (an ordinary UNSIGNED advanced record) applies byte-exact on the next pull — no signature, no
  client-side verification, only the served record + the content-addressed digest re-check on apply.
- **`tests/hero_claude.rs`** — the HERO on the REAL Claude Code adapter, on real client verbs: an author
  genesis-publishes over the wire; a follower's real two-call `follow` arms the actual `settings.json`
  SessionStart hook (asserted byte-exact) into a temp stand-in `$CLAUDE_CONFIG_DIR` and lands the bundle;
  update / `revert --to` land on subsequent bare `pull` sweeps; a drafting confirm-each follower is never
  clobbered. Table-driven so a sibling harness adapter is one case row + one test. Its module doc states
  the honest ceiling: hook-installed + bytes-materialized is asserted; that a live session's hook output
  reaches model context is a documented manual MUST-VERIFY.
- **`tests/follow_e2e.rs`** — the real `topos follow` enrollment loop: invite mint → bootstrap fetch (no
  trust root to pin — the pointer is unsigned) → device authorize → confirm → resume redeems over the wire
  (the grant is the bearer credential; the server checks the redeem body's device public key against the
  grant's bound key — a binding check, nothing signed) → the first-received bundle lands byte-exact; a
  leaked invite on an off-roster identity is denied. Plus the hosted main-domain-link shape on one listener
  (`start_plane_split`: links ride `http://localhost:<port>`, the API stays `http://127.0.0.1:<port>`): the
  minted link rides the public link base, a non-JSON fetch of the link serves the markdown
  agent-instruction document over the real socket, and the client re-roots — only the bootstrap GET touches
  the link host; the device flow and the placing pull ride the declared API base.
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
- **`tests/multi_workspace_e2e.rs`** — one install, one plane, TWO workspaces: a single `follow` twice into
  the same sidecar, both memberships retained, every verb scoped to the right workspace (an authoring
  `publish` moves only its skill's OWN workspace; `invite --workspace` mints into the named one), and
  same-name disambiguation via `--workspace`.
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
  the `admin_claim` table stays empty — and a follower pulling the object back byte-exact), **door 2**
  (`create_workspace` → the owner's follow through the web-approve leg → genesis publish → a real
  `invite` → the member's `invited → confirmed` redeem → byte-exact placement), the **self-host claim
  chain** (`mint_admin_claim` → the ONE-invocation `follow <claim-link>` → publish → invite → a second
  client's bearer redeem → byte-exact placement), and the adversarial witnesses: the off-roster leaked
  self-invite (the client's REQUEST_ACCESS ask-an-owner envelope), uniform approve-standup misses +
  idempotent double-approve (ONE workspace), the 4th-create cap, the standup-session intent guard
  (refused identity legs consume nothing), same-device claim replay vs different-device denial, claim
  expiry (+ `/i/` NotFound), and cross-species token isolation in both directions.
- **`tests/revocation_e2e.rs`** — the credential-model revocation story: a follower enrolls via the REAL
  two-call `follow` and pulls v1; the owner **removes** the member (`Authority::roster_remove` under the
  owner's Bearer credential — the device-lane op the `DELETE …/roster/{email}` route composes); the owner
  ships v2 and the removed follower's next `pull` **fails closed** (the plane's uniform 404 for a non-member
  read maps to a silent no-op — nothing observed/fetched/applied, frozen at v1); the owner re-invites, the
  follower re-runs `follow` (its existing device re-redeems, the seat flips invited → confirmed and the
  workspace credential ROTATES — a `device_registry.credential_sha256` row-witness proves the old one is
  dead); the follower's next `pull` recovers to v2 byte-exact.

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
