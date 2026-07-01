# Topos

A layer for AI agents to **share their behaviors** across a team — so every agent stays current with the
same company processes and everyone gets a consistent experience. A *behavior* (a "skill") is a bundle of
files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

This repository is two programs in one Apache-2.0 Cargo workspace:

- **`topos`** — the local CLI an agent drives to add, follow, publish, and update behaviors across harnesses.
- **`topos-plane`** — the self-hostable sharing server (a library + a thin binary).

They share one trust kernel (`topos-core`): the single, auditable implementation of the byte-exact digest,
consent, signing, and sync algorithm.

## Build & test

```sh
cargo build
cargo test           # requires a Postgres (see below)
cargo fmt --all
cargo clippy --all-targets
```

The plane's storage authority stores its metadata in **Postgres**. The tests provision a fresh database per
test, so `cargo test` needs a reachable Postgres and a `DATABASE_URL`:

```sh
export DATABASE_URL="postgres://topos:topos@localhost:5432/topos"
export SQLX_OFFLINE=true
# e.g. a throwaway one:  docker run --rm -e POSTGRES_USER=topos -e POSTGRES_PASSWORD=topos \
#                          -e POSTGRES_DB=topos -p 5432:5432 postgres:18
cargo test
```

Compilation itself is offline — the compile-time-checked queries read the committed `crates/plane-store/.sqlx`
metadata — so `cargo build`, `clippy`, and `doc` need no database; only running the tests does. Keep
`SQLX_OFFLINE=true` exported alongside `DATABASE_URL` (as above, and as CI does): with `DATABASE_URL` set but
the flag unset, sqlx's `query!` macros compile *live* against that database, so a fresh or unmigrated one
fails the build with confusing errors. The flag only affects compilation — the runtime `#[sqlx::test]` still
provisions a fresh database per test against `DATABASE_URL`.

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

A one-command sanity check that the image builds, starts, and reaches its database:

```sh
./scripts/compose-smoke.sh
```

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
old served generation while the compare-and-set reads the bumped column). A standalone re-sign-on-restore
helper is not yet shipped; until it is, treat an older-snapshot restore as requiring a re-publish of the
affected pointers.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
