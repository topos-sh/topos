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

## History hygiene (before the first public push)

- [x] **Scrub or squash the pre-branch mainline commit messages that name internal review processes.**
      DONE (pre-publication history rewrite): every commit message and historical file revision was
      swept for reviewer names, review-round labels, and roadmap tags — the head tree is byte-identical
      — and the author identity was normalized. Re-audit only if commits land from an unswept branch.
      The published history must be self-contained: no commit message may reference internal reviewers,
      review-round labels, private planning documents, or internal roadmap/phase tags. Audit the shape,
      not just known strings — anything that reads as "who reviewed this and in which internal round"
      rather than "what changed and why" gets reworded (or the run of commits squashed) before the
      remote ever sees it. The committed files are already swept by the in-repo greps; this item covers
      the `git log` itself.

## Operational gaps to close (or explicitly accept and document)

- [x] **First-boot workspace standup**: DONE — the binary mints the one-time claim in-band
      (`topos-plane mint-claim` prints the `/i/` link exactly once; the token never enters tracing), and
      one `topos follow <claim-link>` seats the first owner. The README's self-host walkthrough shows it.
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
