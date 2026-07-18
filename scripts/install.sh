#!/bin/sh
# install.sh — the checksummed installer for the `topos` CLI.
#
# What it does, in order:
#   1. Detects your OS and CPU architecture (uname) and picks the matching release target.
#   2. Downloads topos-<target>.tar.gz AND SHA256SUMS from
#      https://github.com/topos-sh/topos/releases (with retries; GitHub 302s to a CDN).
#   3. When this installer embeds a release-signing public key (MINISIGN_PUBKEY below is
#      non-empty), also downloads the asset's .minisig (REQUIRED then) and verifies the
#      minisign signature — fail-closed — whenever the `minisign` tool is installed;
#      without the tool the signature step is skipped with a loud note.
#   4. Prints the EXPECTED sha256 (this asset's entry in SHA256SUMS) and the ACTUAL sha256
#      (computed locally over the downloaded bytes), and REFUSES to install unless they
#      match. Verification is never skippable — there is no flag that disables it.
#   5. Installs the binary to ~/.local/bin (no sudo): staged inside the destination
#      directory, then moved into place atomically — a half-written binary can never
#      appear on your PATH. Finally runs `topos --version` to prove it executes.
#
# Knobs (a flag wins over its environment variable):
#   TOPOS_VERSION           / --version <tag>   pin a release tag (e.g. v0.3.1); default: latest
#   TOPOS_INSTALL_DIR       / --to <dir>        install directory; default: ~/.local/bin
#   TOPOS_INSTALL_BASE_URL                      alternate download base for mirrors or
#                                               air-gapped proxies; must serve the same URL
#                                               layout: <base>/latest/download/<asset> and
#                                               <base>/download/<tag>/<asset>
#
# What the checksum proves — and what it does not:
#   SHA256SUMS is fetched over TLS from the SAME origin as the binary. A match therefore
#   proves TRANSIT integrity — the bytes you received are the bytes the release published
#   (no corrupted download, no tampering proxy or mirror) — but NOT origin integrity: a
#   party who controls the release controls both files. For an origin-independent check,
#   use GitHub artifact attestation (GitHub CLI), which validates that the artifact was
#   built by this repository's release workflow and is Sigstore-signed:
#       gh attestation verify topos-<target>.tar.gz --repo topos-sh/topos
#
# Usage:
#   curl -fsSL https://github.com/topos-sh/topos/releases/latest/download/install.sh | sh
#   sh install.sh [--version <tag>] [--to <dir>]

set -eu

say() { printf '%s\n' "$*"; }
err() { printf '%s\n' "$*" >&2; }
die() { err "ERROR: $*"; exit 1; }

usage() {
  say "usage: install.sh [--version <tag>] [--to <dir>]"
  say "  --version <tag>  install a pinned release tag (default: latest)"
  say "  --to <dir>       install directory (default: ~/.local/bin)"
  say "env: TOPOS_VERSION, TOPOS_INSTALL_DIR, TOPOS_INSTALL_BASE_URL"
}

# ---------- knobs ----------------------------------------------------------------------

VERSION="${TOPOS_VERSION:-}"
INSTALL_DIR="${TOPOS_INSTALL_DIR:-$HOME/.local/bin}"
BASE_URL="${TOPOS_INSTALL_BASE_URL:-https://github.com/topos-sh/topos/releases}"

# The release-signing public key (minisign, the base64 line of minisign.pub). Empty in the
# pre-key-ceremony state — the checksum below is then the only verification. When non-empty
# (scripts/mint-release-key.sh prints the exact value to paste here, in the same change that
# flips the binary's compiled-in RELEASE_PUBKEY), the asset's .minisig becomes REQUIRED and is
# verified BEFORE the checksum whenever the `minisign` tool is available. Not a knob: this is
# release-time configuration, deliberately not overridable by flag or environment.
MINISIGN_PUBKEY=""

while [ $# -gt 0 ]; do
  case "$1" in
    --version)   [ $# -ge 2 ] || die "--version needs a value (a release tag, e.g. v0.3.1)"
                 VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#--version=}"; shift ;;
    --to)        [ $# -ge 2 ] || die "--to needs a value (an install directory)"
                 INSTALL_DIR="$2"; shift 2 ;;
    --to=*)      INSTALL_DIR="${1#--to=}"; shift ;;
    -h|--help)   usage; exit 0 ;;
    *)           usage >&2; die "unknown argument: $1" ;;
  esac
done

# ---------- platform detection (no fallback, no guessing) ------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"

unsupported() {
  err "ERROR: unsupported platform: $OS $ARCH"
  err "Prebuilt binaries exist for:"
  err "  macOS  arm64   (aarch64-apple-darwin)"
  err "  macOS  x86_64  (x86_64-apple-darwin)"
  err "  Linux  x86_64  (x86_64-unknown-linux-musl, static)"
  err "  Linux  arm64   (aarch64-unknown-linux-musl, static)"
  err "Or build from source with cargo: https://github.com/topos-sh/topos"
  exit 1
}

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64)        TARGET="x86_64-apple-darwin" ;;
      *)             unsupported ;;
    esac ;;
  Linux)
    # The Linux binaries are static musl builds — any distro, any libc — so the CPU
    # architecture is the only axis that matters here.
    case "$ARCH" in
      x86_64|amd64)  TARGET="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
      *)             unsupported ;;
    esac ;;
  MINGW*|MSYS*|CYGWIN*)
    err "ERROR: native Windows is not supported."
    err "Run topos inside WSL2 — the Linux x86_64 binary works there:"
    err "  https://learn.microsoft.com/windows/wsl/install"
    exit 1 ;;
  *)
    unsupported ;;
esac

ASSET="topos-$TARGET.tar.gz"

# ---------- required tools -------------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
  FETCHER=curl
elif command -v wget >/dev/null 2>&1; then
  FETCHER=wget
else
  die "neither curl nor wget found — install one of them and re-run"
fi

if command -v sha256sum >/dev/null 2>&1; then
  HASHER=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  HASHER=shasum
else
  err "ERROR: no sha256 tool found (need sha256sum or shasum)."
  err "Refusing to install an UNVERIFIED binary — checksum verification is not optional."
  exit 1
fi

# hash_file <file> — print the file's sha256 as a bare hex string.
hash_file() {
  if [ "$HASHER" = sha256sum ]; then sha256sum "$1"; else shasum -a 256 "$1"; fi | awk '{print $1}'
}

# fetch <url> <dest> — download one file; any failure returns non-zero and leaves no
# partial file behind. curl: -f makes HTTP errors fail, -L follows GitHub's 302 to the
# asset CDN, --retry rides out transient network blips.
fetch() {
  if [ "$FETCHER" = curl ]; then
    curl -fSL --retry 3 -o "$2" "$1" || { rm -f "$2"; return 1; }
  else
    wget -q -O "$2" "$1" || { rm -f "$2"; return 1; }
  fi
}

# ---------- URLs -----------------------------------------------------------------------

if [ -n "$VERSION" ]; then
  URL_DIR="$BASE_URL/download/$VERSION"
else
  URL_DIR="$BASE_URL/latest/download"
fi

case "$BASE_URL" in
  https://*) ;;
  *)
    say ""
    say "*** WARNING: downloading over a non-HTTPS base URL: ***"
    say "***   $BASE_URL"
    say "*** Only do this against a local mirror you control. ***"
    say ""
    ;;
esac

# ---------- download (to a private temp dir, always cleaned up) ------------------------

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/topos-install.XXXXXX")"
STAGE=""
cleanup() {
  rm -rf "$TMP_DIR"
  if [ -n "$STAGE" ]; then rm -f "$STAGE"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

say "target:  $TARGET"
say "release: ${VERSION:-latest}"

say "downloading: $URL_DIR/$ASSET"
if ! fetch "$URL_DIR/$ASSET" "$TMP_DIR/$ASSET"; then
  if [ -n "$VERSION" ]; then
    err "ERROR: could not download $ASSET for release '$VERSION'."
    err "That release — or its $ASSET asset — may not exist. Published releases:"
    err "  https://github.com/topos-sh/topos/releases"
  else
    err "ERROR: could not download $ASSET from $URL_DIR"
    err "Check your network, then see: https://github.com/topos-sh/topos/releases"
  fi
  exit 1
fi

say "downloading: $URL_DIR/SHA256SUMS"
if ! fetch "$URL_DIR/SHA256SUMS" "$TMP_DIR/SHA256SUMS"; then
  err "ERROR: could not download SHA256SUMS from $URL_DIR"
  err "Refusing to install an UNVERIFIED binary."
  exit 1
fi

# ---------- signature (when a release public key is embedded; mirrors the binary's own
# ---------- self-update order: signature first, then checksum) -------------------------

if [ -n "$MINISIGN_PUBKEY" ]; then
  say "downloading: $URL_DIR/$ASSET.minisig"
  if ! fetch "$URL_DIR/$ASSET.minisig" "$TMP_DIR/$ASSET.minisig"; then
    err "ERROR: could not download $ASSET.minisig from $URL_DIR"
    err "This installer embeds a release-signing public key, so a signature is REQUIRED"
    err "for every asset. Refusing to install an unsigned binary."
    exit 1
  fi
  if command -v minisign >/dev/null 2>&1; then
    if ! minisign -Vm "$TMP_DIR/$ASSET" -x "$TMP_DIR/$ASSET.minisig" -P "$MINISIGN_PUBKEY" >/dev/null 2>&1; then
      err ""
      err "ERROR: minisign signature verification FAILED for $ASSET."
      err "The downloaded bytes are NOT the bytes the release signed."
      err "Refusing to install. Nothing was installed, and the download was deleted."
      err "Possible causes:"
      err "  - a corrupted download"
      err "  - a tampering proxy or mirror between you and the release host"
      err "  - a compromised release"
      err "Please retry once; if it happens again, report it:"
      err "  https://github.com/topos-sh/topos/issues"
      exit 1
    fi
    say "OK: minisign signature verified."
  else
    # The tool is absent: skip loudly, never silently. The sha256 gate below still runs — it is
    # never skippable — and the shipped binary's own self-update enforces the compiled-in key.
    say "NOTE: minisign is not installed — skipping signature verification for this install."
    say "      (The sha256 checksum below is still enforced. To also check the signature,"
    say "      install minisign and re-run this installer.)"
  fi
fi

# ---------- verify (never skippable) ---------------------------------------------------

# The expected hash is the SHA256SUMS line whose filename — the last field, with any
# leading '*' binary-mode marker stripped — is EXACTLY this asset's name: an exact match
# anchored at the end of the line, never a substring.
EXPECTED="$(awk -v a="$ASSET" '{ f = $NF; sub(/^\*/, "", f); if (f == a) { print $1; exit } }' "$TMP_DIR/SHA256SUMS")"
if [ -z "$EXPECTED" ]; then
  err "ERROR: SHA256SUMS does not list $ASSET — refusing to install an unlisted artifact."
  exit 1
fi
ACTUAL="$(hash_file "$TMP_DIR/$ASSET")"

say "expected sha256: $EXPECTED  (from SHA256SUMS)"
say "actual   sha256: $ACTUAL  (computed locally)"

if [ "$EXPECTED" != "$ACTUAL" ]; then
  # Repeat the pair on stderr so the refusal reads self-contained even when stdout and
  # stderr are captured separately (or interleaved out of order).
  err ""
  err "ERROR: sha256 mismatch for $ASSET."
  err "  expected sha256: $EXPECTED  (from SHA256SUMS)"
  err "  actual   sha256: $ACTUAL  (computed locally)"
  err "The downloaded bytes are NOT the bytes the release published."
  err "Refusing to install. Nothing was installed, and the download was deleted."
  err "Possible causes:"
  err "  - a corrupted download"
  err "  - a tampering proxy or mirror between you and the release host"
  err "  - a compromised release"
  err "Please retry once; if it happens again, report it:"
  err "  https://github.com/topos-sh/topos/issues"
  exit 1
fi
say "OK: checksums match."

# ---------- install (staged, atomic) ---------------------------------------------------

mkdir -p "$INSTALL_DIR"

EXTRACT_DIR="$TMP_DIR/extract"
mkdir -p "$EXTRACT_DIR"
tar -xzf "$TMP_DIR/$ASSET" -C "$EXTRACT_DIR"

# The tarball holds the binary either at its top level or inside one wrapping directory.
BIN_SRC=""
if [ -f "$EXTRACT_DIR/topos" ]; then
  BIN_SRC="$EXTRACT_DIR/topos"
else
  for f in "$EXTRACT_DIR"/*/topos; do
    if [ -f "$f" ]; then BIN_SRC="$f"; break; fi
  done
fi
[ -n "$BIN_SRC" ] || die "no 'topos' binary found inside $ASSET"

# A directory squatting on the binary's path would swallow the staged rename ("mv into it")
# and produce a misleading failure — refuse up front instead.
if [ -d "$INSTALL_DIR/topos" ]; then
  die "$INSTALL_DIR/topos is a directory - refusing to install over it. Remove or rename it, or pick another directory with --to."
fi

if [ -x "$INSTALL_DIR/topos" ]; then
  OLD_VERSION="$("$INSTALL_DIR/topos" --version 2>/dev/null || true)"
  if [ -n "$OLD_VERSION" ]; then
    say "replacing existing install: $OLD_VERSION"
  else
    say "replacing existing $INSTALL_DIR/topos"
  fi
fi

# Stage inside the destination directory so the final rename stays on one filesystem and
# is atomic: a half-written binary can never appear at $INSTALL_DIR/topos, and an
# already-running old process keeps its inode.
STAGE="$INSTALL_DIR/.topos.tmp.$$"
cp "$BIN_SRC" "$STAGE"
chmod 0755 "$STAGE"

# Run the STAGED binary once, before it replaces anything — a checksum-valid but
# wrong-architecture (or otherwise broken) asset must never displace a working install.
if ! NEW_VERSION="$("$STAGE" --version)"; then
  rm -f "$STAGE"
  STAGE=""
  err "ERROR: the downloaded binary failed to execute on this machine."
  err "This usually means a wrong-architecture binary (exec format error)."
  if [ -x "$INSTALL_DIR/topos" ]; then
    err "Nothing was replaced - your existing install is untouched."
  else
    err "Nothing was installed."
  fi
  err "Report it: https://github.com/topos-sh/topos/issues"
  exit 1
fi
mv -f "$STAGE" "$INSTALL_DIR/topos"
STAGE=""
say "installed: $NEW_VERSION -> $INSTALL_DIR/topos"

# ---------- PATH hint (printed only — this script never edits your shell config) -------

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    SHELL_NAME="$(basename "${SHELL:-/bin/sh}")"
    say ""
    say "NOTE: $INSTALL_DIR is not on your PATH."
    case "$SHELL_NAME" in
      fish)
        say "Add it (fish):"
        say "  fish_add_path $INSTALL_DIR" ;;
      zsh)
        say "Add it by appending this line to ~/.zshrc:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
      bash)
        say "Add it by appending this line to ~/.bashrc:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
      *)
        say "Add it by appending this line to your shell's startup file:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
    esac
    ;;
esac

say ""
say "Optional, origin-independent check (GitHub CLI; proves the artifact was built by"
say "this repository's release workflow, Sigstore-signed):"
say "  gh attestation verify $ASSET --repo topos-sh/topos"
