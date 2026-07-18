#!/bin/sh
# mint-release-key.sh — the topos release-signing KEY CEREMONY (a maintainer runs it ONCE, offline).
#
# Mints a minisign keypair for release signing and prints EXACTLY what to paste where:
#   1. the Rust constant  (bins/topos/src/ops/self_update.rs — RELEASE_PUBKEY)
#   2. the installer knob (scripts/install.sh — MINISIGN_PUBKEY)
#   3. the GitHub Actions secret (MINISIGN_SECRET_KEY) that arms release signing
#   4. rotation notes
#
# It writes the keypair ONLY into the directory you name (default: a fresh
# ./topos-release-key.<date>/ next to wherever you run it, created 0700). It never uploads
# anything, never touches the repo, and needs no network — safe to run on an offline machine.
#
# The secret key is generated UNENCRYPTED (`minisign -G -W`) because CI must sign
# non-interactively; its protection is (a) the GitHub secret store and (b) your offline copy.
# Treat the whole output directory as a secret. Do NOT commit it.
#
# Usage:
#   sh scripts/mint-release-key.sh [<output-dir>]

set -eu

say() { printf '%s\n' "$*"; }
err() { printf '%s\n' "$*" >&2; }
die() { err "ERROR: $*"; exit 1; }

command -v minisign >/dev/null 2>&1 \
  || die "minisign is not installed (macOS: brew install minisign; Debian/Ubuntu: apt install minisign)"

OUT_DIR="${1:-topos-release-key.$(date +%Y%m%d)}"
[ -e "$OUT_DIR" ] && die "$OUT_DIR already exists — refusing to touch an existing key directory. Name a fresh one: $0 <dir>"

# 0700 dir + 0600 files from creation — no readable window for the secret key.
umask 077
mkdir -p "$OUT_DIR"

minisign -G -W -p "$OUT_DIR/minisign.pub" -s "$OUT_DIR/minisign.key"

# A minisign.pub is two lines: an untrusted comment, then the base64 key — the base64 line is what
# both the Rust constant and the installer variable carry.
PUB_B64="$(sed -n '2p' "$OUT_DIR/minisign.pub")"
[ -n "$PUB_B64" ] || die "could not read the public key line out of $OUT_DIR/minisign.pub"

cat <<EOF

Release-signing keypair minted:
  public key:  $OUT_DIR/minisign.pub
  SECRET key:  $OUT_DIR/minisign.key   (UNENCRYPTED — treat the whole directory as a secret)

Flip the two committed files in ONE change, and set one secret:

1. bins/topos/src/ops/self_update.rs — make the compiled-in key mandatory for self-update:

     pub(crate) const RELEASE_PUBKEY: Option<&str> = Some("$PUB_B64");

2. scripts/install.sh — the installer's key:

     MINISIGN_PUBKEY="$PUB_B64"

3. GitHub Actions — arm release signing (the secret is the whole secret-key FILE, both lines):

     gh secret set MINISIGN_SECRET_KEY --repo topos-sh/topos < "$OUT_DIR/minisign.key"

Then:
  - run \`cargo test -p topos\` — a paste guard validates the constant decodes as a minisign key;
  - commit the two file changes TOGETHER (the next release is signed, and every binary built from
    that commit refuses an unsigned or tampered self-update);
  - keep an OFFLINE copy of $OUT_DIR (a password manager or an encrypted drive), then remove any
    working copy. Never commit it; never paste minisign.key anywhere but the GitHub secret.

Rotation (only if the key must ever change):
  - mint a NEW keypair with this script (a fresh directory);
  - ship ONE transitional release whose binary embeds the NEW public key but is still SIGNED with
    the OLD secret key (update the Rust constant + install.sh; leave MINISIGN_SECRET_KEY as-is):
    existing installs verify it with their old compiled-in key and receive the new one;
  - then update MINISIGN_SECRET_KEY to the new key for every later release;
  - installs that skipped the transitional release re-run the installer (its embedded key ships
    with each release) or verify manually via SHA256SUMS + \`gh attestation verify\`.
EOF
