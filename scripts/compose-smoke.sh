#!/usr/bin/env bash
# Compose smoke test: the web app is the ONE public surface, the vault is internal-only, and a
# FRESH VOLUME boots the whole first-run story. A green run proves the images build, the real
# initdb provisions both roles/schemas/grants, both boot-time migration lineages run (the app's
# drizzle lineage at first request, the vault's sqlx lineage at its boot), the vault publishes
# NO host port, the constant protocol card answers on any path, the boot ceremony prints the
# setup line, and the CLAIM CEREMONY seats a first owner whose signed-in dashboard reads
# custody state across the schema boundary — the cross-lane SELECT grant, proven on the real
# deploy artifact, not a test copy.
#
#   ./scripts/compose-smoke.sh
#
# An ephemeral project name + `down -v` keep it from colliding with a real deployment's volumes.
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="topos-smoke-$$"
# Preset the setup code (the CI/IaC hatch) so the smoke can drive the claim page itself.
export TOPOS_SETUP_CODE="smoke-setup-code-$$-0123456789abcdef"
export TOPOS_WORKSPACE_NAME="smoke-team"
compose() { docker compose -p "$PROJECT" "$@"; }
cleanup() { compose down -v --remove-orphans >/dev/null 2>&1 || true; rm -f "$COOKIES"; }
COOKIES="$(mktemp)"
trap cleanup EXIT

echo "== building + starting the stack (project $PROJECT) =="
compose up -d --build

# ── the vault publishes NO host port ─────────────────────────────────────────────────────────────────
echo "== asserting the vault publishes no host port =="
plane_cid="$(compose ps -q plane)"
[ -n "$plane_cid" ] || { echo "FAIL: no plane container found"; exit 1; }
ports_json="$(docker inspect "$plane_cid" --format '{{json .NetworkSettings.Ports}}')"
echo "plane port map: $ports_json"
if printf '%s' "$ports_json" | grep -q 'HostPort'; then
  echo "FAIL: the vault publishes a host port — it must be internal-only"
  exit 1
fi
echo "PASS: the vault exposes no host port (internal-only)."

# ── the app is up (own-database liveness) ────────────────────────────────────────────────────────────
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

# ── a browser DOCUMENT render of the root (boot migrator + the setup ceremony) ───────────────────────
# The first document request runs the app's drizzle lineage and the boot ceremony (workspace +
# default channel + the printed claim link). Retried: a fresh-volume boot may race the vault's
# first migrate — recovery, not a restart, must get the page green.
echo "== rendering / as a browser document (boot migrations + setup ceremony) =="
landing=""
for _ in $(seq 1 30); do
  landing="$(curl -s -o /dev/null -w '%{http_code}' -H 'Accept: text/html' http://localhost:3000/ || true)"
  [ "$landing" = "200" ] && break
  sleep 2
done
if [ "$landing" != "200" ]; then
  echo "FAIL: / never rendered 200 as a document (last: '$landing')"
  compose logs --no-color web | tail -60
  exit 1
fi
echo "PASS: the landing document renders."

# ── the boot ceremony printed the ONE setup line ─────────────────────────────────────────────────────
echo "== asserting the setup line was printed to the app logs =="
if ! compose logs --no-color web | grep -q 'Finish setup:'; then
  echo "FAIL: the app logs carry no 'Finish setup:' line"
  compose logs --no-color web | tail -60
  exit 1
fi
echo "PASS: the setup line is in the logs."

# ── the constant protocol card, byte-identical on every path ─────────────────────────────────────────
# Two non-browser faces: a JSON card (the discriminant + the API base a client re-roots onto)
# for Accept: application/json, and a markdown card for a bare fetch. Both must be constant
# across paths.
echo "== fetching the protocol card (non-browser faces) =="
card_json="$(curl -s -H 'Accept: application/json' http://localhost:3000/)"
card_json_deep="$(curl -s -H 'Accept: application/json' http://localhost:3000/some/deep/path)"
printf '%s' "$card_json" | grep -q 'topos-protocol-card' || { echo "FAIL: JSON card missing marker: $card_json"; exit 1; }
printf '%s' "$card_json" | grep -q '"api_base_url":"http://localhost:3000/api"' \
  || { echo "FAIL: card api_base_url wrong: $card_json"; exit 1; }
[ "$card_json" = "$card_json_deep" ] || { echo "FAIL: JSON card differs between / and a deep path"; exit 1; }
card_md_root="$(curl -s http://localhost:3000/)"
card_md_deep="$(curl -s http://localhost:3000/some/deep/path)"
printf '%s' "$card_md_root" | grep -q 'topos follow' || { echo "FAIL: markdown card missing the follow teaching"; exit 1; }
[ "$card_md_root" = "$card_md_deep" ] || { echo "FAIL: markdown card differs between / and a deep path"; exit 1; }
echo "PASS: both card faces answer, byte-identical, with the app-rooted api base."

# ── the device lane answers the uniform miss on an unknown credential ────────────────────────────────
echo "== probing the device lane with an unknown bearer =="
lane="$(curl -s -o /dev/null -w '%{http_code}' \
  -H 'Authorization: Bearer smoke-unknown-credential' \
  http://localhost:3000/api/v1/workspaces/ws-smoke-unknown/delivery || true)"
if [ "$lane" != "404" ]; then
  echo "FAIL: unknown-credential delivery answered '$lane', wanted the uniform 404"
  exit 1
fi
echo "PASS: the device lane answers the uniform 404."

# ── THE CLAIM CEREMONY: the preset code seats a first owner ──────────────────────────────────────────
echo "== claiming the workspace with the preset setup code =="
claim_page="$(curl -s -o /dev/null -w '%{http_code}' -H 'Accept: text/html' \
  "http://localhost:3000/claim?code=${TOPOS_SETUP_CODE}" || true)"
[ "$claim_page" = "200" ] || { echo "FAIL: the live claim page answered '$claim_page'"; exit 1; }
claim_post="$(curl -s -o /dev/null -w '%{http_code}' -c "$COOKIES" -b "$COOKIES" -L \
  -H 'Accept: text/html' \
  --data-urlencode "code=${TOPOS_SETUP_CODE}" \
  --data-urlencode "name=Smoke Owner" \
  --data-urlencode "email=owner@smoke.example" \
  --data-urlencode "password=smoke-owner-password-1" \
  http://localhost:3000/claim || true)"
if [ "$claim_post" != "200" ]; then
  echo "FAIL: the claim submit chain ended '$claim_post', wanted 200 after the redirect"
  compose logs --no-color web | tail -40
  exit 1
fi
echo "PASS: the claim consumed and the owner is signed in."

# A consumed code is DEAD: the same link now answers the uniform miss.
dead="$(curl -s -o /dev/null -w '%{http_code}' -H 'Accept: text/html' \
  "http://localhost:3000/claim?code=${TOPOS_SETUP_CODE}" || true)"
[ "$dead" = "404" ] || { echo "FAIL: a consumed claim code still answers '$dead'"; exit 1; }
echo "PASS: the consumed code is dead (uniform miss)."

# ── the signed-in surface reads CUSTODY STATE across the schema boundary ─────────────────────────────
# The dashboard's catalog join reads the vault's plane tables read-only — this is the
# cross-lane SELECT grant (and the vault's own migration lineage) proven through the real
# stack. Retried for the same fresh-volume race as the first render.
echo "== loading the signed-in workspace surface (cross-schema custody reads) =="
dash=""
for _ in $(seq 1 15); do
  dash="$(curl -s -o /dev/null -w '%{http_code}' -b "$COOKIES" -L -H 'Accept: text/html' \
    http://localhost:3000/workspaces || true)"
  [ "$dash" = "200" ] && break
  sleep 2
done
if [ "$dash" != "200" ]; then
  echo "FAIL: the signed-in workspace surface answered '$dash'"
  compose logs --no-color web | tail -60
  exit 1
fi
echo "PASS: the signed-in surface renders over cross-schema reads."

echo "== compose smoke: ALL GREEN =="
