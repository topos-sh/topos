#!/usr/bin/env bash
# Compose smoke test for the POST-CUTOVER shape: the web app is the ONE public surface, the plane is
# internal-only. A green run proves the images build, the app comes up and talks to its database, the
# plane has NO published port yet is reachable THROUGH the app (forwarded to a DB-backed read), the
# constant protocol card answers on any path, and first boot hardened the plane's enrollment secret.
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

# ── (b) the plane publishes NO host port ─────────────────────────────────────────────────────────────
# The whole point of the cutover: only the app is reachable. Inspect the plane container's port map and
# require that no binding carries a HostPort (an unpublished container port maps to `null`).
echo "== asserting the plane publishes no host port =="
plane_cid="$(compose ps -q plane)"
[ -n "$plane_cid" ] || { echo "FAIL: no plane container found"; exit 1; }
ports_json="$(docker inspect "$plane_cid" --format '{{json .NetworkSettings.Ports}}')"
echo "plane port map: $ports_json"
if printf '%s' "$ports_json" | grep -q 'HostPort'; then
  echo "FAIL: the plane publishes a host port — it must be internal-only"
  exit 1
fi
echo "PASS: the plane exposes no host port (internal-only)."

# ── (f) the app is up (own-database liveness) ────────────────────────────────────────────────────────
echo "== waiting for the web app's /healthz (own-database liveness) =="
health=""
for _ in $(seq 1 90); do
  health="$(curl -s -o /dev/null -w '%{http_code}' http://localhost:3000/healthz || true)"
  [ "$health" = "200" ] && break
  sleep 1
done
if [ "$health" != "200" ]; then
  echo "FAIL: /healthz never returned 200 (last: '$health')"
  compose logs --no-color web | tail -40
  exit 1
fi
echo "PASS: /healthz 200 — the app is up and its database is reachable."

# ── (c) a DATABASE-BACKED read, forwarded THROUGH the app to the plane ───────────────────────────────
# The app fronts `/api`, forwarding `/api/v1/*` to the internal plane. A device-lane catalog read
# resolves the presented workspace credential in Postgres before answering, so an unknown credential
# returning the uniform 404 (not 500, not a connection error, not the protocol card) proves the whole
# path: app up → forwarding to the plane → plane migrated → DB-backed resolve.
PROBE="http://localhost:3000/api/v1/workspaces/ws-smoke-unknown/skills"
echo "== probing a forwarded database-backed read ($PROBE with an unknown credential must 404) =="
code=""
for _ in $(seq 1 90); do
  code="$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer smoke-definitely-unknown' "$PROBE" || true)"
  [ "$code" = "404" ] && break
  sleep 1
done
if [ "$code" != "404" ]; then
  echo "FAIL: expected 404 from $PROBE, got '$code'"
  compose logs --no-color web plane | tail -60
  exit 1
fi
echo "PASS: an unknown workspace credential 404'd through the app — forwarding + plane + Postgres all live."

# ── (d) the constant protocol card on any path ───────────────────────────────────────────────────────
# A non-browser JSON fetch of ANY path gets the byte-constant card (no existence oracle): the discriminant
# plus the follow API base the client re-roots onto — the app origin + /api once the app fronts the API.
echo "== probing the constant protocol card (any path, JSON) =="
card="$(curl -s -H 'Accept: application/json' http://localhost:3000/anything-at-all || true)"
echo "card: $card"
if ! printf '%s' "$card" | grep -q 'topos-protocol-card'; then
  echo "FAIL: the protocol card discriminant is missing from the response"
  exit 1
fi
if ! printf '%s' "$card" | grep -q '"api_base_url":"http://localhost:3000/api"'; then
  echo "FAIL: the card's api_base_url is not the app's /api follow base"
  exit 1
fi
echo "PASS: the constant protocol card answers with the app's /api follow base."

# ── (e) first-boot secret hardening (mode 0600, owner uid 10001), via the internal-only plane ────────
# The 404 above proved first boot completed, so the enrollment secret exists — now assert the stated
# custody posture instead of trusting the docs. `compose exec` reaches the plane even with no host port.
# GNU-stat syntax: fine on the debian-slim runtime; revisit if the base image ever changes family.
echo "== verifying the plane's first-boot secret hardening (mode 0600, owner uid 10001) =="
keys="$(compose exec -T plane stat -c '%a %u %n' /data/enroll.key)"
echo "$keys"
printf '%s' "$keys" | grep -q '^600 10001 /data/enroll.key$' || { echo "FAIL: enroll.key is not 0600 uid 10001"; exit 1; }
echo "PASS: enrollment secret is 0600, owned by the unprivileged user."

echo "== ALL CHECKS PASSED =="
