# The self-hostable Topos plane — a STATELESS image: just the `topos-plane` binary + CA roots.
#
# Postgres is deliberately NOT in this image (one concern per container). Point the plane at a database with
# `DATABASE_URL`; the bundled `docker-compose.yml` runs a pinned Postgres beside it, or bring your own
# (managed / external) Postgres. The metadata schema is embedded in the binary and migrated on startup; the
# git-object + large-object stores are on a mounted volume. Because sqlx's Postgres driver is pure-Rust,
# the runtime carries no database client library.

# ── builder ──────────────────────────────────────────────────────────────────────────────────────────
# Base images are pinned by the multi-arch INDEX digest (not a per-platform manifest) so one immutable
# pin serves both linux/amd64 and linux/arm64 builds. Bumps are deliberate: re-resolve with
#   docker buildx imagetools inspect rust:1.96-bookworm    (take the top-level "Digest:")
# edit the pin, rebuild, and re-run scripts/compose-smoke.sh.
FROM rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS builder
WORKDIR /build
# The compile-time-checked queries read the committed `crates/plane-store/.sqlx` metadata, so the build
# needs no live database.
ENV SQLX_OFFLINE=true
COPY . .
# Optional cargo features for the plane build (e.g. --build-arg FEATURES=acme). Empty (the default,
# and what the published image is built with) compiles exactly the standard plane.
ARG FEATURES=""
RUN cargo build --release --locked -p topos-plane ${FEATURES:+--features "$FEATURES"}

# ── runtime ──────────────────────────────────────────────────────────────────────────────────────────
# Refresh: docker buildx imagetools inspect debian:bookworm-slim
FROM debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/topos-plane /usr/local/bin/topos-plane

# The plane's own state lives under /data (mount a volume): the per-workspace git + large object stores and
# the `0600` plane signing key + enrollment secret (generated on first boot if absent). The metadata lives
# in Postgres (DATABASE_URL), NOT here.
# The key/secret paths sit directly under the mounted /data (the plane's `create_dir_all` makes the git +
# large roots, but the signer writes its seed file with O_EXCL and does not create a parent directory — and
# a VOLUME shadows any directory the image created at /data). /data itself is the volume mount, so it exists.
ENV TOPOS_PLANE_BIND=0.0.0.0:8787 \
    TOPOS_PLANE_GIT_ROOT=/data/git \
    TOPOS_PLANE_LARGE_ROOT=/data/large \
    TOPOS_PLANE_KEY=/data/plane.key \
    TOPOS_PLANE_ENROLL_SECRET=/data/enroll.key \
    TOPOS_PLANE_MODE=self_host
# DATABASE_URL is intentionally unset — it is BYO (the compose file or the operator supplies it).

# Run as an unprivileged user. /data is created + owned here so the named volume (compose) inherits that
# ownership on first mount; the plane writes its 0600 key/secret directly under /data on first boot. This
# RUN must precede VOLUME — a directory modified after its VOLUME declaration would be discarded.
RUN useradd --system --uid 10001 topos \
    && mkdir -p /data && chown topos:topos /data
USER topos
VOLUME ["/data"]
EXPOSE 8787
ENTRYPOINT ["topos-plane"]
