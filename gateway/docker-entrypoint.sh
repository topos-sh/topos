#!/bin/sh
# The gateway image's entrypoint: make sure a master key exists, then hand off to the service.
#
# ONLY for the `file` key backend (the default). Under GATEWAY_KEY_BACKEND=gcp-kms/aws-kms the
# master key lives in the key service and minting a file here would leave a 32-byte decoy on the
# volume that protects nothing. A file→KMS migration still needs the OLD key file, so it stays
# mounted until the re-wrap has run — mounted, never minted.
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

if [ "${GATEWAY_KEY_BACKEND:-file}" = "file" ]; then
  key="${GATEWAY_MASTER_KEY_FILE:?GATEWAY_MASTER_KEY_FILE must be set (the image defaults it)}"

  if [ ! -e "$key" ]; then
    mkdir -p "$(dirname "$key")"
    # 0600 from birth: the umask covers the window between create and chmod.
    (umask 077 && head -c 32 /dev/urandom >"$key")
    chmod 600 "$key"
    echo "gateway: minted a new master key at $key — back up this volume with the database, or every stored sign-in becomes unreadable" >&2
  fi
fi

exec "$@"
