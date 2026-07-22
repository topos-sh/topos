# The product web app (`@topos/web`) — React Router 8 on bun, the ONE public surface after the door
# cutover: it fronts `/api`, serves the resource addresses + the constant protocol card, and drives the
# plane's internal session lane. The Rust plane loses its published port and answers only on the
# compose-internal network; this image is what the world reaches.
#
# BUILD CONTEXT IS THE REPO ROOT (compose: `build: { context: ., dockerfile: web/Dockerfile }`). The app
# reads two files that live OUTSIDE `web/` — the vault OpenAPI contract and the checksummed installer —
# so the context must be the root, not `web/`, and the paths below are written root-relative. Everything
# is laid out to MIRROR the repo tree (`/app/web` + a sibling `/app/scripts` + `/app/contracts`), so the
# app's own `../scripts/…` / `../contracts/…` relative reads resolve exactly as they do in a checkout.

# ── builder ────────────────────────────────────────────────────────────────────────────────────────────
# Pinned by the multi-arch INDEX digest (not a per-platform manifest) so one immutable pin serves both
# linux/amd64 and linux/arm64. Refresh deliberately: docker buildx imagetools inspect oven/bun:1.3.10
# (take the top-level "Digest:"), edit the pin, rebuild, re-run scripts/compose-smoke.sh.
FROM oven/bun:1.3.10@sha256:b86c67b531d87b4db11470d9b2bd0c519b1976eee6fcd71634e73abfa6230d2e AS builder
WORKDIR /app/web

# Lockfile-first so `bun install` caches on the manifest, not the whole source tree. `--frozen-lockfile`
# fails the build if bun.lock and package.json disagree — the same contract CI enforces.
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile

# The two repo files the app reads relative to its own root (kept at the mirrored sibling paths):
#   ../contracts/openapi/openapi.json — the vault contract the type-gen + `check:contract` gate read
#     (scripts/gen-plane-types.mjs resolves it as <web>/../contracts/openapi/openapi.json).
#   ../scripts/install.sh — the bytes `GET /install` serves (env.server.ts INSTALL_SH_PATH default
#     `../scripts/install.sh`, resolved against the process cwd = this WORKDIR).
COPY contracts/openapi/openapi.json /app/contracts/openapi/openapi.json
COPY scripts/install.sh /app/scripts/install.sh

# The app source (web/node_modules + web/build are excluded by .dockerignore, so the host tree never
# clobbers the freshly-installed deps or leaks a stale build).
COPY web/ ./
RUN bun run build

# ── runtime ────────────────────────────────────────────────────────────────────────────────────────────
# NODE, not bun, runs the server — the code decides: app/entry.server.tsx is a Node server (`node:stream`
# PassThrough, `@react-router/node`, `renderToPipeableStream` from `react-dom/server`). Under a bun
# runtime, bun swaps `react-dom/server` for `react-dom/server.bun.js`, which has no `renderToPipeableStream`,
# and the server crashes on boot. The team's `bun run start` works only because their machines ALSO have
# Node on PATH, so bun delegates to react-router-serve's `#!/usr/bin/env node` shebang; a Node-less image
# has no such fallback. So: BUILD with bun (above), SERVE with Node here. bun's node_modules layout is
# standard, so Node consumes it directly. Debian-bookworm base matches the plane's runtime family (and
# carries `useradd`). Refresh: docker buildx imagetools inspect node:22-bookworm-slim.
FROM node:22-bookworm-slim@sha256:53ada149d435c38b14476cb57e4a7da73c15595aba79bd6971b547ceb6d018bf AS runtime
WORKDIR /app/web

# APP_ENV=production arms the server-tier belts (the sign-in + write rate limiters). PORT/HOST are what
# react-router-serve binds. INSTALL_SH_PATH is pinned ABSOLUTE so the `/install` route never depends on the
# process cwd — it also equals the checkout default `../scripts/install.sh` resolved from this WORKDIR, so
# nothing drifts. The app parses its env LAZILY (first request), so a missing runtime secret fails a request
# loudly, never the image build.
ENV NODE_ENV=production \
    APP_ENV=production \
    PORT=3000 \
    HOST=0.0.0.0 \
    INSTALL_SH_PATH=/app/scripts/install.sh

# Unprivileged user (uid 10001, matching the plane image). react-router-serve reads the build + deps, and
# the app writes nothing to disk (its state is Postgres + the vault), so a read-only-friendly non-root
# user is enough.
RUN useradd --system --uid 10001 --create-home --home-dir /home/topos topos

# react-router-serve needs exactly: the build output (server + client), the installed deps,
# package.json (`"type": "module"` + the `start` script), and the DRIZZLE MIGRATIONS — the boot
# migrator (app/lib/db/migrate.server.ts) runs at process start, resolves `<cwd>/drizzle`, and
# fails LOUDLY when it is absent, so an image without that folder never comes up (the compose
# smoke renders a document to pin this). The installer bytes ride along at the mirrored sibling
# path the ENV points at. No app source, no config file is needed at runtime.
COPY --from=builder --chown=10001:10001 /app/web/build ./build
COPY --from=builder --chown=10001:10001 /app/web/node_modules ./node_modules
COPY --from=builder --chown=10001:10001 /app/web/package.json ./package.json
COPY --from=builder --chown=10001:10001 /app/web/drizzle ./drizzle
COPY --from=builder --chown=10001:10001 /app/scripts/install.sh /app/scripts/install.sh

USER topos
EXPOSE 3000
# Serve under Node (same command the `start` script runs, invoked directly on the CJS bin so it never needs
# bun or a PATH shim). The web schema migrates itself EAGERLY at process start (the server entry runs
# app/lib/db/migrate.server.ts at module load, before the port is bound), so the FIRST request already
# sees a migrated database — the entrypoint only serves.
CMD ["node", "node_modules/@react-router/serve/bin.cjs", "./build/server/index.js"]
