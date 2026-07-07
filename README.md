# Topos

[![CI](https://github.com/topos-sh/topos/actions/workflows/ci.yml/badge.svg)](https://github.com/topos-sh/topos/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A layer for AI agents to **share their behaviors** across a team — so every agent stays current with the
same company processes and everyone gets a consistent experience. A *behavior* (a "skill") is a bundle of
files (`SKILL.md` + scripts + reference docs); the **whole bundle** is the unit of trust.

Two programs in one Apache-2.0 workspace:

- **`topos`** — the local CLI an agent drives to add, follow, publish, and update behaviors.
- **`topos-plane`** — the self-hostable sharing server (a library + a thin binary).

They share one trust kernel, `topos-core`: the single, auditable implementation of the byte-exact digest,
signing, consent, and sync algorithm.

## Quickstart

Install the CLI:

```sh
curl -fsSL https://topos.sh/install.sh | sh
```

**Share a skill with your team** — from a machine that has the skill locally:

```sh
topos add ~/.claude/skills/pr-describe                     # adopt it (offline; no account)
topos list --json                                          # shows each skill's digest
topos publish pr-describe --approve pr-describe@<digest>   # sign in when prompted; prints an invite link
```

`--approve <skill>@<digest>` pins the exact bytes being shipped. The first publish stands up your workspace
on the hosted plane (sign in when prompted) and prints an `/i/` invite link for teammates.

**Follow your team's skills** — from a teammate's machine:

```sh
topos follow https://topos.sh/i/<token>   # approve in the browser when it opens; it completes on its own
topos follow --approve pr-describe        # place the disclosed first version
```

Following arms a session-start hook that runs `topos pull`, so updates the team publishes land byte-exact at
the start of each session — verified against the plane's signed pointer, and never over your local edits.

**Propose a change back:**

```sh
topos publish pr-describe --propose --approve pr-describe@<digest>   # open a PR-like proposal
topos review pr-describe@<hash> --approve                            # a reviewer lands it
```

## Install

```sh
curl -fsSL https://topos.sh/install.sh | sh
```

Installs the `topos` binary to `~/.local/bin` (no sudo). Platforms: macOS (Apple Silicon and Intel) and
Linux (x86_64 and arm64 — static musl, any distro, no runtime deps). On Windows, run it inside
[WSL2](https://learn.microsoft.com/windows/wsl/install).

The installer verifies a SHA-256 checksum downloaded over TLS and refuses to install on a mismatch (this
proves transit integrity and cannot be disabled). For origin integrity, verify the Sigstore build-provenance
attestation:

```sh
gh attestation verify topos-<target>.tar.gz --repo topos-sh/topos
```

Knobs (env var or flag): `TOPOS_VERSION` / `--version <tag>` pin a release; `TOPOS_INSTALL_DIR` / `--to <dir>`
set the install directory; `TOPOS_INSTALL_BASE_URL` points at a mirror or air-gapped proxy.

## Commands

The agent usually drives these non-interactively — `--json` emits a machine envelope and never prompts — but
the same verbs work by hand.

| Command | What it does |
|---|---|
| `add <dir>` | Adopt a local skill into topos (offline; no server, no account). |
| `follow <link>` | Enroll via an `/i/` link and follow its skills. Approve in the browser; the command completes on its own. |
| `follow --approve <skill>` | Place a disclosed first version (or resume an unfollowed skill). |
| `pull [<skill>]` | Apply updates to followed skills. The session-start hook runs this for you. |
| `publish <skill> --approve <skill>@<digest>` | Move `current` to your draft (or genesis-create a skill). |
| `publish --propose …` | Open a proposal (a PR) without moving `current`. |
| `review <skill>@<hash> --approve\|--reject` | Resolve a proposal. |
| `revert <skill> --to <hash> --approve <skill>@<hash>` | Move the team to older bytes — a forward, invertible move. |
| `invite [emails…] --skills …` | Owner mints an `/i/` invite link. |
| `unfollow <skill>` | Stop following; keep your local copy. |
| `list` · `diff` · `log` | Inventory skills · show a change · show a skill's history. |
| `uninstall` | Remove the hook, the binary, and `~/.topos/`. Touches no skill bytes. |

Consent is explicit end to end: `--approve <skill>@<digest>` pins the exact bytes, nothing lands that wasn't
disclosed and pinned, and a diverged local draft is surfaced — never overwritten.

## Trust & security

A behavior you follow is code and prose that runs inside your agent, so integrity and consent are the whole
point of the tool. A bundle's identity is a **byte-exact sha256** over every file (different bytes are never
"the same"); the plane **signs** every `current` pointer, and a follower **pins the plane key** on first
follow and verifies every move; and **nothing lands that was not disclosed and pinned**. What Topos does
*not* do is judge whether an approved behavior is safe to run — it guarantees disclosure and integrity, not
a sandbox or a second permission system.

The design behind this — trust boundaries, the consent + sync model — is in
[`ARCHITECTURE.md`](ARCHITECTURE.md); to run a plane safely, see [Self-hosting](#self-hosting-the-plane). To
report a vulnerability, see [`SECURITY.md`](SECURITY.md).

## Self-hosting the plane

The bundled compose file runs the plane and its Postgres together:

```sh
docker compose up --build     # plane on http://localhost:8787
```

### Stand up the first workspace

A brand-new plane has no workspace yet. Mint its first identity in-band:

```sh
topos-plane mint-claim --workspace w_acme --display-name "Acme"
```

This prints a one-time `/i/` claim link (a bearer owner capability — store it like a secret). A single
`topos follow <claim-link>` stands the workspace up and seats that device as its first owner, who can then
`publish` and mint ordinary invites.

### Configuration

`docker compose up` works out of the box. For a real (non-localhost) deployment, set:

- `TOPOS_PLANE_BASE_URL` — the public `https://…` address clients dial (behind your reverse proxy; the
  invite and verification links are built on it).
- `TOPOS_PLANE_ADMIN_TOKEN` *(optional)* — enables the review-required gate.
- `TOPOS_PLANE_SMTP_HOST` / `_PORT` / `_USER` / `_PASS` / `_FROM` *(optional)* — enable emailed-passcode
  enrollment (set all five).

The bundled `docker-compose.yml` is an annotated starting point (common vars with defaults; optional
features commented out). For the full reference — every variable and its default — run `topos-plane --help`.

Client-side: `TOPOS_DEBUG=1` prints each error's full source chain to stderr (the chain always lands in
`~/.topos/log.jsonl`); `TOPOS_HOME` overrides the `~/.topos` root.

### TLS

The plane serves plain HTTP and is designed to sit behind a TLS-terminating reverse proxy (Caddy, nginx,
Traefik, or your platform's load balancer). Point the proxy at `http://plane:8787`, set `TOPOS_PLANE_BASE_URL`
to your public `https://…` address, and let the proxy own certificates. (An optional, default-off built-in
ACME listener exists but is experimental — the reverse proxy is the supported path.)

## Build & contribute

```sh
cargo build
cargo xtask ci     # the full non-DB gate: fmt, clippy, doc, the drift gates, check-arch
cargo test         # requires a Postgres via DATABASE_URL
```

`cargo xtask ci` is the pre-push loop and matches CI's gate exactly. Compilation is offline (the
compile-time-checked queries read the committed `.sqlx` metadata) — only the tests need a database, which
the suite provisions per test:

```sh
export DATABASE_URL="postgres://topos:topos@localhost:5432/topos"
docker run --rm -e POSTGRES_USER=topos -e POSTGRES_PASSWORD=topos \
  -e POSTGRES_DB=topos -p 5432:5432 postgres:18
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) to propose changes and [`ARCHITECTURE.md`](ARCHITECTURE.md) for the
design.

## License

Apache-2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Copyright 2026 The Topos Authors.
Contributions are inbound = outbound, no CLA (see [`CONTRIBUTING.md`](CONTRIBUTING.md)).
