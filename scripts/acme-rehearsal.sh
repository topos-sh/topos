#!/usr/bin/env bash
# acme-rehearsal.sh — prove the plane's built-in ACME TLS serve path (the default-off `acme` feature)
# against a REAL local ACME test server (pebble), end to end. Asserts, in order, failing loudly on each:
#
#   1. plain HTTP still serves beside the TLS listener (GET /v1/current/<unknown> → 404);
#   2. a real tls-alpn-01 issuance: the plane orders from pebble, answers the challenge inside its own
#      TLS acceptor on :8443, and then serves the API over TLS that curl VERIFIES against pebble's
#      issuing root (fetched live from the management API — pebble mints a fresh CA per boot);
#   3. persistence across restart WITHOUT re-issuance: with pebble STOPPED (the ACME server is down),
#      a plane restart re-serves the SAME certificate — fingerprint F2 == F1 is structural proof the
#      bytes came from the persistent /data/acme DirCache, not a new order.
#
# What this does NOT prove (only a real box does): public DNS, Let's Encrypt staging → production,
# CA rate limits, renewal timing, IPv4/IPv6 reachability.
#
#   ./scripts/acme-rehearsal.sh
#
# An ephemeral project name + `down -v` keep it from colliding with a real deployment's volumes.
set -euo pipefail
cd "$(dirname "$0")/.."

COMPOSE_FILE="scripts/acme-rehearsal/docker-compose.yml"
PROJECT="topos-acme-$$"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/topos-acme.XXXXXX")"

compose() { docker compose -p "$PROJECT" -f "$COMPOSE_FILE" "$@"; }
cleanup() {
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# The published ports must be free — a still-running compose-smoke holds 8787. Wait, bounded.
for port in 8787 8443 15000; do
  for i in $(seq 1 90); do
    if ! lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then break; fi
    [ "$i" = 1 ] && echo "== port $port is busy — waiting for it to free (a compose-smoke still running?)"
    [ "$i" = 90 ] && fail "port $port is still busy after 3 minutes"
    sleep 2
  done
done

echo "== building + starting the rehearsal stack (project $PROJECT) =="
compose up -d --build

# ---------- 1. plain HTTP still serves beside the TLS listener --------------------------
# Same probe as compose-smoke: an unknown read token 404ing proves up + migrated + querying Postgres.
PROBE_HTTP="http://localhost:8787/v1/current/rt_unknown"
echo "== [1/3] probing plain HTTP ($PROBE_HTTP must 404) =="
code=""
for _ in $(seq 1 60); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "$PROBE_HTTP" || true)"
  [ "$code" = "404" ] && break
  sleep 1
done
if [ "$code" != "404" ]; then
  compose logs --no-color plane | tail -40
  fail "expected 404 from $PROBE_HTTP, got '$code'"
fi
pass "plain HTTP serves beside the TLS listener (404 on an unknown read token)"

# ---------- 2. issuance + VERIFIED TLS serve ---------------------------------------------
# The ISSUING root: pebble mints a fresh CA every boot and serves it on the management API; that
# endpoint's own HTTPS cert is signed by the committed minica TEST root.
MINICA="scripts/acme-rehearsal/pebble.minica.pem"
ROOT="$TMP/pebble-issuing-root.pem"
echo "== [2/3] fetching pebble's issuing root (management API /roots/0) =="
ok=""
for _ in $(seq 1 30); do
  if curl -sf --cacert "$MINICA" https://localhost:15000/roots/0 -o "$ROOT" \
    && grep -q "BEGIN CERTIFICATE" "$ROOT"; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || fail "could not fetch pebble's issuing root from https://localhost:15000/roots/0"

PROBE_TLS="https://plane.rehearsal.test:8443/v1/current/rt_unknown"
tls_probe() {
  curl -s -o /dev/null -w '%{http_code}' \
    --resolve plane.rehearsal.test:8443:127.0.0.1 --cacert "$ROOT" "$PROBE_TLS" || true
}
fingerprint() {
  openssl s_client -connect localhost:8443 -servername plane.rehearsal.test </dev/null 2>/dev/null |
    openssl x509 -noout -fingerprint -sha256 2>/dev/null
}

echo "== [2/3] polling for issuance + a 404 over VERIFIED TLS ($PROBE_TLS) =="
code=""
for _ in $(seq 1 120); do
  code="$(tls_probe)"
  [ "$code" = "404" ] && break
  sleep 1
done
if [ "$code" != "404" ]; then
  compose logs --no-color plane pebble | tail -60
  fail "expected 404 over verified TLS from $PROBE_TLS, got '$code'"
fi
pass "tls-alpn-01 issuance completed — the plane answers over TLS verified against pebble's issuing root"

F1="$(fingerprint)"
[ -n "$F1" ] || fail "could not read the served certificate's sha256 fingerprint"
echo "   F1 = $F1"

# ---------- 3. persistence across restart WITHOUT re-issuance ---------------------------
echo "== [3/3] stopping pebble (the ACME server is now DOWN) and restarting the plane =="
compose stop pebble
compose restart plane
code=""
for _ in $(seq 1 60); do
  code="$(tls_probe)"
  [ "$code" = "404" ] && break
  sleep 1
done
if [ "$code" != "404" ]; then
  compose logs --no-color plane | tail -60
  fail "the restarted plane did not serve TLS (got '$code') — with pebble down, the /data/acme cache did not carry the certificate"
fi
F2="$(fingerprint)"
echo "   F2 = $F2"
[ "$F1" = "$F2" ] || fail "fingerprint changed across restart (F1=$F1, F2=$F2) — the served cert was not the cached one"
pass "the restarted plane re-served the SAME certificate with the ACME server down — the bytes came from the persistent DirCache"

echo
echo "REHEARSAL GREEN: plain HTTP beside TLS + a real tls-alpn-01 issuance + cached-cert persistence all behaved."
