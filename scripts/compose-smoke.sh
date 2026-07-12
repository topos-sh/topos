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

# The device-lane catalog read resolves the presented workspace credential in Postgres before answering,
# so an unknown credential returning the uniform 404 (not a 500, not a connection error — and not the
# constant protocol card an anonymous GET gets) proves the plane is up, migrated, and querying the database.
PROBE="http://localhost:8787/v1/workspaces/ws-smoke-unknown/skills"
echo "== probing a database-backed read ($PROBE with an unknown credential must 404) =="
code=""
for _ in $(seq 1 60); do
  code="$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer smoke-definitely-unknown' "$PROBE" || true)"
  [ "$code" = "404" ] && break
  sleep 1
done

if [ "$code" = "404" ]; then
  echo "PASS: an unknown workspace credential 404'd — the plane is up, migrated, and querying Postgres."
  # The 404 proves first boot completed, so the enrollment secret exists — now assert the stated custody
  # posture (0600, owned by the unprivileged uid) instead of trusting the docs. GNU-stat syntax: fine
  # on the debian-slim runtime; revisit if the base image ever changes family.
  echo "== verifying first-boot secret hardening (mode 0600, owner uid 10001) =="
  keys="$(compose exec -T plane stat -c '%a %u %n' /data/enroll.key)"
  echo "$keys"
  echo "$keys" | grep -q '^600 10001 /data/enroll.key$' || { echo "FAIL: enroll.key is not 0600 uid 10001"; exit 1; }
  echo "PASS: enrollment secret is 0600, owned by the unprivileged user."
  exit 0
fi
echo "FAIL: expected 404 from $PROBE, got '$code'"
compose logs --no-color plane | tail -40
exit 1
