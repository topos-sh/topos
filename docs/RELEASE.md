# Release / launch checklist

The gate this public repository must clear before it is announced. Kept in-repo so the gate is auditable:
check an item off in the same change that lands it.

## Launch-gate artifacts (authored fresh, in-repo)

- [ ] `SECURITY.md` — the trust model (what the digest/signature/consent chain does and does not protect
      against) + a vulnerability-reporting channel.
- [ ] `ARCHITECTURE.md` — the public design doc: the crate graph, the trust boundaries (client is never an
      authority; the plane is a composable library), the sync/consent model.
- [ ] `CONTRIBUTING.md` — how to build (`cargo xtask ci`), run the Postgres-backed suite
      (`DATABASE_URL` + `cargo test`), and propose changes.

## Operational gaps to close (or explicitly accept and document)

- [ ] **First-boot workspace standup**: mint + log the one-time `admin-claim` token so a fresh self-hosted
      plane can seat its first owner in-band (the authority op + `POST /v1/admin-claim` route exist; the
      binary does not yet mint the token).
- [ ] **TLS posture**: the plane serves plain HTTP — the reverse-proxy termination pattern must be
      documented as the supported deployment (it is, in the README; restate it in `SECURITY.md`).
- [ ] **At-rest key posture**: the plane signing key + enrollment secret are plaintext `0600` seeds;
      either encrypt at rest or state the posture in `SECURITY.md`.

## Already in place (verify green at the gate)

- [x] CI: fmt / clippy / rustdoc / the schema+fixture+OpenAPI drift gates / check-arch / the Postgres test
      suite / cargo-deny / the sqlx offline-metadata drift gate / the compose smoke job.
- [x] `cargo xtask ci` reproduces the non-DB gates locally, in CI's order.
- [x] Apache-2.0 license; pinned toolchain + pinned Docker builder image (the pair is drift-gated by
      `check-arch`).
