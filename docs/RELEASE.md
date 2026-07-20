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
- [x] **At-rest key posture**: the server holds no key files — the vault is identity-free and the
      internal-lane bearer arrives via environment, never disk. The one at-rest secret is the CLIENT's
      device credential (`~/.topos/identity/credentials.json`, plaintext `0600`; at-rest encryption not
      implemented). Accepted for v0, stated as fact in the README's self-hosting Backups section
      (disk/volume encryption is the operator's; a lost or leaked credential is handled by revoke +
      re-enroll).

## Release signing (minisign)

The release pipeline signs every CLI tarball, `install.sh`, and `SHA256SUMS` with
[minisign](https://jedisct1.github.io/minisign/) — small, boring, verifiable with one command.

**State today: ARMED.** The key ceremony has run. The release public key is compiled into the binary
(`RELEASE_PUBKEY` in `bins/topos/src/ops/self_update.rs`) and set in the installer (`MINISIGN_PUBKEY`
in `scripts/install.sh`), and the signing secret (`MINISIGN_SECRET_KEY`) is stored as an environment
secret of the `release-signing` GitHub environment. `topos self-update` requires a valid signature
(fail-closed); the installer verifies when the `minisign` tool is present.

How the pieces fit:

- **The key ceremony** — `scripts/mint-release-key.sh`, run once by a maintainer (offline-safe). It
  minted the minisign keypair and printed exactly what to paste where: the `RELEASE_PUBKEY` constant
  in `bins/topos/src/ops/self_update.rs`, the `MINISIGN_PUBKEY` variable in `scripts/install.sh`, and
  the `gh secret set MINISIGN_SECRET_KEY` command that armed CI. One commit flipped both files; a
  test (`release_pubkey_when_present_is_a_valid_minisign_key`) guards the pasted constant. Re-run the
  script only to rotate the key (below).
- **CI** (`.github/workflows/release.yml`, the `release` job) — with the `MINISIGN_SECRET_KEY` secret
  present (it is), the job signs `dist/*.tar.gz`, `install.sh`, and `SHA256SUMS` (pre-hashed minisign;
  the trusted comment `topos-sh/topos <tag> <file>` binds each signature to ONE release — the clients
  token-match it) and uploads the `.minisig` files alongside the assets; a signing failure FAILS the
  release. The signer itself is a sha256-PINNED official minisign binary (never an unpinned package
  install next to the key), the key touches only a `0600` file under `RUNNER_TEMP` (removed when the
  step ends), and the job binds to the **`release-signing` environment** — the secret lives there as
  an ENVIRONMENT secret behind a required reviewer, so a pushed `v*` tag alone cannot reach the key.
  (The presence check is a job env var; were the secret ever absent the release would proceed
  unsigned — it is not.)
- **The self-updater** (`topos self-update`) — with `RELEASE_PUBKEY = Some(…)` compiled in, signature
  verification is MANDATORY and fail-closed: the asset's `.minisig` is fetched and verified over the
  downloaded bytes BEFORE the checksum gate and long before the binary is touched; a missing or
  invalid signature is a typed `INTEGRITY_ERROR` with no unsigned fallback, and the SIGNED trusted
  comment must name the exact tag + asset the update resolved — so a valid signature minted for an OLD
  release cannot be re-served under a newer tag (a substitution the checksum cannot catch, since
  whoever moves the asset moves its SHA256SUMS too). A verified upgrade discloses `signed: true`.
- **The installer** (`scripts/install.sh`) — with `MINISIGN_PUBKEY` set, the asset's `.minisig` is
  REQUIRED and verified before the checksum whenever the `minisign` tool is installed (a pinned
  `--version` install additionally binds the signed comment to that tag); without the tool the
  signature step is skipped with a loud, honest note — the sha256 gate below it is never skippable.
  The skip is a tradeoff, not an oversight: minisign is preinstalled nowhere, so a hard requirement
  would break virtually every `curl | sh` first install, and the installer rides the same origin as
  the assets — installer-side verification can never exceed origin trust. The origin-independent
  anchors are the binary's COMPILED-IN key (every `self-update` thereafter is fail-closed) and GitHub
  artifact attestation.

What signing adds over `SHA256SUMS`: checksums prove transit integrity (the sums file rides the
same origin as the asset), not origin integrity — whoever controls the release controls both files.
A minisign signature moves that trust to the offline secret key: a compromised release host or
repository account cannot mint a `.minisig` that an already-shipped binary accepts.

Rotation is a transitional release signed with the OLD key that embeds the NEW public key — the
ceremony script (`scripts/mint-release-key.sh`) prints the full procedure.

## Before the first public release (in order)

- [x] **Key ceremony** — DONE. The keypair is minted, `RELEASE_PUBKEY` + `MINISIGN_PUBKEY` are set,
      and `MINISIGN_SECRET_KEY` is stored in the `release-signing` environment.
- [ ] **(a) Make the repository public.** The two protections below (an environment reviewer and a tag
      ruleset) are only available on a public repository on this plan, so they follow publishing.
- [ ] **(b) Add a required reviewer to the `release-signing` environment.** Repository → Settings →
      Environments → `release-signing` → require a reviewer (a maintainer). Until this is set, a pushed
      `v*` tag reaches the signing key with no human approval (the environment is auto-created
      unprotected on the first release run).
- [ ] **(c) Add a `v*` tag ruleset restricting tag creation to maintainers.** Repository → Settings →
      Rules → Rulesets → a tag ruleset targeting `v*` that limits who may create matching tags, so an
      unprivileged push cannot start a signed release.
- [ ] **(d) Verify the pipeline on a release-candidate tag.** Push `vX.Y.Z-rc.N` (see "Versioning and
      cutting a release" below), let `release.yml` run to completion, and confirm every asset,
      `SHA256SUMS`, and `.minisig` is present and that `install.sh` + `topos self-update` verify. An
      `-rc` tag runs the identical pipeline.
- [ ] **(e) Cut the first release.** Push `vX.Y.Z` and run the post-release verification.

## Already in place (verify green at the gate)

- [x] CI: fmt / clippy / rustdoc / the schema+fixture+OpenAPI drift gates / check-arch / the Postgres test
      suite / cargo-deny / the sqlx offline-metadata drift gate / the compose smoke job.
- [x] `cargo xtask ci` reproduces the non-DB gates locally, in CI's order.
- [x] Apache-2.0 license; pinned toolchain + pinned Docker builder image (the pair is drift-gated by
      `check-arch`).

## Versioning and cutting a release

Topos follows [semantic versioning](https://semver.org). Pre-1.0 (the `0.x` line): a `0.MINOR` bump
carries features and behavior changes; a `0.x.PATCH` bump carries fixes only. Release candidates are
`vX.Y.Z-rc.N`.

**How the version is derived.** `topos --version` prints `CARGO_PKG_VERSION`, which is the workspace
`[workspace.package] version` in the root `Cargo.toml` (both binaries inherit it via
`version.workspace = true`). The release is triggered by the **git tag**, not the crate version:
`release.yml` runs on any `v*` tag. The CLI tarball name is deliberately versionless
(`topos-<triple>.tar.gz`), so the `releases/latest/download/…` URLs stay stable. Nothing in the
pipeline checks that the tag equals the crate version — but `topos self-update` compares the resolved
tag's version against the binary's compiled-in `CARGO_PKG_VERSION` (pre-release suffixes ignored; only
the `X.Y.Z` core is compared). **So the crate version MUST be bumped to match the tag before tagging**,
or every binary the release ships reports a stale version and treats itself as perpetually behind.

### Cutting a release (the exact procedure)

Preconditions:

1. `main` is green. `release.yml` reuses the full CI gate (`gate: uses: ./.github/workflows/ci.yml`),
   so a red tree fails the release too.
2. Bump the version. Edit `[workspace.package] version` in the root `Cargo.toml` to `X.Y.Z` (or
   `X.Y.Z-rc.N` for a candidate), then run `cargo build` so `Cargo.lock` picks up the new member
   versions. The release builds with `--locked`, so a stale `Cargo.lock` fails it. Commit both files.
3. No changelog to maintain: `release.yml` generates the GitHub release notes (an asset table, the
   server-image pull lines, the verify commands, and — since signing is armed — a minisign note). A
   behavior change's doc updates land in the same change that introduced it, per `CONTRIBUTING.md`.

Cut it — from the pushed `main` commit that bumped the version:

```sh
git tag vX.Y.Z          # must equal the Cargo.toml version; -rc.N for a candidate
git push origin vX.Y.Z  # this tag push is the release trigger
```

Once the tag ruleset is in place only a maintainer can create a `v*` tag, and the `release` job waits
for the `release-signing` environment's required reviewer before it reaches the signing key.

Post-release verification:

- The GitHub release for `vX.Y.Z` exists and lists, for every built target, `topos-<triple>.tar.gz` +
  `SHA256SUMS` + a matching `<asset>.minisig` (plus `install.sh`, `install.sh.minisig`,
  `SHA256SUMS.minisig`, `topos-plane-image-digest.txt`, and the two SBOMs).
- Each signature verifies against the release public key:
  `minisign -Vm topos-<triple>.tar.gz -P <the RELEASE_PUBKEY / MINISIGN_PUBKEY value>`.
- From a clean environment, `curl -fsSL https://topos.sh/install | sh` installs and runs
  `topos --version`, printing `X.Y.Z`.
- `shasum -a 256 -c SHA256SUMS` passes, and `gh attestation verify <asset> --repo topos-sh/topos`
  passes.
- Once a prior release exists, `topos self-update --json` from it upgrades and reports `"signed": true`
  (the fail-closed signature gate passed); a tampered asset refuses with `INTEGRITY_ERROR`.

A `-rc.N` tag runs this identical pipeline. Note that `gh release create` marks whatever it publishes
as the latest release (the workflow passes no prerelease flag), so a candidate tag becomes the
`releases/latest` target until a final tag supersedes it — cut candidates deliberately.
