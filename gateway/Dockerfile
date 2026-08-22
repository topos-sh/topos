# The MCP gateway — the service every agent's MCP traffic rides. It holds the workspace's upstream
# sign-ins (encrypted), attaches them per call, filters tools to the workspace's policy, and writes
# one usage row per request.
#
# TWO LISTENERS, one image: the PUBLIC one (8788) is what agents dial and what compose publishes;
# the INTERNAL one (8789) is the web app's lane and must never leave the deployment's own network.
# Only the public port is EXPOSEd — an EXPOSEd 8789 would be published by a bare `docker run -P`,
# which is exactly the mistake the two-socket split exists to make impossible.
#
# BUILD CONTEXT IS THE REPO ROOT (compose: `build: { context: ., dockerfile: gateway/Dockerfile }`),
# matching the other two images, so the paths below are written root-relative.
#
# Postgres is deliberately NOT in this image (one concern per container): point it at a database
# with `DATABASE_URL` as role `topos_gateway`. It migrates its own `gateway` schema at boot, before
# it serves.

# Pinned by the multi-arch INDEX digest (not a per-platform manifest) so one immutable pin serves
# both linux/amd64 and linux/arm64 — the same pin, deliberately, that web/Dockerfile builds on.
# Refresh: docker buildx imagetools inspect oven/bun:1.3.10 (take the top-level "Digest:"), edit
# both pins, rebuild, re-run scripts/compose-smoke.sh.
FROM oven/bun:1.3.10@sha256:b86c67b531d87b4db11470d9b2bd0c519b1976eee6fcd71634e73abfa6230d2e AS runtime
WORKDIR /app/gateway

# CA ROOTS ARE LOAD-BEARING HERE. The bun base image carries none, and this service does nothing
# BUT dial upstream https — every MCP server, every OAuth discovery document, every token
# endpoint. Without them the first proxied call fails TLS verification and every sign-in walk
# dies at discovery. (The plane image installs these for the same reason; the web image inherits
# them from its Debian runtime.)
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Lockfile-first so `bun install` caches on the manifest, not the source tree. `--frozen-lockfile`
# fails the build if bun.lock and package.json disagree — the same contract CI enforces.
# `--production` leaves the test toolchain out of a deployed image; the service's runtime
# dependencies are two packages.
COPY gateway/package.json gateway/bun.lock ./
RUN bun install --frozen-lockfile --production

# UNLIKE the web image there is no build step: bun runs the TypeScript directly, which is why the
# gateway SERVES with bun (Bun.serve is the server) where the web app builds with bun and serves
# with Node. Only the three directories the process reads ship — the engine, the container host,
# and the SQL lineage `service/migrate.ts` applies at boot from a path relative to itself.
COPY gateway/core ./core
COPY gateway/service ./service
COPY gateway/migrations ./migrations
# The one-off master-key re-wrap, run against a stopped deployment's database:
#   docker compose run --rm gateway bun scripts/rewrap-workspace-keys.ts --from file --dry-run
COPY gateway/scripts ./scripts

# Both listeners bind all interfaces INSIDE the container (the checkout defaults are loopback, for
# a laptop); which of them the world can reach is the compose file's ruling, not this one.
# APP_ENV=production arms the refusal of placeholder secrets. The master key path points at the
# volume declared below.
ENV GATEWAY_BIND=0.0.0.0:8788 \
    GATEWAY_INTERNAL_BIND=0.0.0.0:8789 \
    GATEWAY_MASTER_KEY_FILE=/run/topos/gateway-master.key \
    APP_ENV=production
# DATABASE_URL, GATEWAY_INTERNAL_TOKEN, GATEWAY_PUBLIC_URL and TOPOS_PUBLIC_URL are intentionally
# unset — the compose file or the operator supplies them (the internal lane answers 404 on every
# route until a token is set).

# Run as an unprivileged user. /run/topos is created + owned here so the named volume (compose)
# inherits that ownership on first mount — the entrypoint mints the key file as this user, and a
# root-owned mount point would make first boot fail. This RUN must precede VOLUME: a directory
# modified after its VOLUME declaration would be discarded.
COPY gateway/docker-entrypoint.sh /usr/local/bin/gateway-entrypoint.sh
RUN chmod 0755 /usr/local/bin/gateway-entrypoint.sh \
    && useradd --system --uid 10001 topos \
    && mkdir -p /run/topos && chown topos:topos /run/topos
USER topos
VOLUME ["/run/topos"]
EXPOSE 8788
ENTRYPOINT ["/usr/local/bin/gateway-entrypoint.sh"]
CMD ["bun", "service/main.ts"]
