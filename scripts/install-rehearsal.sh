#!/usr/bin/env bash
# install-rehearsal.sh — prove scripts/install.sh end-to-end, locally, before any release exists.
#
# What it proves (install.sh runs UNMODIFIED — TOPOS_INSTALL_BASE_URL is the seam, pointed at a
# local busybox httpd that serves the exact GitHub release URL layout: /download/<tag>/<asset>
# and /latest/download/<asset>):
#   1. Happy path, pinned URL:  on a toolchain-free Linux container (buildpack-deps:bookworm-curl —
#      curl + CA certs + dash as /bin/sh, no compiler) the installer downloads, echoes the
#      expected/actual sha256 pair, verifies, installs atomically to ~/.local/bin, and the
#      installed `topos --version` runs.
#   2. Happy path, latest URL:  the /latest/download/<asset> shape resolves and installs too.
#   3. Refusal path:            one flipped byte inside the tarball (SHA256SUMS unchanged) makes
#      the installer print both hashes, refuse loudly, exit non-zero, and leave NO binary behind.
#
# What it cannot prove (covered elsewhere):
#   - The real GitHub 302-redirect + TLS chain: one post-push run against a real release covers it.
#   - The macOS-native path (Darwin targets, `shasum -a 256`): one documented manual run on a Mac.
#
# Inputs:
#   REHEARSAL_DIST=<dir>  reuse an existing dist tree (must already contain
#                         download/v0.0.0-rehearsal/{topos-<target>.tar.gz, SHA256SUMS}).
#                         Otherwise one is assembled from the repo's prebuilt static musl binary
#                         at target-musl/<target>/release/topos, building it in the pinned Rust
#                         container if absent. This host runs arm64 Linux containers natively,
#                         hence the aarch64 musl target.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$(pwd)"

TARGET="aarch64-unknown-linux-musl"
ASSET="topos-$TARGET.tar.gz"
TAG="v0.0.0-rehearsal"
BUILDER_IMAGE="rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663"
# Digest-pinned like the builder image: the "no toolchain" premise asserted below must not
# drift with a re-pushed tag. Refresh: docker buildx imagetools inspect buildpack-deps:bookworm-curl
CLIENT_IMAGE="buildpack-deps:bookworm-curl@sha256:66cc6f34ca3b53e5d611a39eb387e5ee6046930b5ec992d97cf2a3e694f2ffa9"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/topos-rehearsal.XXXXXX")"
NET="topos-rehearsal-net-$$"
SRV_GOOD="topos-rehearsal-good-$$"
SRV_BAD="topos-rehearsal-bad-$$"

cleanup() {
  docker rm -f "$SRV_GOOD" "$SRV_BAD" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# need <haystack> <needle> <label> — assert a transcript contains a fixed string.
need() { grep -qF -- "$2" <<<"$1" || fail "missing $3: '$2'"; }

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

# run_client <sh-script> — run one toolchain-free client container on the rehearsal network
# with install.sh mounted read-only; prints the combined transcript.
run_client() {
  docker run --rm --network "$NET" \
    -v "$REPO/scripts/install.sh":/install.sh:ro \
    "$CLIENT_IMAGE" sh -c "$1" 2>&1
}

# ---------- 1. the dist tree (the fake "GitHub releases" filesystem) -------------------

DIST="$TMP/dist"
if [[ -n "${REHEARSAL_DIST:-}" && -f "${REHEARSAL_DIST}/download/$TAG/$ASSET" && -f "${REHEARSAL_DIST}/download/$TAG/SHA256SUMS" ]]; then
  echo "== reusing existing dist: $REHEARSAL_DIST"
  mkdir -p "$DIST"
  cp -R "$REHEARSAL_DIST/." "$DIST/"
else
  BIN="$REPO/target-musl/$TARGET/release/topos"
  if [[ ! -f "$BIN" ]]; then
    echo "== no prebuilt musl binary at $BIN — building in the pinned container (slow, one-off)"
    # Order matters inside the container: the repo pins its toolchain (rust-toolchain.toml),
    # which can differ from the image default. Sync the pinned toolchain FIRST (from the repo
    # dir, so the override applies), THEN add the musl target — otherwise the musl std lands
    # on the wrong toolchain and the build fails with a missing-core (E0463) error.
    docker run --rm \
      -v "$REPO":/w -w /w \
      -v topos-rehearsal-cargo-registry:/usr/local/cargo/registry \
      -e "CC_aarch64_unknown_linux_musl=musl-gcc" \
      -e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc" \
      -e "CARGO_TARGET_DIR=/w/target-musl" \
      "$BUILDER_IMAGE" \
      bash -ceu '
        apt-get update -qq && apt-get install -y -qq musl-tools >/dev/null
        rustup toolchain install >/dev/null 2>&1 || rustup show >/dev/null
        rustup target add aarch64-unknown-linux-musl
        cargo build --release -p topos --target aarch64-unknown-linux-musl
      '
    [[ -f "$BIN" ]] || fail "the container build produced no binary at $BIN"
  fi

  echo "== assembling dist tree from $BIN"
  PKG="$TMP/pkg/topos-$TARGET"
  mkdir -p "$PKG" "$DIST/download/$TAG"
  cp "$BIN" "$PKG/topos"
  cp "$REPO/LICENSE" "$PKG/LICENSE"
  # --no-xattrs: a real release tarball is built on Linux CI and carries no macOS/docker
  # extended attributes; strip them here too so the client-side tar stays warning-free.
  tar --no-xattrs -czf "$DIST/download/$TAG/$ASSET" -C "$TMP/pkg" "topos-$TARGET"
  (cd "$DIST/download/$TAG" && sha256 "$ASSET" > SHA256SUMS)
fi

# latest/ mirrors the pinned dir (a copy, not a symlink — no symlink-follow surprises inside
# the httpd container) so the /latest/download/<asset> URL shape gets exercised too.
rm -rf "$DIST/latest"
mkdir -p "$DIST/latest"
cp -R "$DIST/download/$TAG" "$DIST/latest/download"

# ---------- 2. the corrupted twin (refusal-path input) ---------------------------------

echo "== corrupting a copy of the dist: one flipped byte inside the tarball, SHA256SUMS unchanged"
BAD="$TMP/dist-bad"
mkdir -p "$BAD"
cp -R "$DIST/." "$BAD/"
TARBALL="$BAD/download/$TAG/$ASSET"
size=$(wc -c < "$TARBALL")
off=$((size / 2))
cur=$(dd if="$TARBALL" bs=1 skip="$off" count=1 2>/dev/null | od -An -tu1 | tr -d ' ')
new=$(((cur + 1) % 256))
printf '%b' "$(printf '\\0%03o' "$new")" |
  dd of="$TARBALL" bs=1 seek="$off" count=1 conv=notrunc 2>/dev/null
echo "   byte at offset $off: $cur -> $new"

# ---------- 3. the origins (busybox httpd, one per dist) -------------------------------

echo "== starting the origins on network $NET"
docker network create "$NET" >/dev/null
docker run -d --rm --name "$SRV_GOOD" --network "$NET" \
  -v "$DIST":/www:ro busybox httpd -f -p 80 -h /www >/dev/null
docker run -d --rm --name "$SRV_BAD" --network "$NET" \
  -v "$BAD":/www:ro busybox httpd -f -p 80 -h /www >/dev/null

# ---------- 4. happy path, pinned URL ---------------------------------------------------

# Assert the "toolchain-free" premise instead of trusting the image name: the clean-machine
# claim is only proven if the client container really has no compiler / Node / Python / git.
echo
echo "== asserting the client container is toolchain-free"
# shellcheck disable=SC2016  # the single quotes are the point: $t expands in the container, not here
TOOLCHECK_OUT="$(run_client 'for t in cc gcc g++ make git python3 python node npm rustc cargo; do command -v "$t" >/dev/null 2>&1 && { echo "TOOLCHAIN-PRESENT: $t"; exit 42; }; done; echo NO-TOOLCHAIN')" ||
  { echo "$TOOLCHECK_OUT"; fail "the client container carries a toolchain — the clean-machine premise is broken"; }
need "$TOOLCHECK_OUT" "NO-TOOLCHAIN" "the toolchain-free assert"
pass "client container has no compiler / Node / Python / git"

echo
echo "== happy path (pinned: /download/$TAG/$ASSET)"
HAPPY_OUT="$(run_client "TOPOS_INSTALL_BASE_URL=http://$SRV_GOOD TOPOS_VERSION=$TAG sh /install.sh && \"\$HOME/.local/bin/topos\" --version")" ||
  { echo "$HAPPY_OUT"; fail "the pinned happy-path install exited non-zero"; }
echo "$HAPPY_OUT"
need "$HAPPY_OUT" "expected sha256:" "the expected-hash echo"
need "$HAPPY_OUT" "actual   sha256:" "the actual-hash echo"
need "$HAPPY_OUT" "OK: checksums match." "the verification OK line"
need "$HAPPY_OUT" "WARNING: downloading over a non-HTTPS base URL" "the non-HTTPS warning"
grep -Eq 'topos [0-9]+\.[0-9]+\.[0-9]+' <<<"$HAPPY_OUT" || fail "no 'topos <semver>' version line"
pass "pinned-URL install verified, installed, and executed"

# ---------- 5. happy path, latest URL ----------------------------------------------------

echo
echo "== happy path (latest: /latest/download/$ASSET)"
LATEST_OUT="$(run_client "TOPOS_INSTALL_BASE_URL=http://$SRV_GOOD sh /install.sh && \"\$HOME/.local/bin/topos\" --version")" ||
  { echo "$LATEST_OUT"; fail "the latest-URL happy-path install exited non-zero"; }
need "$LATEST_OUT" "OK: checksums match." "the verification OK line (latest URL)"
grep -Eq 'topos [0-9]+\.[0-9]+\.[0-9]+' <<<"$LATEST_OUT" || fail "no 'topos <semver>' version line (latest URL)"
pass "latest-URL install verified, installed, and executed"

# ---------- 6. refusal path --------------------------------------------------------------

echo
echo "== refusal path (corrupted tarball, pinned URL)"
# All three checks live in ONE container invocation: the non-zero installer exit, the refusal
# wording, and — in the same filesystem the installer just touched — that no binary exists.
REFUSE_OUT="$(run_client "
  set +e
  TOPOS_INSTALL_BASE_URL=http://$SRV_BAD TOPOS_VERSION=$TAG sh /install.sh
  rc=\$?
  echo \"installer-exit-code: \$rc\"
  if [ \"\$rc\" -eq 0 ]; then echo 'REHEARSAL-BUG: installer exited 0 on corrupted bytes'; exit 40; fi
  if [ -e \"\$HOME/.local/bin/topos\" ]; then echo 'REHEARSAL-BUG: a binary was left behind after refusal'; exit 41; fi
  echo 'REFUSAL-CONFIRMED: non-zero exit, no binary installed'
")" || { echo "$REFUSE_OUT"; fail "the refusal-path container run itself failed"; }
echo "$REFUSE_OUT"
need "$REFUSE_OUT" "expected sha256:" "the expected-hash echo (refusal run)"
need "$REFUSE_OUT" "actual   sha256:" "the actual-hash echo (refusal run)"
need "$REFUSE_OUT" "sha256 mismatch for $ASSET" "the mismatch headline"
need "$REFUSE_OUT" "Refusing to install." "the refusal line"
need "$REFUSE_OUT" "REFUSAL-CONFIRMED" "the in-container confirmation"
pass "corrupted tarball refused: hashes echoed, non-zero exit, nothing installed"

echo
echo "REHEARSAL GREEN: pinned install + latest install + corruption refusal all behaved."
