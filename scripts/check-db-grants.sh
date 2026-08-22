#!/bin/sh
# check-db-grants.sh — the cross-lane grants-shape gate, probed AS THE LOGIN ROLES.
#
# The database boundary this repo ships is: three login roles, one per application, each owning
# its schema — and the CROSS-LANE rules grant-enforced:
#   • the web app cannot write (or ALTER, or CREATE IN) schema `plane`, but CAN read every plane
#     table, including tables created by FUTURE vault migrations (ALTER DEFAULT PRIVILEGES);
#   • the vault can read neither `web` nor `gateway` — it is byte custody and nothing else;
#   • the gateway can READ the web rows every proxied call resolves against and write none of
#     them; the web app can read the gateway's METADATA (credential rows, observed tools, usage)
#     and is refused the ciphertext, the workspace keys and the in-flight authorize rows.
# That last refusal is the whole custody claim of the gateway, so it is probed table by table.
#
# This script provisions a scratch database with the real initdb (scripts/compose-init-db.sh),
# runs all three real lineages each AS ITS OWN ROLE, then probes every rule by LOGGING IN as
# each role — never SET ROLE, which does not adopt the role's search_path and would prove
# nothing about a real connection.
#
# Usage: PGHOST/PGPORT/PGUSER/PGPASSWORD (a superuser) must reach a Postgres; the script
# creates and drops its own database. `--self-test` additionally provisions a deliberately
# broken variant (one grant removed per lane) and asserts the probes FAIL — the gate's own
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

run_lineage() { # role password directory — every migration file, in order, as the owning role
  # A lineage replayed from 0000 is loud with NOTICEs (IF NOT EXISTS, cascades) that say nothing
  # about grants; warnings and errors still come through.
  for file in "$3"/[0-9]*.sql; do
    PGPASSWORD="$2" PGOPTIONS='--client-min-messages=warning' \
      psql -X -q -v ON_ERROR_STOP=1 -U "$1" -d "$db" -f "$file" >/dev/null
  done
}

provision() {
  # $1 = 'real' | 'broken' (broken removes one grant per lane — the self-test's red path)
  psql -X -q -v ON_ERROR_STOP=1 -d postgres -c "CREATE DATABASE \"$db\""
  POSTGRES_USER="$PGUSER" POSTGRES_DB="$db" \
  TOPOS_PLANE_DB_PASSWORD=plane TOPOS_WEB_DB_PASSWORD=web TOPOS_GATEWAY_DB_PASSWORD=gateway \
    sh "$repo_root/scripts/compose-init-db.sh" >/dev/null
  # The initdb's role creation is idempotent-guarded; on a cluster where the roles pre-exist
  # (CI reruns, dev boxes) their passwords may differ — pin the probe passwords here.
  psql -X -q -v ON_ERROR_STOP=1 -d "$db" \
    -c "ALTER ROLE topos_plane PASSWORD 'plane'; ALTER ROLE topos_web PASSWORD 'web'; ALTER ROLE topos_gateway PASSWORD 'gateway'" >/dev/null
  if [ "$1" = "broken" ]; then
    # Break the app's read of FUTURE custody tables. Removed before the lineages run, because
    # default privileges only decide what a table is born with.
    psql -X -q -v ON_ERROR_STOP=1 -d "$db" \
      -c "ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane REVOKE SELECT ON TABLES FROM topos_web" >/dev/null
  fi
  # The real lineages, each applied AS ITS OWN role (exactly how the applications boot). The web
  # lineage runs WHOLE — the gateway reads tables that arrived long after 0000, so a first-file
  # shortcut would prove the boundary against a schema nothing runs.
  PGPASSWORD=plane psql -X -q -v ON_ERROR_STOP=1 -U topos_plane -d "$db" \
    -f "$repo_root/crates/plane-store/migrations/0001_custody.sql" >/dev/null
  run_lineage topos_web web "$repo_root/web/drizzle"
  run_lineage topos_gateway gateway "$repo_root/gateway/migrations"
  # A table from a "future" vault migration — the default-privileges proof target.
  PGPASSWORD=plane psql -X -q -v ON_ERROR_STOP=1 -U topos_plane -d "$db" \
    -c "CREATE TABLE future_custody_fact (id text PRIMARY KEY)" >/dev/null
  if [ "$1" = "broken" ]; then
    # Break the app's read of gateway metadata. Removed AFTER the lineage, because this grant is
    # written by the gateway's own migration rather than by initdb.
    psql -X -q -v ON_ERROR_STOP=1 -d "$db" \
      -c "REVOKE SELECT ON gateway.credential FROM topos_web" >/dev/null
  fi
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

  # The vault is blind to web AND to gateway — it cannot even USAGE either schema.
  expect_denied topos_plane plane "SELECT count(*) FROM web.\"user\"" "plane SELECT of web.user refused"
  expect_denied topos_plane plane "SELECT count(*) FROM web.seat" "plane SELECT of web.seat refused"
  expect_denied topos_plane plane "INSERT INTO web.audit_event (workspace_id, actor_display, kind, outcome) VALUES ('w','x','k','ok')" \
    "plane INSERT into web refused"
  expect_denied topos_plane plane "SELECT count(*) FROM gateway.credential" \
    "plane SELECT of gateway.credential refused"

  # ── the gateway lane ──────────────────────────────────────────────────────────────────────
  sp=$(as_role topos_gateway gateway "SHOW search_path")
  [ "$sp" = "gateway, web" ] || fail "topos_gateway search_path is '$sp', wanted 'gateway, web'"

  # The app renders sign-in state, the tools checklist and the usage page from these three.
  expect_ok topos_web web "SELECT count(*) FROM gateway.credential" "web reads gateway.credential"
  expect_ok topos_web web "SELECT count(*) FROM gateway.observed_tool" "web reads gateway.observed_tool"
  expect_ok topos_web web "SELECT count(*) FROM gateway.usage_event" "web reads gateway.usage_event"

  # THE CUSTODY CLAIM: the ciphertext, the workspace keys and the in-flight authorize rows are
  # granted to nobody. If any of these three ever answers, the gateway holds nothing the app
  # could not read for itself.
  expect_denied topos_web web "SELECT count(*) FROM gateway.credential_secret" \
    "web SELECT of gateway.credential_secret refused"
  expect_denied topos_web web "SELECT count(*) FROM gateway.workspace_key" \
    "web SELECT of gateway.workspace_key refused"
  expect_denied topos_web web "SELECT count(*) FROM gateway.oauth_flow" \
    "web SELECT of gateway.oauth_flow refused"

  # Even where the app reads, it may not write — and it may not add tables of its own.
  expect_denied topos_web web "DELETE FROM gateway.credential" "web DELETE from gateway refused"
  expect_denied topos_web web "CREATE TABLE gateway.intruder (id text)" \
    "web CREATE IN gateway refused"

  # The gateway reads exactly what a proxied call resolves against, and nothing of custody.
  for table in cli_session seat workspace bundle bundle_mcp mcp_server mcp_server_revision \
    mcp_tool_policy mcp_tool_selection; do
    expect_ok topos_gateway gateway "SELECT count(*) FROM web.$table" "gateway reads web.$table"
  done
  expect_denied topos_gateway gateway "SELECT count(*) FROM plane.version" \
    "gateway SELECT of plane.version refused"

  # It writes none of them: a policy change, a session revocation, a connection are all the
  # app's acts, and the gateway only ever obeys the rows it finds.
  expect_denied topos_gateway gateway "UPDATE web.cli_session SET status = 'active'" \
    "gateway UPDATE of web.cli_session refused"
  expect_denied topos_gateway gateway "DELETE FROM web.mcp_tool_selection" \
    "gateway DELETE from web.mcp_tool_selection refused"
  expect_denied topos_gateway gateway "CREATE TABLE web.intruder (id text)" \
    "gateway CREATE IN web refused"

  # Each application owns and writes its own schema.
  expect_ok topos_plane plane \
    "INSERT INTO plane.version (workspace_id,bundle_id,version_id,commit_id,author_display) VALUES ('w','b','v','c','x')" \
    "plane writes its own schema"
  expect_ok topos_web web "INSERT INTO web.workspace (id, name, display_name, claim_code_sha256) VALUES ('w1','t','T', sha256('x'::bytea))" \
    "web writes its own schema"
  expect_ok topos_gateway gateway \
    "INSERT INTO gateway.workspace_key (workspace_id, wrapped_key) VALUES ('w1', '\\x00'::bytea)" \
    "gateway writes its own schema"
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
  # The same red path for the gateway's half: with its metadata grant revoked, the probe that
  # asserts the app CAN read sign-in state must go red.
  if out=$(as_role topos_web web "SELECT count(*) FROM gateway.credential"); then
    fail "self-test: broken provisioning still let web read gateway.credential — the gate cannot fire"
  fi
  cleanup
  echo "check-db-grants: self-test OK (the gate fires on a broken grant shape in both lanes)"
fi
