# `tests/` — the workspace-level end-to-end suite

One workspace member (`topos-e2e`) holding the composed-stack end-to-end tests: the GENUINE client
engine (the real `ureq` transports, the real verbs) against the GENUINE product topology. Since the
identity unification that topology is: the REAL web app — spawned from its production build
(`web/build/server/index.js`; CI builds it before `cargo test`, locally run
`cd web && bun install && bun run build` once) — serving the WHOLE public surface (the pages, the
resource addresses/protocol card, and the `/api/v1` device lane over its own `web` schema), in front
of an in-process vault (`topos_plane::router` — pure byte custody, the bearer-gated `/internal/v1`
lane, no public face). Identity is ONE `user.id`: the harness claims the boot-minted workspace,
signs people in with cookie sessions, and approves every device flow at the real `/verify` ceremony
(a plain signed-in accept — no step-up) — the same HTTP a browser would send. SMTP stays UNSET in every suite:
the whole enrolled loop must work with zero mail delivery. Per-crate unit + generative tests live in
their crates; this directory is for what only a cross-crate composed run can prove.

## Layout (what actually exists)

- **`src/lib.rs`** — an intentionally-empty anchor so the package is a real workspace member that
  `cargo test --workspace` discovers.
- **`tests/common/`** — the shared harness:
  - **per-test Postgres by the PRODUCTION recipe** (`provision_pg`): a uniquely-named database on
    `$DATABASE_URL`, the two application roles (`topos_plane`/`topos_web`), the two schemas each
    owned by its role, the role-level search_paths, the ALTER DEFAULT PRIVILEGES chain (mirroring
    `scripts/compose-init-db.sh`), then BOTH migration lineages — the vault's sqlx migrations AS
    `topos_plane`, the app's drizzle lineage AS `topos_web` via the app's own
    `web/scripts/migrate.mjs` (needs `node` on PATH);
  - **the composed stack** (`Stack` / `start_stack`): the in-process vault served on a loopback
    port with the internal token armed, the web app spawned in front of it (`TOPOS_SETUP_CODE`
    preset, `TOPOS_WORKSPACE_NAME = "acme"`, `APP_ENV=test`, rate belt off, SMTP unset), and one
    document request to run first-boot setup (the boot-minted workspace + the printed claim link,
    mirrored to `TOPOS_SETUP_LINK_FILE`);
  - **HTTP ceremonies** (`Session` — a manual-cookie-jar `ureq` browser stand-in): `claim_owner`
    (GET+POST `/claim` with the preset code → the signed-in first owner), `sign_in`/`sign_up`
    (better-auth's own REST rungs, Origin header included), `approve_device`/`deny_device` (the
    `/verify` action — a plain signed-in accept, no step-up field), `enroll_begin_and_approve` (the CLI's
    `follow` call 1 + the human approval — the caller resumes), and `mint_device` (a probe
    credential over the real device flow, for wire-level lane calls the CLI has no verb for);
  - **the raw device lane** (`device_get`/`device_put`/`device_delete`/`device_post_json` —
    Bearer requests against `<origin>/api`) and **row-level witnesses** (the superuser pool:
    `count` / `text_witness` / `user_id`);
  - **the named mail-less arrangement helpers** (`open_registration`, `seat`, `add_member`) —
    direct rows + an audit note for exactly the steps whose OSS surface is the invitation mailbox
    rung (the SMTP-unset suites run without it; the web Playwright mail-sink spec drives that rung
    for real). Everything else goes through the product surfaces.

  Each e2e runs a blocking `ureq` client on the test thread beside servers on a self-owned
  **multi-thread** runtime — which is why these tests cannot use `#[sqlx::test]` (its
  current-thread runtime would deadlock). Provisioned databases are left behind (the CI/local
  Postgres is disposable; dropping a database under a live pool is racy).

- **`tests/hero.rs`** — the distribute HERO, one test: boot → claim → the authoring CLI enrolls
  through the device flow → genesis publish → a second device (same person) follows and lands the
  bytes byte-exact (incl. the executable bit) → v2 fast-forwards on the next sweep (a repeat sweep
  is a commit-sensitive no-op; the fleet report row is witnessed) → a revert restores the v1 bytes
  as a NEW forward version (generation moves FORWARD) the follower lands.
- **`tests/hero_claude.rs`** — the HERO on the REAL adapters, table-driven (Claude Code / OpenClaw
  / Hermes over temp stand-in homes): the enrollment promote arms each adapter's genuine auto-update
  surface (`settings.json` SessionStart / OpenClaw's silent auto-update cron via the rig's fake CLI /
  `config.yaml` `on_session_start` + `on_session_reset`), the genesis lands byte-exact in the
  adapter's own skill dir, and
  v2 lands on the next bare sweep. The honest ceiling: hook-installed + bytes-materialized is
  asserted; that a live session's hook output reaches model context is a manual MUST-VERIFY.
- **`tests/follow_e2e.rs`** — the real `topos follow <address>` loop: the constant protocol card on
  BOTH faces (JSON declares `api_base_url` = the origin's `/api` mount; markdown is the constant
  agent hand-off) and byte-identical across the address / the origin root / an unmatched path; call
  1 pends (user code + the `0600` WAL) → the signed-in member approves at `/verify` → the resumed
  follow persists the ONE `0600` bearer credential and continues into the two-phase DESCRIBE
  (role, installs with consent digests, the `via everyone` attribution, the all-devices +
  fleet-reporting disclosures) → `--yes` lands `everyone`'s genesis byte-exact. The TARGET-SCOPED
  consent regression: with a waiting `everyone` arrival never received on the member's device, the
  targeted channel/skill describes list ONLY their named target's set and the targeted `--yes`
  lands exactly that (no subscription state, no bytes for the un-named arrival — no member
  direct-follow row either), while a later targeted `--yes` on the arrival lands it byte-exact.
  A RESOURCE-address enrollment (`follow <origin>/<ws>/skills/<x>` and the `channels/<y>` form)
  completes through the same resume and lands ONLY its named target at enroll time — the
  `everyone` arrival stays uninstalled, individually consentable afterwards. The DENIED arm: the
  approver clicks Deny and the device's next poll is the ONE typed refusal (`DENIED` +
  `REQUEST_ACCESS`) with zero enrollment state; a wrong workspace name at the flow start is the
  uniform 404, byte-identical to a wrong path.
- **`tests/contribute_e2e.rs`** — the contribute loop: a member's genesis lands directly (no base
  to review), the owner tightens the bundle to `reviewed` (`protect`, row-witnessed), the member's
  next direct publish DOWNGRADES to a proposal (never an error), the reviewer's inbox leads with
  the author's message, the approve CAS-moves `current` and the follower lands the bytes, the
  author's next sweep narrates + ACKS the verdict notice (acked-at row-witnessed; a second sweep is
  silent), a reject's reason rides the notice verbatim, and a revert to the genesis still lands.
- **`tests/channels_e2e.rs`** — delivery as channel math: unplacing a skill from its last
  delivering channel WITHDRAWS it on the next sweep (agent dir cleaned, sidecar kept); a first
  placement CREATES the named channel; joining it re-delivers. The DEFAULT channel is leavable —
  the leave is a per-person `channel_optout` row (the copy DETACHES, bytes frozen in place) and the
  rejoin deletes it. `remove` is a per-device exclusion row (fenced to the acting credential's own
  device) that `follow` lifts; the fleet report row is witnessed. A CURATED `everyone` gates the
  genesis default placement too: a member's bare genesis SUCCEEDS catalog-only (no reference row,
  the withheld placement disclosed on the receipt) until a curator's real `channel add` places it
  and a second device lands it byte-exact; a member's explicit `--to everyone` answers exactly what
  a named curated channel answers.
- **`tests/revocation_e2e.rs`** — revocation: a self-revoke ends the lane IMMEDIATELY (the very
  next request under the dead credential 404s), is FINAL (the un-revoke UPDATE is refused by the
  DB trigger), and re-enrolling recovers; the CLI's `auth logout` best-effort-revokes its device
  and deletes the credential doc while every byte stays; a seat removal through the app's members
  ceremony (step-up) writes the detach records and ends delivery IN THE SAME REQUEST — the removed
  member's sweep fails CLOSED into a freeze (placements intact, the quiet hook exit-0 with its
  one-liner) and resumes when re-seated.
- **`tests/cross_workspace_e2e.rs`** — the cross-workspace refusal probe: with a second workspace
  row inserted directly, a credential seated in workspace A gets the uniform wire 404 on EVERY
  workspace-B route (reads and row-op writes), byte-identical to a workspace that never existed
  and to a wrong path — no oracle in any direction; the A lane is untouched.
- **`tests/claim_e2e.rs`** — the first-boot claim door: the printed link (the
  `TOPOS_SETUP_LINK_FILE` mirror) claims once — first account, first owner seat, signed in — and
  the consumed code is then the SAME uniform miss as a wrong code (GET and POST, byte-for-byte;
  the spent-code POST creates nothing). Registration: closed by default with ONE constant
  non-enumerating refusal (the login page carries the copy; the wire names no cause), a refused
  step-up cannot flip the knob, the REAL settings ceremony flips it (audit-rowed), and the
  admitted uninvited sign-up lands an ACCOUNT — never a seat.

## Running it

The suite **requires a Postgres** reachable via `DATABASE_URL` (each test provisions its own fresh
database; provisioned databases are left behind — point it at a disposable server/container),
**node on PATH** (the app's migrator + the spawned production build), and the web app **built
once** (`cd web && bun install && bun run build`). Keep `SQLX_OFFLINE=true` for compilation (the
committed `.cargo/config.toml` defaults it).

```sh
export DATABASE_URL="postgres://postgres:postgres@localhost:5432/postgres"
cargo test -p topos-e2e
```

The client side is driven through its feature-gated `test-fixtures` surface (`topos::test_support`);
the vault side through `plane-store`'s `test-fixtures` (the pool-injection constructor + the
migrator) — dev-dependencies of this test-only member, never enabled in a production build
(`cargo xtask check-arch` asserts it).
