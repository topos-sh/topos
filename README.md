# Topos

A layer for AI agents to **share their behaviors** across a team — so every agent stays current with the
same company processes and everyone gets a consistent experience. A *behavior* (a "skill") is a bundle of
files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

**Status: real, but early.** The distribute and contribute loops both work end-to-end today — publish →
follow → pull lands byte-exact, signature-verified bundles (proven against the real Claude Code adapter),
and propose → review → apply plus team-wide revert run over HTTP, with a compose file that self-hosts the
plane. Not there yet: the plane serves plain HTTP (terminate TLS at a reverse proxy in front), the
large-object store has no S3-compatible remote backend, harness adapters beyond Claude Code, and seating a
brand-new plane's first workspace owner is not yet an in-band command (see `docs/RELEASE.md`).

This repository is two programs in one Apache-2.0 Cargo workspace:

- **`topos`** — the local CLI an agent drives to add, follow, publish, and update behaviors across harnesses.
- **`topos-plane`** — the self-hostable sharing server (a library + a thin binary).

They share one trust kernel (`topos-core`): the single, auditable implementation of the byte-exact digest,
consent, signing, and sync algorithm.

## Using the CLI

The agent usually drives these non-interactively (add `--json` for the machine envelope); the same verbs
work by hand.

**Adopt** — track a skill where it already sits (offline; no server, no account):

```sh
topos add ~/.claude/skills/pr-describe
```

**Share** — ship it to the team. Consent is explicit: `--approve` pins the exact bytes being shipped
(`topos list --json` shows each skill's digest). A skill's first-ever publish by a workspace owner also
prints an invite link for teammates:

```sh
topos publish pr-describe --approve pr-describe@<digest>
```

**Join** — enroll once via an invite link, approve the disclosed first version, then stay current:

```sh
topos follow https://plane.example/i/<token>   # prints a verification URL; confirm it, then:
topos follow --resume                          # completes the enrollment
topos follow --approve pr-describe             # place the disclosed first version
topos pull                                     # updates; the session-start hook runs this for you
```

Following arms the Claude Code session-start hook (`topos pull --quiet`), so updates the team publishes
land at the start of each session — byte-exact, verified against the plane's signed `current` pointer, and
never over your local edits (a diverged draft is surfaced, not clobbered). Propose a change back with
`topos publish <skill> --propose --approve <skill>@<digest>`; a reviewer lands it with
`topos review <skill>@<hash> --approve`.

## Build & test

```sh
cargo build
cargo xtask ci       # the full non-DB gate sequence, in CI's order (fmt, clippy, doc, drift gates, check-arch)
cargo test           # requires a Postgres (see below)
```

`cargo xtask ci` is the pre-push loop — one command that matches CI's `gate` job exactly. (The `xtask`
alias is provided by the committed `.cargo/config.toml`.)

The plane's storage authority stores its metadata in **Postgres**. The tests provision a fresh database per
test, so `cargo test` needs a reachable Postgres and a `DATABASE_URL`:

```sh
export DATABASE_URL="postgres://topos:topos@localhost:5432/topos"
# e.g. a throwaway one:  docker run --rm -e POSTGRES_USER=topos -e POSTGRES_PASSWORD=topos \
#                          -e POSTGRES_DB=topos -p 5432:5432 postgres:18
cargo test
```

Compilation itself is offline — the compile-time-checked queries read the committed
`crates/plane-store/.sqlx` metadata — so `cargo build`, `clippy`, and `doc` need no database; only running
the tests does. The committed `.cargo/config.toml` defaults `SQLX_OFFLINE=true` for exactly this reason
(non-forced: a value you export yourself wins — CI's metadata-regeneration job sets `SQLX_OFFLINE=false`
explicitly). The flag only affects compilation — the runtime `#[sqlx::test]` still provisions a fresh
database per test against `DATABASE_URL`.

## Install

```sh
curl -fsSL https://github.com/topos-sh/topos/releases/latest/download/install.sh | sh
```

Installs the `topos` binary to `~/.local/bin` — no sudo. Supported platforms: macOS (Apple Silicon
and Intel) and Linux (x86_64 and arm64; static musl binaries, any distro — no compiler, Node,
Python, or git needed at runtime). On Windows, run it inside
[WSL2](https://learn.microsoft.com/windows/wsl/install) — the Linux x86_64 binary works there;
native Windows binaries are not yet published.

Knobs (env var or flag):

| Knob | Flag | Default | What it does |
|---|---|---|---|
| `TOPOS_VERSION` | `--version <tag>` | latest | pin a specific release tag |
| `TOPOS_INSTALL_DIR` | `--to <dir>` | `~/.local/bin` | install directory |
| `TOPOS_INSTALL_BASE_URL` | — | GitHub releases | alternate download base (mirrors, air-gapped proxies; same URL layout) |

**What the checksum proves — and what it does not.** The installer downloads `SHA256SUMS` over TLS
from the same origin as the binary, prints the expected and the locally computed sha256, and refuses
to install on any mismatch (this check cannot be disabled). A match proves *transit* integrity — the
bytes you received are the bytes the release published — but not *origin* integrity: whoever controls
the release controls both files. For an origin-independent check, use GitHub artifact attestation,
which validates the artifact was built by this repository's release workflow and is Sigstore-signed:

```sh
gh attestation verify topos-<target>.tar.gz --repo topos-sh/topos
```

## Self-hosting the plane

Self-hosting is one stateless plane container plus a Postgres. The bundled compose file runs both:

```sh
docker compose up --build
# → the plane on http://localhost:8787, a pinned Postgres beside it
```

The plane image is **stateless and holds no database** (one concern per container): Postgres runs as its
own service, and the plane connects to it via `DATABASE_URL`. On first boot the plane migrates the schema
and generates its `0600` signing key + enrollment secret onto the mounted `plane-data` volume; the
git-object and large-object stores live there too.

### Configuration reference

Everything the plane binary reads from the environment (most also exist as `--flags`; run
`topos-plane --help`). The Docker image presets the paths + bind (the *image* column).

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | *required* | The Postgres connection URL; the schema migrates on startup. Append `?sslmode=require` for a networked database. |
| `TOPOS_PLANE_BIND` | `127.0.0.1:8787` (image: `0.0.0.0:8787`) | The address to bind. |
| `TOPOS_PLANE_GIT_ROOT` | *required* (image: `/data/git`) | The per-workspace git-object store root (created if absent). |
| `TOPOS_PLANE_LARGE_ROOT` | *required* (image: `/data/large`) | The per-workspace large-object store root (created if absent). |
| `TOPOS_PLANE_KEY` | *required* (image: `/data/plane.key`) | The plane signing key — a `0600` seed, generated on first run if absent. |
| `TOPOS_PLANE_ENROLL_SECRET` | *required* (image: `/data/enroll.key`) | The enrollment HMAC secret (`0600` seed, generated on first run) every opaque invite / grant / read token derives from. |
| `TOPOS_PLANE_BASE_URL` | `http://<bind>` | The plane's PUBLIC base URL — invite + verification links are built on it; set it to the externally reachable address behind a reverse proxy. |
| `TOPOS_PLANE_MODE` | `self_host` | The deployment posture: `self_host` or `cloud`. |
| `TOPOS_PLANE_ENROLLMENT_METHOD` | `passcode` if SMTP is configured, else `device_code` | The enrollment method advertised in the bootstrap. |
| `TOPOS_PLANE_SMTP_HOST` / `_PORT` / `_USER` / `_PASS` / `_FROM` | unset | The SMTP relay for emailed-passcode enrollment — **all five** must be set to enable it (credentials are never logged). |
| `TOPOS_PLANE_ADMIN_TOKEN` | unset | The operator admin bearer token (only its sha256 is retained). Enables `PUT /v1/workspaces/{ws}/policy/review-required` — the **review-required** anti-poisoning gate; while unset, that route answers 404. |
| `TOPOS_PLANE_GC_INTERVAL_SECS` | `300` | Seconds between storage-maintenance passes (recovery sweep + quarantine janitor + per-workspace garbage collection; the first pass runs at startup). `0` disables the scheduler (run the passes out-of-band). |
| `TOPOS_PLANE_RATELIMIT` | on | `off` disables the in-process rate limiter. |
| `TOPOS_PLANE_DB_MAX_CONNECTIONS` | driver default (10) | The Postgres pool size — raise it for a plane serving concurrent HTTP. |
| `TOPOS_PLANE_DB_ACQUIRE_TIMEOUT_SECS` | driver default (30) | The pool acquire timeout. |
| `TOPOS_PLANE_DB_STATEMENT_TIMEOUT_SECS` | off | An opt-in per-statement ceiling (off so a long legitimate whole-bundle render is never capped). |
| `TOPOS_PLANE_DB_LOCK_TIMEOUT_SECS` | off | An opt-in lock-wait ceiling. |
| `TOPOS_PLANE_DB_IDLE_IN_TX_TIMEOUT_SECS` | `30` | Kills abandoned/stuck transactions that would pin row locks; `0` disables. |
| `TOPOS_PLANE_OIDC_ISSUER` / `_CLIENT_ID` / `_CLIENT_SECRET` / `_REDIRECT_URI` | unset | The generic OIDC enrollment connector — read only when the plane is built with `--features enroll-oidc` (default-off); a complete set enables it. |
| `RUST_LOG` | `info` | The JSON-log filter (a standard tracing `EnvFilter`). |

Client-side: `TOPOS_DEBUG=1` makes the `topos` CLI print each error's full source chain to stderr (stdout
stays the clean envelope; the chain always lands in `~/.topos/log.jsonl`). `TOPOS_HOME` overrides the
`~/.topos` sidecar root, and the Claude Code adapter honors `$CLAUDE_CONFIG_DIR` exactly as Claude Code
does.

### Bring your own Postgres

To use a managed or external Postgres instead of the bundled one, set `DATABASE_URL` and start just the
plane (the `db` service goes unused):

```sh
export DATABASE_URL="postgres://user:pass@your-db.example:5432/topos?sslmode=require"
docker compose up --build --no-deps plane
```

`--no-deps` starts only the plane, leaving the bundled `db` service down (it would otherwise start because
the plane declares it as a dependency, and could clash on port 5432).

Add `?sslmode=require` when reaching a database over the network — the plane speaks TLS to Postgres over
rustls. Terminate TLS for the plane's own HTTP at a reverse proxy in front of it, and set
`TOPOS_PLANE_BASE_URL` to the externally reachable address (the invite + verification links are built on it).

### First contact

A one-command sanity check that the image builds, starts, and reaches its database (CI runs the same
script on pushes to main):

```sh
./scripts/compose-smoke.sh
```

### TLS: reverse proxy (recommended) or built-in ACME (experimental)

**Reverse proxy (recommended).** The plane serves plain HTTP on `:8787` and is designed to sit
behind a TLS-terminating reverse proxy (Caddy, nginx, Traefik, or your platform's load balancer).
Point the proxy at `http://plane:8787`, set `TOPOS_PLANE_BASE_URL` to your public `https://…`
address (the invite + verification links are built on it), and let the proxy own certificates and
renewal — the best-understood, most operable setup.

**Built-in ACME (experimental).** The image can also be built with an optional, default-off `acme`
feature that adds a second, TLS listener with automatic certificates via the ACME tls-alpn-01
challenge (rustls-acme, ring-only):

```sh
docker build --build-arg FEATURES=acme -t topos-plane:acme .
```

The feature alone changes nothing — a non-empty domain list is the on-switch:

```sh
TOPOS_PLANE_ACME_DOMAINS=plane.example.com      # non-empty = ACME on
TOPOS_PLANE_ACME_CONTACT=mailto:ops@example.com # required when on
TOPOS_PLANE_ACME_CACHE=/data/acme               # required when on; on the volume, so the
                                                # account + certs survive restarts
# optional:
TOPOS_PLANE_ACME_DIRECTORY=…    # default: Let's Encrypt production — try staging first
TOPOS_PLANE_ACME_BIND=0.0.0.0:8443
TOPOS_PLANE_ACME_EXTRA_ROOT=…   # extra PEM trust root, for TEST ACME directories only
```

Map public 443 to the container's 8443 — the challenge is answered inside the TLS acceptor on that
same port, so no separate port 80 is needed. The plain HTTP listener on 8787 keeps serving
unchanged beside it (healthchecks and loopback keep working).

**What "experimental" means.** The mechanism is rehearsed end to end against a local ACME test
server (`scripts/acme-rehearsal.sh`: a real tls-alpn-01 issuance, serving over verified TLS, and
the certificate surviving a plane restart from the cache with the ACME server down). What only a
real box proves: public DNS for your domain, Let's Encrypt staging → production, CA rate limits,
renewal timing over weeks, and IPv4/IPv6 reachability of your 443. Start against the staging
directory (`https://acme-staging-v02.api.letsencrypt.org/directory`) — production rate limits are
strict. If any of this is friction, use the reverse proxy.

The first *client* command against the running plane is a `follow` of an invite link minted on it:

```sh
topos follow http://localhost:8787/i/<token>
```

Honest caveat: minting the **first** identity on a brand-new plane is not yet in-band — the `admin-claim`
route that seats a fresh workspace's first owner is built, but the binary does not yet mint its one-time
claim token (it is on the launch checklist, `docs/RELEASE.md`). Until then a fresh plane is exercised end
to end by the workspace-seeded e2e suites (`cargo test -p topos-e2e`) and the smoke probe above.

### Backups & restore

There are two independent pieces of state to back up: the **Postgres metadata database** and the plane's
**object stores** (the `plane-data` volume — git + large objects + the keys).

The database is authoritative and the object store trails it, so **snapshot the object store first, then the
database** — that way a restored database never references an object the store backup lacks (a content-
addressed object the store has but the database doesn't is harmless and reclaimed by garbage collection).

```sh
# 1. object stores + keys (a volume snapshot, or a tar of the mounted volume)
docker run --rm -v <project>_plane-data:/data -v "$PWD":/backup alpine \
  tar czf /backup/plane-data.tgz -C /data .
# 2. the metadata database
docker compose exec db pg_dump -U topos topos > topos-db.sql
```

**Upgrading Postgres across a major version (e.g. an existing deployment to `postgres:18`) is a dump/restore,
not just a tag bump.** The official image changed its data directory and declared volume at v18 (PGDATA is now
`/var/lib/postgresql/<major>/docker`; the volume is the parent `/var/lib/postgresql`), so an existing pre-18
`pg-data` volume must be `pg_dump`ed on the old image and restored into the new layout — pointing `postgres:18`
at the old `/var/lib/postgresql/data` mount will refuse to start. (The bundled compose file already mounts the
new path, so fresh deployments need nothing extra.)

**Restoring an *older* snapshot needs care — prefer rolling forward.** Restoring a backup can move a skill's
`current` pointer backward relative to what followers already observed, and followers reject a pointer at or
below the highest generation they have seen (anti-rollback). Recovering forward from the newest good backup
avoids this entirely.

If you must restore to an older state, the affected skills' `current` pointers have to be **re-issued
through the plane** at a fresh, higher generation before followers reconnect. This must go through the plane
because followers verify the *signed* pointer record (what `GET /v1/current` returns), not the database
column — so a raw `UPDATE current SET epoch = …` does **not** work: it leaves the served signature unchanged
(followers never see the bump) and makes every author's next write `CONFLICT` (the author signs the
old served generation while the compare-and-set reads the bumped column). The plane ships that re-sign
as a subcommand, and an older-snapshot restore is these four steps:

```sh
# 1. stop the plane (leave the database up)
docker compose stop plane
# 2. restore BOTH pieces from the SAME backup set: the database (psql < topos-db.sql after
#    recreating it) and the plane-data volume (untar plane-data.tgz back into the volume)
# 3. re-issue every restored pointer at a bumped, freshly signed generation
docker compose run --rm plane restore-bump-epoch --all-workspaces
# 4. bring the plane back
docker compose up -d
```

The helper re-signs each skill's `current` at a **bumped epoch** (same bytes, a strictly higher
generation), so followers see an ordinary forward move — including followers that already raised the
rollback alarm; they recover on their next pull. Re-running it is safe (each run is one more forward
bump). Two things to watch:

- **The database and the `plane-data` volume must come from the same backup set.** The helper signs
  with `/data/plane.key`, and followers verify against the plane key they pinned when they first
  followed. Every line the helper prints ends with the signing `key <key_id>` — compare it against
  your pre-incident key id. A restored `/data` holding a *different* (or lost) seed changes the key
  id and every follower fails closed; that is a key-rotation event, and no epoch bump fixes it.
- **If you have restored before from an even older backup**, pass `--epoch-at-least <n>` with a
  value above any epoch you have ever served (the helper takes the max of that floor and each
  pointer's `epoch + 1`), so a repeated restore can never re-issue a generation followers already
  recorded. Keep the helper's printed output with your backup records.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
