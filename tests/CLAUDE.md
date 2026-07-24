# `tests/` — the workspace-level end-to-end suite

One workspace member (`topos-e2e`) holding the composed-stack end-to-end tests: the GENUINE client
engine (the real `ureq` transports, the real verbs through `topos::test_support::SessionInstall`)
against the GENUINE product topology. That topology is: the REAL web app — spawned from its
production build (`web/build/server/index.js`; CI builds it before `cargo test`, locally run
`cd web && bun install && bun run build` once) — serving the WHOLE public surface (the pages, the
resource addresses/protocol card, and the `/api/v1` session lane over its own `web` schema), in
front of an in-process vault (`topos_plane::router` — pure byte custody, the bearer-gated
`/internal/v1` lane, no public face). Identity is ONE `user.id`: the harness claims the boot-minted
workspace, signs people in with cookie sessions, and approves every CLI login at the real `/verify`
ceremony (a plain signed-in accept) — the same HTTP a browser would send. SMTP stays UNSET (the
whole logged-in loop must work with zero mail delivery). Per-crate unit + generative tests live in
their crates — the manifest-layer resolution, the reference grammar, and the reconcile's per-kind
arms are covered by `bins/topos`'s own suites (`src/tests/manifest_reconcile.rs` and friends) over
fakes; this directory is for what only a cross-crate composed run can prove.

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
    document request to run first-boot setup;
  - **HTTP ceremonies** (`Session` — a manual-cookie-jar `ureq` browser stand-in): `claim_owner`,
    `sign_in`/`sign_up` (better-auth's own REST rungs, Origin header included),
    `approve_device`/`deny_device` (the `/verify` action — a plain signed-in accept),
    `login_begin_and_approve`/`login_complete` (the CLI's `topos login` call 1 + the human
    approval + the resumed grant), and `mint_session`/`mint_session_in` (a probe session over the
    real login flow, for wire-level lane calls the CLI has no verb for — born per the one rule:
    active, or pending under the workspace's `session_approval` knob);
  - **the raw session lane** (`device_get`/`device_put`/`device_delete`/`device_post_json` —
    Bearer requests against `<origin>/api`) and **row-level witnesses** (the superuser pool:
    `count` / `text_witness` / `user_id`);
  - **the named mail-less arrangement helpers** (`open_registration`, `set_session_approval`,
    `seat` / `seat_in`, `add_member`, `add_workspace`) — direct rows + an audit note for exactly
    the steps whose OSS surface is the invitation mailbox rung. Everything else goes through the
    product surfaces.

  Each e2e runs a blocking `ureq` client on the test thread beside servers on a self-owned
  **multi-thread** runtime — which is why these tests cannot use `#[sqlx::test]` (its
  current-thread runtime would deadlock). Provisioned databases are left behind (the CI/local
  Postgres is disposable; dropping a database under a live pool is racy).

- **`tests/session_manifest_e2e.rs`** — the SESSION + MANIFEST hero loop, plus the deny/logout
  arms:
  - the hero: the author's `login` pends → the owner approves at `/verify` → the resumed grant
    persists the ACTIVE session; adopting a local dir records the `./deploy` manifest line and the
    landed genesis publish runs the **governance transfer** (the line rewritten to the canonical
    workspace reference, disclosed on the receipt); a second person's `add @acme/deploy` in a git
    checkout writes the project manifest AND delivers in the same invocation — byte-exact
    (executable bit kept) into `<proj>/.claude/skills/`, git-excluded via `.git/info/exclude`
    (idempotent); v2 fast-forwards silently on the next sweep (login was the acceptance — no offer
    step); `protect` tightens to `reviewed` and the member's next publish DOWNGRADES to a proposal
    the owner approves (the follower lands it); the `-g` profile lane delivers person-scope and
    removes cleanly; the OWNER's remove-session arm (the real `/settings/sessions` POST) ends the
    lane — the member's next sweep prints ONE typed `SESSION_ENDED` line, marks the local row
    ended, freezes the bytes in place, and stays quiet on the sweep after;
  - `deny` answers the resumed login with one typed refusal, sweeps the WAL, mints nothing;
    `logout --all` ends the session server-side (row-witnessed) and deletes the local rows.
- **`tests/uniform_e2e.rs`** — the NON-ORACLE discipline over real HTTP:
  - eight misses — a real-but-foreign workspace, a never-existed one, a wrong path, a garbage
    credential; reads, the me describe, and a profile row-op write alike — answer ONE
    byte-identical uniform 404, and the prober's own lane is untouched;
  - the `session_approval` knob: a member's login is born PENDING (the granted poll says so);
    exactly TWO routes answer typed (`/me` and `/delivery` — `session_status: "pending"`, the
    delivery shape-complete and EMPTY) while every other route stays the uniform 404
    byte-identical to a garbage credential's; the owner's approve arm on the sessions page opens
    the same lane.

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

The client side is driven through its feature-gated `test-fixtures` surface
(`topos::test_support::SessionInstall`); the vault side through `plane-store`'s `test-fixtures`
(the pool-injection constructor + the migrator) — dev-dependencies of this test-only member, never
enabled in a production build (`cargo xtask check-arch` asserts it).
