#!/bin/sh
# compose-init-db.sh — first-boot role + schema provisioning for the bundled Postgres.
#
# Mounted into the postgres image at /docker-entrypoint-initdb.d/, this runs ONCE, as the superuser,
# against ${POSTGRES_DB}, on a fresh data volume — BEFORE the server accepts TCP connections (so before
# the healthcheck passes, and therefore before the plane or web containers start). Both roles and both
# schemas must exist before either application first boots; this script is where they are born.
#
# ONE ROLE PER APPLICATION, each owning its schema and running its own migration lineage at boot —
# the posture mainstream self-hosted products use:
#   • topos_web owns schema `web` (the app: identity, policy, product rows — the drizzle lineage);
#   • topos_plane owns schema `plane` (the vault: byte custody only — the sqlx lineage).
# In-lane protection is constraints + triggers (bug-guards); what stays GRANT-enforced is the
# cross-lane boundary: the app cannot write (or ALTER) plane, and the vault cannot read web. The
# app's read-only view of custody state (history/fleet/currency pages) rides ALTER DEFAULT
# PRIVILEGES, so tables from future plane migrations arrive already SELECT-granted — no manual
# grant re-runs on a live deployment, ever.
#
# Passwords come from the environment (TOPOS_PLANE_DB_PASSWORD / TOPOS_WEB_DB_PASSWORD, matching the
# DATABASE_URLs the compose file builds), with dev-only defaults. An initdb .sh script (not a .sql one)
# is required because .sql files cannot read the compose environment.
#
# The whole thing is fed to psql over STDIN (not `-c`): psql performs `:'var'` interpolation on file/stdin
# input but NOT on a `-c` string, so the passwords ride as psql variables and are safely quoted by
# `format(%L)`. Role creation is guarded with `\gexec` so a manual re-run is a no-op. `ON_ERROR_STOP=1`
# makes any failure fail the whole init (postgres then aborts first boot rather than leaving a
# half-provisioned cluster) — so a green `up` means the roles + schemas really landed.
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
-- The app's migrator (and its ledger bootstrap) issues CREATE SCHEMA IF NOT EXISTS, and
-- Postgres checks the CREATE privilege BEFORE the existence short-circuit — so the app role
-- needs database CREATE even though its schema is born here. In-lane latitude only: schema
-- plane and its objects stay owned by topos_plane, out of reach.
GRANT CREATE ON DATABASE "$db" TO topos_web;

-- Each application owns its schema (the owner may not hold CREATE on the database, so the
-- superuser makes them here).
CREATE SCHEMA IF NOT EXISTS web   AUTHORIZATION topos_web;
CREATE SCHEMA IF NOT EXISTS plane AUTHORIZATION topos_plane;

-- Role-level search_path (probed in CI by LOGGING IN as the role — SET ROLE does not adopt it).
ALTER ROLE topos_web   IN DATABASE "$db" SET search_path = web, plane;
ALTER ROLE topos_plane IN DATABASE "$db" SET search_path = plane;

-- The app reads custody state (history/fleet/currency pages) — read-only, grant-enforced; the
-- default privileges cover every table a future plane migration adds.
GRANT USAGE ON SCHEMA plane TO topos_web;
ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane
  GRANT SELECT ON TABLES TO topos_web;
EOSQL

echo "compose-init-db: roles topos_plane/topos_web + schemas web/plane provisioned in database $db"
