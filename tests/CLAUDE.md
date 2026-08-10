# `tests/` — the workspace-level end-to-end suite

One workspace member (`topos-e2e`) holding the composed-stack e2e tests: the GENUINE client engine
(real `ureq` transports, real verbs via `topos::test_support::SessionInstall`) against the GENUINE
product topology — the REAL web app spawned from its production build
(`web/build/server/index.js`) serving the whole public surface, in front of an in-process vault
(`topos_plane::router`, no public face). Identity is one `user.id`: the harness claims the
boot-minted workspace, signs people in with cookie sessions, approves CLI logins at the real
`/verify` ceremony. SMTP stays UNSET — the whole logged-in loop must work with zero mail. Unit +
generative coverage lives in the crates (the manifest grammar, editor inverse, per-scope
resolution, and reconcile arms are `bins/topos`'s own suites over fakes); this directory is for
what only a cross-crate composed run can prove.

## Layout

- **`src/lib.rs`** — an intentionally-empty anchor so `cargo test --workspace` discovers the
  package.
- **`tests/common/`** — the shared harness: per-test Postgres by the PRODUCTION recipe
  (`provision_pg` — two roles/schemas, both migration lineages, mirroring
  `scripts/compose-init-db.sh`; needs `node` on PATH for the app's migrator); the composed stack
  (`start_stack`); HTTP ceremonies (`Session`, a manual-cookie-jar `ureq` browser stand-in:
  claim / sign-in / the `/verify` approval / `mint_session`); the raw session lane
  (`device_get`/`device_put`/…); row-level witnesses (superuser pool); named mail-less
  arrangement helpers (`seat`, `add_workspace`, `open_registration`, `set_session_approval` —
  direct rows only for steps whose OSS surface is the invitation mailbox rung); and the real-CLI
  rig the subprocess-driving suites share (`cli_binary` / `run_cli` / `CliOut`).
- **`tests/session_manifest_e2e.rs`** — the SESSION + MANIFEST hero loop (login approval →
  governance-transferring genesis publish → project-manifest delivery → silent fast-forward →
  protect/review → the person-scope feed lane and its `-g` arms → the owner-side session end),
  plus the deny/logout arms.
- **`tests/mcp_e2e.rs`** — the `kind = "mcp"` bundle loop: `add --kind mcp` adopting a local
  `server.json` folder, the publish that makes it a catalog bundle (witnessed on the row, the
  delivery lane, and the workspace's registry-shape read API), a second member's sweep landing the
  entry in ALL SIX MCP-capable agents' configs in their exact dialects, the applied report's
  per-agent states reaching the workspace, removal converging every surface back to the person's
  own bytes, a hand-edited entry left DRIFTED and disclosed, project scope reaching the four
  project surfaces alone, and the two kinds coexisting under one skills root. Drives the REAL CLI
  BINARY as a subprocess over a fake `$HOME` — harness detection and the config surfaces resolve
  against the environment, so only a real process proves them (the fixture rig still owns the
  browser login; both halves share one `~/.topos`).
- **`tests/conflict_e2e.rs`** — the MERGE-CONFLICT loop, a state only a composed run reaches (the
  diverged apply class is reachable only through a workspace delivery): three bundles published,
  edited by a second person, republished over the SAME lines, then merged by an ordinary `topos
  update`. Asserts the property — no folder an agent reads holds a marker, every placement still
  holds that person's own bytes, the marked-up copy is in `~/.topos/conflicts/<name>/` carrying
  both sides, `publish` refused — and drives all three exits end to end (`--keep-mine` over an
  untouched workbench, over an edited one, and `--reset`), each clearing the block and deleting the
  workbench. Then the second property: `--keep-mine` is `git merge -X ours` — the author's v2 also
  changes a line nobody contested and adds a whole new file, and BOTH survive the exit, the publish,
  and the round trip back into the AUTHOR's own install, whose next update fast-forwards onto the
  resolution (their folder is rewritten from the published tree, which is what that hop proves —
  it is not a fresh teammate's first install). Publishing after it is an ordinary `--yes`. A last
  arm puts a copy behind (a go-back): the direct publish refuses, the `--propose` lands.
  Binary-driven too: the placement dirs and the workbench path resolve against the environment.
- **`tests/hardening_e2e.rs`** — pinned references delivering exactly their version and holding
  across sweeps; `publish --to` refusing nonexistent channels without minting one.
- **`tests/uniform_e2e.rs`** — the NON-ORACLE discipline over real HTTP: byte-identical uniform
  404s across foreign/never-existed workspaces, wrong paths, garbage credentials; the
  `session_approval` knob's born-pending lane (exactly two typed answers until approval).

## Running it

Requires a Postgres via `DATABASE_URL` (each test provisions its own database; provisioned DBs
are left behind — point at a disposable server), `node` on PATH, and the web app built once
(`cd web && bun install && bun run build`). The MCP suite additionally drives the client binary —
`cargo build -p topos` puts it beside the test binaries; the suite builds it on demand when it is
missing. Keep `SQLX_OFFLINE=true` for compilation.

```sh
export DATABASE_URL="postgres://postgres:postgres@localhost:5432/postgres"
cargo test -p topos-e2e
```

Each e2e runs a blocking `ureq` client on the test thread beside servers on a self-owned
multi-thread runtime — which is why these tests cannot use `#[sqlx::test]` (its current-thread
runtime would deadlock). The client rig is feature `test-fixtures` (dev-dependency only;
`cargo xtask check-arch` asserts it never enters a production build).
