#!/bin/sh
# compose-init-db.sh — first-boot role + schema provisioning for the bundled Postgres.
#
# Mounted into the postgres image at /docker-entrypoint-initdb.d/, this runs ONCE, as the superuser,
# against ${POSTGRES_DB}, on a fresh data volume — BEFORE the server accepts TCP connections (so before
# the healthcheck passes, and therefore before the plane or web containers start). That ordering is the
# point: the plane's migration 0019 records the web tier's grants but SKIPS them when `topos_web` is
# absent, so BOTH roles must exist before the plane first boots. This script is where they are born.
#
# What it establishes (mirroring web/tests/e2e/db-setup.mjs — the in-repo record of the role/grant model):
#   • two LOGIN roles, topos_plane (owns schema `plane`, runs the plane migrations) and topos_web (owns
#     schema `web`, reads `plane`, writes it ONLY through the guarded topos_* functions);
#   • the database locked down — REVOKE ALL FROM PUBLIC, then CONNECT granted to both and CREATE to
#     topos_web (its drizzle migrator runs CREATE SCHEMA web);
#   • schema `plane` created up front, AUTHORIZATION topos_plane (topos_plane holds no CREATE on the
#     database, so it could not create its own schema after a SET ROLE — the superuser makes it here);
#   • per-database search_paths: topos_plane → plane; topos_web → web, plane (web first for its own
#     unqualified tables, plane on the path for the unqualified guarded-function calls).
# The per-table plane grants are NOT here — migration 0019 carries them next to the schema they bind.
#
# Passwords come from the environment (TOPOS_PLANE_DB_PASSWORD / TOPOS_WEB_DB_PASSWORD, matching the
# DATABASE_URLs the compose file builds), with dev-only defaults. An initdb .sh script (not a .sql one)
# is required because .sql files cannot read the compose environment.
#
# The whole thing is fed to psql over STDIN (not `-c`): psql performs `:'var'` interpolation on file/stdin
# input but NOT on a `-c` string, so the passwords ride as psql variables and are safely quoted by
# `format(%L)`. Role creation is guarded with `\gexec` so a manual re-run is a no-op. `ON_ERROR_STOP=1`
# makes any failure fail the whole init (postgres then aborts first boot rather than leaving a
# half-provisioned cluster) — so a green `up` means the roles + schema really landed.
set -eu

plane_pw="${TOPOS_PLANE_DB_PASSWORD:-plane}"
web_pw="${TOPOS_WEB_DB_PASSWORD:-web}"
db="${POSTGRES_DB:-topos}"

# `$db` is shell-interpolated into the (quoted) SQL identifiers below; `:'plane_pw'` / `:'web_pw'` are
# left for psql. The heredoc is unquoted, but the only shell-special token in the body is `$db`.
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$db" \
     -v plane_pw="$plane_pw" -v web_pw="$web_pw" <<EOSQL
SELECT format('CREATE ROLE topos_plane LOGIN PASSWORD %L', :'plane_pw')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_plane')\gexec
SELECT format('CREATE ROLE topos_web LOGIN PASSWORD %L', :'web_pw')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_web')\gexec

REVOKE ALL ON DATABASE "$db" FROM PUBLIC;
GRANT CONNECT ON DATABASE "$db" TO topos_plane;
GRANT CONNECT ON DATABASE "$db" TO topos_web;
GRANT CREATE  ON DATABASE "$db" TO topos_web;
CREATE SCHEMA IF NOT EXISTS plane AUTHORIZATION topos_plane;
ALTER ROLE topos_plane IN DATABASE "$db" SET search_path = plane;
ALTER ROLE topos_web   IN DATABASE "$db" SET search_path = web, plane;
EOSQL

echo "compose-init-db: roles topos_plane/topos_web + schema plane provisioned in database $db"
