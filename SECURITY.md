# Security policy

Topos is infrastructure for sharing the behaviors that drive AI agents. A shared behavior is code and
prose that runs inside your agent, so integrity and consent are the whole point of the tool. This document
states what the trust model protects, what it deliberately does **not**, how to run a plane safely, and how
to report a vulnerability.

## Reporting a vulnerability

**Please do not open a public issue for a security report.** Use GitHub's private vulnerability reporting:

> **[Report a vulnerability](https://github.com/topos-sh/topos/security/advisories/new)**
> (repository → **Security** → **Advisories** → **Report a vulnerability**)

This opens a private advisory visible only to you and the maintainers. Include, where you can:

- what an attacker gains (the impact), and the trust boundary it crosses;
- the affected component (`topos` CLI, the `topos-plane` server, or a specific crate) and version;
- a minimal reproduction — commands, a bundle, or a request sequence.

We aim to acknowledge a report within **72 hours** and to keep you updated as we confirm, fix, and prepare a
release. We will credit reporters who want credit. Because Topos is pre-1.0, security fixes ship on the
**latest release** only; there is no back-port line yet.

| Version | Supported |
|---|---|
| latest `0.x` release | ✅ |
| older `0.x` | ❌ (upgrade to latest) |

## The trust model — what Topos guarantees

The unit of trust is the **whole bundle** (`SKILL.md` + scripts + reference docs), because prose can drive
an agent as surely as a script can. Three mechanisms carry the guarantee, and all three are implemented
**once**, in the pure `topos-core` kernel — the single, auditable trust implementation that the CLI, the
plane, the tests, and the generated contracts all link.

- **Byte-exact digest.** A bundle's identity is a plain sha256 over the raw bytes of every file — no
  normalization, no canonicalization. Different bytes are never "the same" bundle. `<skill>@<hash>` pins
  that exact digest.
- **Explicit, end-to-end consent.** Nothing lands that was not first disclosed and pinned. Publishing
  requires `--approve <skill>@<digest>` against the exact bytes being shipped; following requires approving
  the disclosed first version. There are no silent overwrites — a follower's local edits (a *diverged
  draft*) are surfaced, never clobbered.
- **Signed, monotonic pointers.** A team follows one movable `current` pointer per skill. The plane signs
  every pointer move with an Ed25519 key; a follower **pins that plane key on first follow** and verifies
  every subsequent pointer against it. Pointers carry a monotonic generation, and a follower refuses any
  pointer at or below the highest generation it has already seen (anti-rollback) — a restored-from-backup
  or replayed older pointer raises an alarm rather than moving a team backward.

The result: what a human (or a delegated reviewer) approved is byte-for-byte what every agent receives,
and a compromised transport cannot substitute different bytes without failing verification.

### Trust boundaries

- **The client is never an authority.** `topos` is a thin sync tool. It holds no database, computes no
  authoritative roster, and takes no dependency on the server's storage crate — a dependency-graph check
  (`cargo xtask check-arch`) enforces this at build time.
- **The plane is the authority for the pointer and the roster**, and nothing else. It signs pointers and
  gates who may read or write a workspace's skills.
- **Integrity and disclosure, not a second permission system.** See below.

## What Topos deliberately does NOT do

Read this part carefully — it is where most misconceptions live.

- **Topos is not a sandbox and not a permission system.** A behavior you follow is code and prose that runs
  inside your harness with your harness's permissions. Topos guarantees that the bytes are exactly what was
  disclosed and approved; it does **not** judge whether those bytes are safe to run. *How much a human sits
  in the loop before an approved behavior executes is the job of your agent/harness setup, never this tool.*
  Only follow skills from a workspace you trust, and use the **review-required** gate (below) for skills
  where a second human must approve every change.
- **The digest proves integrity, not safety.** A signed, byte-exact bundle can still contain a hostile
  script. Signing and pinning guarantee provenance and consent — that this is the bundle the workspace
  published and you approved — not that its contents are benign.
- **The installer checksum proves transit integrity, not origin.** The installer downloads `SHA256SUMS`
  over TLS from the same origin as the binary and refuses to install on a mismatch. That proves the bytes
  you got are the bytes the release published; it does not prove *who* published them (whoever controls the
  release controls both files). For origin integrity, verify the Sigstore build-provenance attestation:

  ```sh
  gh attestation verify topos-<target>.tar.gz --repo topos-sh/topos
  ```

### The anti-poisoning lever: review-required

By default a workspace owner can move `current` directly. A plane operator can turn on a **review-required**
gate per workspace, after which a change must be `publish --propose`d and a **second** party must
`review --approve` it before it becomes `current` (four-eyes). This is the primary defense against a single
compromised author poisoning a followed behavior. It is off by default; enabling it is an operator (admin
token) or hosted-composition action.

## Operating a plane safely

If you self-host the plane, these are the postures to know. Some are deliberate v0 trade-offs, stated
plainly.

- **TLS terminates at a reverse proxy (the supported default).** The plane serves plain HTTP on `:8787` and
  is designed to sit behind a TLS-terminating reverse proxy (Caddy, nginx, Traefik, or your platform's load
  balancer), which owns certificates and renewal. Do not expose `:8787` directly to the internet. An
  optional, default-off, **experimental** built-in ACME listener exists but is unproven on a real box — the
  reverse proxy is the documented, supported path. See the README's self-hosting section.
- **The plane's keys are plaintext `0600` seeds at rest.** On first boot the plane generates its Ed25519
  signing key and its enrollment HMAC secret as `0600` files on the mounted data volume. At-rest encryption
  is **not yet implemented**, so the security of those two files is the security of the volume: restrict
  access to it, back it up like a secret, and never commit or log it. Losing or replacing the signing key
  is a key-rotation event — every follower that pinned the old key fails closed.
- **Invite, enrollment, and read tokens are bearer capabilities.** A `/i/<token>` link or a device grant is
  a secret: anyone holding it can act with its scope. Treat minted links like passwords, and prefer
  short-lived distribution. The plane derives them deterministically from the enrollment secret, so
  protecting that seed protects them all.
- **The admin token gates operator routes.** `TOPOS_PLANE_ADMIN_TOKEN` enables the review-required policy
  route; while unset that route answers 404. Only its sha256 is retained. Give it the handling of a root
  credential.
- **Back up in the right order.** Snapshot the object store **before** the database, and re-issue pointers
  through the plane after an *older*-than-current restore (anti-rollback will otherwise fail-close
  followers). The README's "Backups & restore" section is the procedure.
- **Secrets are never logged.** SMTP credentials, tokens, and keys are kept out of the JSON logs by
  construction; report any leak into logs as a vulnerability.

## Scope

In scope: the `topos` CLI, the `topos-plane` server and its library crates, the release pipeline, and the
installer — anything in this repository. Out of scope: misconfiguration of a self-hosted deployment against
this document's guidance, third-party dependencies (report those upstream; we track advisories with
`cargo-deny` in CI), and any separate hosted product built on top of this OSS core.

## Supply-chain posture

- The toolchain and the Docker builder image are pinned, and the pin pair is drift-gated in CI.
- `cargo-deny` runs on every change for advisories **and** license compliance.
- Releases publish SBOMs and Sigstore build-provenance attestations for every artifact.
- `unsafe` code is forbidden workspace-wide (`#![forbid(unsafe_code)]`).
