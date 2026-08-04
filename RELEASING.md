# Releasing

The maintainer runbook for cutting a `topos` release. Not user documentation — the user-facing
install and verification steps live in the docs site under `docs/`.

## Versioning

Topos follows [semantic versioning](https://semver.org). Pre-1.0 (the `0.x` line): releases bump the
**PATCH** — features, behavior changes, and fixes alike; a `0.MINOR` bump is reserved for milestone
or compatibility-breaking shifts. Release candidates are `vX.Y.Z-rc.N`.

**How the version is derived.** `topos --version` prints `CARGO_PKG_VERSION`, which is the workspace
`[workspace.package] version` in the root `Cargo.toml` (both binaries inherit it via
`version.workspace = true`). The release is triggered by the **git tag**, not the crate version:
`release.yml` runs on any `v*` tag. The CLI tarball name is deliberately versionless
(`topos-<triple>.tar.gz`), so the `releases/latest/download/…` URLs stay stable.

Nothing in the pipeline checks that the tag equals the crate version — but `topos self-update`
compares the resolved tag's version against the binary's compiled-in `CARGO_PKG_VERSION`
(pre-release suffixes ignored; only the `X.Y.Z` core is compared). **So the crate version MUST be
bumped to match the tag before tagging**, or every binary the release ships reports a stale version
and treats itself as perpetually behind.

## Cutting a release

Preconditions:

1. `main` is green. `release.yml` reuses the full CI gate (`gate: uses: ./.github/workflows/ci.yml`),
   so a red tree fails the release too.
2. Bump the version — in **two** places, both asserted against the tag by the release's `pins` job
   before anything is built or pushed:
   - `[workspace.package] version` in the root `Cargo.toml` → `X.Y.Z` (or `X.Y.Z-rc.N`), then run
     `cargo build` so `Cargo.lock` picks up the new member versions. The release builds with
     `--locked`, so a stale `Cargo.lock` fails it. The web app reads the same number: run
     `cd web && bun run gen:plane` and commit the regenerated
     `web/app/lib/plane/contract/version.ts`, which is the release version the app declares on its
     protocol card. CI's `check:contract` gate fails while it is stale.
   - The image pin in `docker-compose.yml` → `${TOPOS_VERSION:-vX.Y.Z}` on **both** services. That
     file is the published deployment artifact (served at `/compose.yml`, and the file a self-host
     clone of the tag runs), so it must name the release it ships with. A `-rc.N` tag is exempt —
     the pin stays on the last stable release, because a candidate is not what a self-hoster should
     land on.

   Commit them together.
3. No changelog to maintain: `release.yml` generates the GitHub release notes (an asset table, the
   self-host quickstart, the image pull lines, the verify commands, and a minisign note). A
   behavior change's doc updates land in the same change that introduced it, per `CONTRIBUTING.md`.
4. **A wire-breaking release moves BOTH compatibility floors, in the same change as the break.**
   Each side declares the oldest counterpart it speaks to: `MIN_SERVER_VERSION` in
   `bins/topos/src/compat.rs` (the oldest server the CLI dials) and `MIN_CLI_VERSION` in
   `web/app/lib/api/compat.server.ts` (the oldest CLI the server answers). Set both to the release
   that breaks the wire — that is the entire mechanism, and a break that ships without moving them
   is a break that fails silently on somebody's machine instead of refusing with its own fix. An
   additive release moves neither. One consequence is already written into the server half: a
   client that does not identify itself passes only while the CLI floor sits at or below `0.1.21`,
   the first release whose CLI names its version on the session lane. Raise the floor past that and
   silence stops being ambiguous — unidentified clients are refused with the rest.

Cut it — from the pushed `main` commit that bumped the version:

```sh
git tag vX.Y.Z          # must equal the Cargo.toml version; -rc.N for a candidate
git push origin vX.Y.Z  # this tag push is the release trigger
```

Only a maintainer can create a `v*` tag (the `release-tags` ruleset), and the `release` job waits for
the `release-signing` environment's required reviewer before it reaches the signing key — so every
release pauses for one approval click.

A `-rc.N` tag runs the identical pipeline. Note that `gh release create` marks whatever it publishes
as the latest release (the workflow passes no prerelease flag), so a candidate tag becomes the
`releases/latest` target until a final tag supersedes it — cut candidates deliberately.

## Post-release verification

- The GitHub release for `vX.Y.Z` lists, for every built target, `topos-<triple>.tar.gz` +
  `SHA256SUMS` + a matching `<asset>.minisig` (plus `install.sh`, `install.sh.minisig`,
  `SHA256SUMS.minisig`, both `<image>-image-digest.txt` files, and the three SBOMs).
- Both images are pullable ANONYMOUSLY — the release publishes them, but a package's registry
  visibility is a settings toggle, and a newly created package is born **private**:
  `docker logout ghcr.io && docker pull ghcr.io/topos-sh/topos-web:vX.Y.Z`. A `denied` here means
  the package needs Package settings → Change visibility → Public, by hand — there is no REST API
  for package visibility. If that dialog shows Public greyed out as *disabled by organization
  administrators*, the block is one level up: Organization settings → Packages → Package creation
  must have **Public** ticked before any package in the org can be made public. The self-host
  quickstart is broken for everyone until the anonymous pull passes.
- Each signature verifies against the release public key:
  `minisign -Vm topos-<triple>.tar.gz -P <the RELEASE_PUBKEY / MINISIGN_PUBKEY value>`.
- From a clean environment, `curl -fsSL https://topos.sh/install | sh` installs and runs
  `topos --version`, printing `X.Y.Z`.
- `shasum -a 256 -c SHA256SUMS` passes, and `gh attestation verify <asset> --repo topos-sh/topos`
  passes.
- `topos self-update --json` from the previous release upgrades and reports `"signed": true` (the
  fail-closed signature gate passed); a tampered asset refuses with `INTEGRITY_ERROR`.

## Release signing (minisign)

The release pipeline signs every CLI tarball, `install.sh`, and `SHA256SUMS` with
[minisign](https://jedisct1.github.io/minisign/) — small, boring, verifiable with one command.

**State: armed.** The release public key is compiled into the binary (`RELEASE_PUBKEY` in
`bins/topos/src/ops/self_update.rs`) and set in the installer (`MINISIGN_PUBKEY` in
`scripts/install.sh`); the signing secret (`MINISIGN_SECRET_KEY`) is an environment secret of the
`release-signing` GitHub environment, behind a required reviewer.

How the pieces fit:

- **The key ceremony** — `scripts/mint-release-key.sh`, run once by a maintainer (offline-safe). It
  minted the keypair and printed exactly what to paste where: the `RELEASE_PUBKEY` constant, the
  `MINISIGN_PUBKEY` variable, and the `gh secret set MINISIGN_SECRET_KEY` command that armed CI. A
  test (`release_pubkey_when_present_is_a_valid_minisign_key`) guards the pasted constant. Re-run the
  script only to rotate the key.
- **CI** (`.github/workflows/release.yml`, the `release` job) — signs `dist/*.tar.gz`, `install.sh`,
  and `SHA256SUMS` (pre-hashed minisign; the trusted comment `topos-sh/topos <tag> <file>` binds each
  signature to ONE release — clients token-match it) and uploads the `.minisig` files alongside the
  assets. A signing failure FAILS the release. The signer is a sha256-pinned official minisign
  binary, and the key touches only a `0600` file under `RUNNER_TEMP`, removed when the step ends.
- **The self-updater** — with `RELEASE_PUBKEY` compiled in, signature verification is MANDATORY and
  fail-closed: the `.minisig` is verified over the downloaded bytes BEFORE the checksum gate and long
  before the binary is touched. A missing or invalid signature is a typed `INTEGRITY_ERROR` with no
  unsigned fallback, and the signed trusted comment must name the exact tag + asset the update
  resolved — so a valid signature minted for an OLD release cannot be re-served under a newer tag (a
  substitution the checksum cannot catch, since whoever moves the asset moves its `SHA256SUMS` too).
- **The installer** (`scripts/install.sh`) — the `.minisig` is REQUIRED and verified before the
  checksum whenever the `minisign` tool is installed (a pinned `--version` install additionally binds
  the signed comment to that tag); without the tool the signature step is skipped with a loud, honest
  note — the sha256 gate below it is never skippable. The skip is a tradeoff, not an oversight:
  minisign is preinstalled nowhere, so a hard requirement would break virtually every `curl | sh`
  first install, and the installer rides the same origin as the assets — installer-side verification
  can never exceed origin trust. The origin-independent anchors are the binary's COMPILED-IN key
  (every `self-update` thereafter is fail-closed) and GitHub artifact attestation.

What signing adds over `SHA256SUMS`: checksums prove transit integrity (the sums file rides the same
origin as the asset), not origin integrity — whoever controls the release controls both files. A
minisign signature moves that trust to the offline secret key: a compromised release host or
repository account cannot mint a `.minisig` that an already-shipped binary accepts.

**Rotation** is a transitional release signed with the OLD key that embeds the NEW public key — the
ceremony script prints the full procedure.

## Repository protections

In place, and assumed by the procedure above:

- `main-linear` — a branch ruleset keeping `origin/main` linear.
- `release-tags` — a tag ruleset restricting who may create `v*` tags.
- `release-signing` — a deployment environment with a required reviewer, so a pushed tag alone cannot
  reach the signing key.
- Secret scanning with push protection.

## CI gates a release inherits

fmt · clippy · rustdoc · the schema/fixture/OpenAPI drift gates · `check-arch` · the Postgres test
suite · cargo-deny · the sqlx offline-metadata drift gate · the compose smoke job · the web job
(checks, vitest, Playwright).

`cargo xtask ci` reproduces the non-DB gates locally, in CI's order.
