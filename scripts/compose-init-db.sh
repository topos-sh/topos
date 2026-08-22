#!/bin/sh
# compose-init-db.sh — first-boot role + schema provisioning for the bundled Postgres.
#
# Mounted into the postgres image at /docker-entrypoint-initdb.d/, this runs ONCE, as the superuser,
# against ${POSTGRES_DB}, on a fresh data volume — BEFORE the server accepts TCP connections (so before
# the healthcheck passes, and therefore before any application container starts). Every role and every
# schema must exist before its application first boots; this script is where they are born.
#
# ONE ROLE PER APPLICATION, each owning its schema and running its own migration lineage at boot —
# the posture mainstream self-hosted products use:
#   • topos_web owns schema `web` (the app: identity, policy, product rows — the drizzle lineage);
#   • topos_plane owns schema `plane` (the vault: byte custody only — the sqlx lineage);
#   • topos_gateway owns schema `gateway` (the MCP gateway: credential custody, observed tools,
#     usage — its own plain-SQL lineage).
# In-lane protection is constraints + triggers (bug-guards); what stays GRANT-enforced is the
# cross-lane boundary: the app cannot write (or ALTER) plane, the vault cannot read web, and the
# gateway can only READ the web tables it resolves calls against. The app's read-only view of
# custody state (history/fleet pages) rides ALTER DEFAULT PRIVILEGES, so tables from future plane
# migrations arrive already SELECT-granted — no manual grant re-runs on a live deployment, ever;
# the gateway's read of `web` is granted the same way. What the app may read of `gateway` is NOT:
# the gateway's own migrations grant it table by table, because the ciphertext table must never be
# swept up by a blanket.
#
# Passwords come from the environment (TOPOS_PLANE_DB_PASSWORD / TOPOS_WEB_DB_PASSWORD /
# TOPOS_GATEWAY_DB_PASSWORD, matching the DATABASE_URLs the compose file builds), with dev-only
# defaults. An initdb .sh script (not a .sql one) is required because .sql files cannot read the
# compose environment.
#
# The whole thing is fed to psql over STDIN (not `-c`): psql performs `:'var'` interpolation on file/stdin
# input but NOT on a `-c` string, so the passwords ride as psql variables and are safely quoted by
# `format(%L)`. Role creation is guarded with `\gexec` so a manual re-run is a no-op. `ON_ERROR_STOP=1`
# makes any failure fail the whole init (postgres then aborts first boot rather than leaving a
# half-provisioned cluster) — so a green `up` means the roles + schemas really landed.
set -eu

plane_pw="${TOPOS_PLANE_DB_PASSWORD:-plane}"
web_pw="${TOPOS_WEB_DB_PASSWORD:-web}"
gateway_pw="${TOPOS_GATEWAY_DB_PASSWORD:-gateway}"
db="${POSTGRES_DB:-topos}"

# `$db` is shell-interpolated into the (quoted) SQL identifiers below; `:'plane_pw'` / `:'web_pw'` /
# `:'gateway_pw'` are left for psql. The heredoc is unquoted, but the only shell-special token in the
# body is `$db`.
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$db" \
     -v plane_pw="$plane_pw" -v web_pw="$web_pw" -v gateway_pw="$gateway_pw" <<EOSQL
SELECT format('CREATE ROLE topos_plane LOGIN PASSWORD %L', :'plane_pw')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_plane')\gexec
SELECT format('CREATE ROLE topos_web LOGIN PASSWORD %L', :'web_pw')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_web')\gexec
SELECT format('CREATE ROLE topos_gateway LOGIN PASSWORD %L', :'gateway_pw')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'topos_gateway')\gexec

REVOKE ALL ON DATABASE "$db" FROM PUBLIC;
GRANT CONNECT ON DATABASE "$db" TO topos_plane;
GRANT CONNECT ON DATABASE "$db" TO topos_web;
GRANT CONNECT ON DATABASE "$db" TO topos_gateway;
-- The app's migrator (and its ledger bootstrap) issues CREATE SCHEMA IF NOT EXISTS, and
-- Postgres checks the CREATE privilege BEFORE the existence short-circuit — so the app role
-- needs database CREATE even though its schema is born here. In-lane latitude only: schema
-- plane and its objects stay owned by topos_plane, out of reach. The gateway's own migrator
-- opens with the same statement and needs the same latitude.
GRANT CREATE ON DATABASE "$db" TO topos_web;
GRANT CREATE ON DATABASE "$db" TO topos_gateway;

-- Each application owns its schema (the owner may not hold CREATE on the database, so the
-- superuser makes them here).
CREATE SCHEMA IF NOT EXISTS web     AUTHORIZATION topos_web;
CREATE SCHEMA IF NOT EXISTS plane   AUTHORIZATION topos_plane;
CREATE SCHEMA IF NOT EXISTS gateway AUTHORIZATION topos_gateway;

-- Role-level search_path (probed in CI by LOGGING IN as the role — SET ROLE does not adopt it).
ALTER ROLE topos_web     IN DATABASE "$db" SET search_path = web, plane;
ALTER ROLE topos_plane   IN DATABASE "$db" SET search_path = plane;
ALTER ROLE topos_gateway IN DATABASE "$db" SET search_path = gateway, web;

-- The app reads custody state (history/fleet pages) — read-only, grant-enforced; the
-- default privileges cover every table a future plane migration adds.
GRANT USAGE ON SCHEMA plane TO topos_web;
ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane
  GRANT SELECT ON TABLES TO topos_web;

-- The gateway resolves every proxied call against a FIXED set of web rows it may only READ. Those
-- grants are NOT made here: a blanket "SELECT on all of web" (or the ALTER DEFAULT PRIVILEGES that
-- would cover a fresh volume) would also hand the most-exposed component the identity store — the
-- Better Auth account tokens, session tokens, and emails. Instead the web lineage migration
-- 0028_gateway_web_reads grants exactly the eight tables the gateway reads, guarded on this role
-- existing, once the tables are real and topos_web owns them. Creating the role here is enough; the
-- next web boot-migration makes the precise grants (and re-affirms them idempotently).

-- The app renders sign-in state, the tools checklist and the usage page straight out of the
-- gateway's schema — so it needs to REACH the schema. WHICH TABLES it may read is decided in
-- the gateway's own migrations, table by table and never by a blanket: the ciphertext,
-- workspace-key and in-flight-authorize tables are granted to nobody, ever.
GRANT USAGE ON SCHEMA gateway TO topos_web;
EOSQL

echo "compose-init-db: roles topos_plane/topos_web/topos_gateway + schemas plane/web/gateway provisioned in database $db"
