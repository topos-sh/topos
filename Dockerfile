# The self-hostable Topos plane — a STATELESS image: just the `topos-plane` binary + CA roots.
#
# Postgres is deliberately NOT in this image (one concern per container). Point the plane at a database with
# `DATABASE_URL`; the bundled `docker-compose.yml` runs a pinned Postgres beside it, or bring your own
# (managed / external) Postgres. The metadata schema is embedded in the binary and migrated on startup; the
# git-object + large-object stores are on a mounted volume. Because sqlx's Postgres driver is pure-Rust,
# the runtime carries no database client library.

# ── builder ──────────────────────────────────────────────────────────────────────────────────────────
FROM rust:1.96-bookworm AS builder
WORKDIR /build
# The compile-time-checked queries read the committed `crates/plane-store/.sqlx` metadata, so the build
# needs no live database.
ENV SQLX_OFFLINE=true
COPY . .
RUN cargo build --release --locked -p topos-plane

# ── runtime ──────────────────────────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
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
VOLUME ["/data"]
EXPOSE 8787
ENTRYPOINT ["topos-plane"]
