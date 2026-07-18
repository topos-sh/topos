# Release / launch checklist

The gate this public repository must clear before it is announced. Kept in-repo so the gate is auditable:
check an item off in the same change that lands it.

## Launch-gate artifacts (authored fresh, in-repo)

- [x] `SECURITY.md` — a focused vulnerability-reporting policy (GitHub private advisories). DONE. The trust
      model itself (what the digest/signature/consent chain does and does not protect against) lives in
      `ARCHITECTURE.md` + the README's "Trust & security" section, not in `SECURITY.md`.
- [x] `ARCHITECTURE.md` — the public design doc: the crate graph, the trust boundaries (client is never an
      authority; the plane is a composable library), the sync/consent model. DONE.
- [x] `CONTRIBUTING.md` — how to build (`cargo xtask ci`), run the Postgres-backed suite
      (`DATABASE_URL` + `cargo test`), and propose changes (inbound = outbound, no CLA). DONE. A
      a `NOTICE` (copyright), issue/PR templates, and `CODEOWNERS` landed alongside.

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
- [x] **TLS posture**: the plane serves plain HTTP — the reverse-proxy termination pattern is documented as
      the supported deployment in the README's self-hosting section. DONE.
- [ ] **At-rest key posture**: the plane signing key + enrollment secret are plaintext `0600` seeds
      (at-rest encryption not implemented). Accepted for v0. NOTE: the README's Backups prose (which stated
      this and the key-loss caveat) was trimmed, so the posture is currently documented nowhere
      user-facing — decide whether to restate it briefly or leave it.

## Release signing (minisign)

The release pipeline can sign every CLI tarball, `install.sh`, and `SHA256SUMS` with
[minisign](https://jedisct1.github.io/minisign/) — small, boring, verifiable with one command. The
scheme is committed end-to-end but DORMANT until the key ceremony runs.

**State today: UNSIGNED.** No key has been minted; releases carry checksums + GitHub artifact
attestation only, and `topos self-update` prints an honest "unsigned build" note on every install.

How the pieces fit:

- **The key ceremony** — `scripts/mint-release-key.sh`, run once by a maintainer (offline-safe). It
  mints a minisign keypair into a directory it creates `0700` and prints exactly what to paste
  where: the `RELEASE_PUBKEY` constant in `bins/topos/src/ops/self_update.rs`, the
  `MINISIGN_PUBKEY` variable in `scripts/install.sh`, and the `gh secret set MINISIGN_SECRET_KEY`
  command that arms CI. One commit flips both files; a test
  (`release_pubkey_when_present_is_a_valid_minisign_key`) guards the paste.
- **CI** (`.github/workflows/release.yml`, the `release` job) — with the `MINISIGN_SECRET_KEY`
  secret present, the job signs `dist/*.tar.gz`, `install.sh`, and `SHA256SUMS` (pre-hashed
  minisign; the trusted comment `topos-sh/topos <tag> <file>` binds each signature to ONE release
  — the clients token-match it) and uploads the `.minisig` files alongside the assets; a signing
  failure FAILS the release. With the secret absent, the release proceeds unsigned — today's
  behavior, bit-for-bit. The signer itself is a sha256-PINNED official minisign binary (never an
  unpinned package install next to the key), the key touches only a `0600` file under
  `RUNNER_TEMP` (removed when the step ends), and the job binds to the **`release-signing`
  environment**: store the secret as an ENVIRONMENT secret and give the environment a required
  reviewer, so a pushed `v*` tag alone cannot reach the key — pair it with a tag ruleset
  restricting `v*` creation to maintainers (both are repo settings; the ceremony script prints
  the exact steps).
- **The self-updater** (`topos self-update`) — with `RELEASE_PUBKEY = Some(…)` compiled in,
  signature verification is MANDATORY and fail-closed: the asset's `.minisig` is fetched and
  verified over the downloaded bytes BEFORE the checksum gate and long before the binary is
  touched; a missing or invalid signature is a typed `INTEGRITY_ERROR` with no unsigned fallback,
  and the SIGNED trusted comment must name the exact tag + asset the update resolved — so a valid
  signature minted for an OLD release cannot be re-served under a newer tag (a substitution the
  checksum cannot catch, since whoever moves the asset moves its SHA256SUMS too). With `None`
  (today) the checksum path is unchanged and the outcome discloses `signed: false` + the
  unsigned-build note.
- **The installer** (`scripts/install.sh`) — with `MINISIGN_PUBKEY` set, the asset's `.minisig` is
  REQUIRED and verified before the checksum whenever the `minisign` tool is installed (a pinned
  `--version` install additionally binds the signed comment to that tag); without the tool the
  signature step is skipped with a loud, honest note — the sha256 gate below it is never
  skippable. The skip is a DELIBERATE tradeoff, not an oversight: minisign is preinstalled
  nowhere, so a hard requirement would break virtually every `curl | sh` first install, and the
  installer rides the same origin as the assets — installer-side verification can never exceed
  origin trust. The origin-independent anchors are the binary's COMPILED-IN key (every
  `self-update` thereafter is fail-closed) and GitHub artifact attestation.

What signing adds over `SHA256SUMS`: checksums prove transit integrity (the sums file rides the
same origin as the asset), not origin integrity — whoever controls the release controls both files.
A minisign signature moves that trust to the offline secret key: a compromised release host or
repository account cannot mint a `.minisig` that an already-shipped binary accepts.

Rotation is a transitional release signed with the OLD key that embeds the NEW public key — the
ceremony script prints the full procedure.

- [ ] **Key ceremony** — not yet run (deliberate: mint the key at the first public release). Run
      `scripts/mint-release-key.sh` and follow its printed steps — including the two repo
      settings it names (required reviewer on the `release-signing` environment; a `v*` tag
      ruleset); nothing else needs editing. Until then, releases ship checksum-only — treat the
      ceremony as a launch-gate item.

## Already in place (verify green at the gate)

- [x] CI: fmt / clippy / rustdoc / the schema+fixture+OpenAPI drift gates / check-arch / the Postgres test
      suite / cargo-deny / the sqlx offline-metadata drift gate / the compose smoke job.
- [x] `cargo xtask ci` reproduces the non-DB gates locally, in CI's order.
- [x] Apache-2.0 license; pinned toolchain + pinned Docker builder image (the pair is drift-gated by
      `check-arch`).
