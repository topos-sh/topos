#!/bin/sh
# check-db-grants.sh — the cross-lane grants-shape gate, probed AS THE LOGIN ROLES.
#
# The database boundary this repo ships is: two login roles, one per application, each owning
# its schema — and the CROSS-LANE rules grant-enforced: the web app cannot write (or ALTER, or
# CREATE IN) schema `plane`; the vault cannot read schema `web`; the app CAN read every plane
# table, including tables created by FUTURE vault migrations (ALTER DEFAULT PRIVILEGES). This
# script provisions a scratch database with the real initdb (scripts/compose-init-db.sh), runs
# the real web + plane lineages, then probes every rule by LOGGING IN as each role — never
# SET ROLE, which does not adopt the role's search_path and would prove nothing about a real
# connection.
#
# Usage: PGHOST/PGPORT/PGUSER/PGPASSWORD (a superuser) must reach a Postgres; the script
# creates and drops its own database. `--self-test` additionally provisions a deliberately
# broken variant (no default-privilege grant) and asserts the probe FAILS — the gate's own
# red test.
set -eu

PGHOST="${PGHOST:-localhost}"
PGPORT="${PGPORT:-5432}"
PGUSER="${PGUSER:-postgres}"
export PGHOST PGPORT PGUSER
: "${PGPASSWORD:?set PGPASSWORD for the superuser}"

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
db="topos_grants_check_$$"
fail() { echo "check-db-grants: FAIL — $1" >&2; exit 1; }

cleanup() {
  psql -X -q -d postgres -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)" >/dev/null 2>&1 || true
}
trap cleanup EXIT

provision() {
  # $1 = 'real' | 'broken' (broken skips the ALTER DEFAULT PRIVILEGES grant — the self-test)
  psql -X -q -v ON_ERROR_STOP=1 -d postgres -c "CREATE DATABASE \"$db\""
  POSTGRES_USER="$PGUSER" POSTGRES_DB="$db" \
  TOPOS_PLANE_DB_PASSWORD=plane TOPOS_WEB_DB_PASSWORD=web \
    sh "$repo_root/scripts/compose-init-db.sh" >/dev/null
  # The initdb's role creation is idempotent-guarded; on a cluster where the roles pre-exist
  # (CI reruns, dev boxes) their passwords may differ — pin the probe passwords here.
  psql -X -q -v ON_ERROR_STOP=1 -d "$db" \
    -c "ALTER ROLE topos_plane PASSWORD 'plane'; ALTER ROLE topos_web PASSWORD 'web'" >/dev/null
  if [ "$1" = "broken" ]; then
    psql -X -q -v ON_ERROR_STOP=1 -d "$db" \
      -c "ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane REVOKE SELECT ON TABLES FROM topos_web" >/dev/null
  fi
  # The real lineages, each applied AS ITS OWN role (exactly how the applications boot).
  PGPASSWORD=plane psql -X -q -v ON_ERROR_STOP=1 -U topos_plane -d "$db" \
    -f "$repo_root/crates/plane-store/migrations/0001_custody.sql" >/dev/null
  PGPASSWORD=web psql -X -q -v ON_ERROR_STOP=1 -U topos_web -d "$db" \
    -f "$repo_root/web/drizzle/0000_init.sql" >/dev/null
  # A table from a "future" vault migration — the default-privileges proof target.
  PGPASSWORD=plane psql -X -q -v ON_ERROR_STOP=1 -U topos_plane -d "$db" \
    -c "CREATE TABLE future_custody_fact (id text PRIMARY KEY)" >/dev/null
}

# as_web / as_plane run one statement as the LOGIN role; expect_ok / expect_denied assert.
as_role() { PGPASSWORD="$2" psql -X -q -v ON_ERROR_STOP=1 -U "$1" -d "$db" -tAc "$3" 2>&1; }
expect_ok() { # role pass sql label
  out=$(as_role "$1" "$2" "$3") || fail "$4 (expected allowed, got: $out)"
}
expect_denied() { # role pass sql label
  if out=$(as_role "$1" "$2" "$3"); then fail "$4 (expected denied, got: $out)"; fi
}

probe_real() {
  # Role-level search_path, adopted at LOGIN.
  sp=$(as_role topos_web web "SHOW search_path")
  [ "$sp" = "web, plane" ] || fail "topos_web search_path is '$sp', wanted 'web, plane'"
  sp=$(as_role topos_plane plane "SHOW search_path")
  [ "$sp" = "plane" ] || fail "topos_plane search_path is '$sp', wanted 'plane'"

  # The app reads custody state — including a table born AFTER the grants were set.
  expect_ok topos_web web "SELECT count(*) FROM plane.version" "web reads plane.version"
  expect_ok topos_web web "SELECT count(*) FROM plane.future_custody_fact" \
    "web reads a future plane table (default privileges)"

  # The app cannot write, ALTER, or CREATE IN plane.
  expect_denied topos_web web \
    "INSERT INTO plane.version (workspace_id,bundle_id,version_id,commit_id,author_display) VALUES ('w','b','v','c','x')" \
    "web INSERT into plane refused"
  expect_denied topos_web web "ALTER TABLE plane.version ADD COLUMN sneaky text" \
    "web ALTER of plane refused"
  expect_denied topos_web web "CREATE TABLE plane.intruder (id text)" \
    "web CREATE IN plane refused"
  expect_denied topos_web web "DROP TABLE plane.version" "web DROP of plane refused"

  # The vault is blind to web — it cannot even USAGE the schema.
  expect_denied topos_plane plane "SELECT count(*) FROM web.\"user\"" "plane SELECT of web.user refused"
  expect_denied topos_plane plane "SELECT count(*) FROM web.seat" "plane SELECT of web.seat refused"
  expect_denied topos_plane plane "INSERT INTO web.audit_event (workspace_id, actor_display, kind, outcome) VALUES ('w','x','k','ok')" \
    "plane INSERT into web refused"

  # Each application owns and writes its own schema.
  expect_ok topos_plane plane \
    "INSERT INTO plane.version (workspace_id,bundle_id,version_id,commit_id,author_display) VALUES ('w','b','v','c','x')" \
    "plane writes its own schema"
  expect_ok topos_web web "INSERT INTO web.workspace (id, name, display_name, claim_code_sha256) VALUES ('w1','t','T', sha256('x'::bytea))" \
    "web writes its own schema"
  echo "check-db-grants: OK (all cross-lane probes green)"
}

provision real
probe_real
cleanup

if [ "${1:-}" = "--self-test" ]; then
  trap cleanup EXIT
  provision broken
  if out=$(as_role topos_web web "SELECT count(*) FROM plane.future_custody_fact"); then
    fail "self-test: broken provisioning still let web read the future table — the gate cannot fire"
  fi
  cleanup
  echo "check-db-grants: self-test OK (the gate fires on a broken grant shape)"
fi
