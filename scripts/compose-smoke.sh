#!/usr/bin/env bash
# Compose smoke test: bring the self-host stack up, prove the plane serves a DATABASE-BACKED request, tear
# it down. A green run means the image builds, the plane starts, connects to Postgres, migrates the schema,
# and answers — the minimum "a basic op succeeds" signal for the self-host packaging.
#
#   ./scripts/compose-smoke.sh
#
# An ephemeral project name + `down -v` keep it from colliding with a real deployment's volumes.
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="topos-smoke-$$"
compose() { docker compose -p "$PROJECT" "$@"; }
cleanup() { compose down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "== building + starting the stack (project $PROJECT) =="
compose up -d --build

# `GET /v1/current/<token>` resolves the read token in Postgres, so an unknown one returning 404 (not a 500
# or a connection error) proves the plane is up, migrated, and querying the database.
PROBE="http://localhost:8787/v1/current/rt_smoke_definitely_unknown"
echo "== probing a database-backed read ($PROBE must 404) =="
code=""
for _ in $(seq 1 60); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "$PROBE" || true)"
  [ "$code" = "404" ] && break
  sleep 1
done

if [ "$code" = "404" ]; then
  echo "PASS: an unknown read token 404'd — the plane is up, migrated, and querying Postgres."
  exit 0
fi
echo "FAIL: expected 404 from $PROBE, got '$code'"
compose logs --no-color plane | tail -40
exit 1
