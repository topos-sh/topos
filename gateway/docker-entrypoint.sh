#!/bin/sh
# The gateway image's entrypoint: make sure a master key exists, then hand off to the service.
#
# The master key wraps every workspace's data key, which in turn encrypts every stored upstream
# credential — so the key file is not configuration, it is CUSTODY. It lives on a volume rather
# than in the image or the environment, and it is minted HERE, on first boot, because a
# deployment whose operator must first generate a 32-byte file by hand is a deployment that
# starts with a placeholder in it.
#
# Minting is a first-boot-only act: a key file that already exists is never touched, never
# rotated, never regenerated. Lose the volume and the ciphertext in the database becomes
# undecryptable — every sign-in has to be made again (the rows are metadata; nothing else is
# lost). Back the volume up with the database.
set -eu

key="${GATEWAY_MASTER_KEY_FILE:?GATEWAY_MASTER_KEY_FILE must be set (the image defaults it)}"

if [ ! -e "$key" ]; then
  mkdir -p "$(dirname "$key")"
  # 0600 from birth: the umask covers the window between create and chmod.
  (umask 077 && head -c 32 /dev/urandom >"$key")
  chmod 600 "$key"
  echo "gateway: minted a new master key at $key — back up this volume with the database, or every stored sign-in becomes unreadable" >&2
fi

exec "$@"
