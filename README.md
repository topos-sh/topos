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
consent, content-addressed identity, and sync algorithm.

## Quickstart

Install the CLI:

```sh
curl -fsSL https://topos.sh/install.sh | sh
```

**Share a skill with your team** — from a machine that has the skill locally:

```sh
topos add ~/.claude/skills/pr-describe   # adopt it (offline; no account)
topos publish pr-describe                # sign in when prompted; prints your workspace address
```

The first publish stands up your workspace on the hosted plane (sign in when prompted) and prints its
**address** (`https://topos.sh/<name>`) — the share line teammates paste to follow. To pin the exact bytes
being shipped, add a `@<digest>` suffix (`topos publish pr-describe@<digest>`, where `topos list --json`
prints each digest) — the publish then refuses on any mismatch.

**Follow your team's skills** — from a teammate's machine:

```sh
topos follow https://topos.sh/acme   # your workspace address; approve in the browser, it completes on its own
topos follow pr-describe              # place the disclosed first version
```

Following arms a session-start hook that runs `topos update`, so updates the team publishes land byte-exact
at the start of each session — verified byte-for-byte against the plane's `current` pointer, and never over
your local edits.

**Propose a change back:**

```sh
topos publish pr-describe --propose         # open a PR-like proposal
topos review pr-describe@<hash> --approve   # a reviewer lands it
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
the same verbs work by hand. Every mutating verb is **two-phase**: a bare invocation *describes* what would
change (nothing is written), and `--yes` applies it. The full, always-current reference — every flag,
generated straight from the CLI — is in [`docs/cli.md`](docs/cli.md).

| Command | What it does |
|---|---|
| `add <dir>` | Adopt a local skill into topos (offline; no server, no account). |
| `follow <address>` | Enroll in a workspace by its address and subscribe (its channels / skills). Approve in the browser; the command completes on its own. |
| `follow <skill>` | Place a disclosed first version (or resume an unfollowed skill). |
| `update [<skill>]` | Apply updates to followed skills. The session-start hook runs this for you. (`pull` is a hidden alias.) |
| `publish <skill>[@<digest>]` | Move `current` to your draft (or genesis-create a skill); the optional `@<digest>` pins the exact bytes. |
| `publish --propose <skill>` | Open a proposal (a PR) without moving `current`. |
| `review [<skill>@<hash> --approve\|--reject]` | The review inbox, or resolve a proposal. |
| `revert <skill> --to <hash>` | Move the team to older bytes — a forward, invertible move. |
| `channel add\|remove <channel> <skill>…` | Group skills into channels (the distribution unit). |
| `protect <target> [<level>]` | Set a skill's or channel's protection level. |
| `invite <emails…>` | Seat teammates as members (they join by the workspace address). |
| `unfollow <skill>` · `remove <skill>` | Stop following (keep the copy) · take a skill off this device. |
| `list` · `diff` · `log` | Inventory skills · show a change · show a skill's history. |
| `auth login\|logout\|status` · `self-update` | Manage this install's sign-in · update the `topos` binary. |

Consent is explicit end to end: a `<skill>@<digest>` pin binds the exact bytes, nothing lands that wasn't
disclosed and pinned, and a diverged local draft is surfaced — never overwritten.

## Skill discovery across harnesses

`topos list` scans the skills directory of every agent harness it knows about — not just the ones it fully
drives — so it surfaces *untracked* skills sitting in any harness's folder, ready to `topos add` (adopt the
bytes; offline, no account). Three harnesses are first-class: **Claude Code**, **OpenClaw**, and **Hermes
Agent** get live currency — topos places and follows their skills, and updates land at session start. Every
other harness in the table is **discover + add** today: topos tracks and shares the bytes, and full currency
for that harness lands later.

The directory conventions below are sourced from [`vercel-labs/skills`](https://github.com/vercel-labs/skills)
(MIT). User-scope dirs resolve under `$HOME`; project-scope dirs are relative to the directory you run in.

| Harness | Slug | User-scope dir | Project-scope dir | topos support |
|---|---|---|---|---|
| Claude Code | `claude-code` | `~/.claude/skills` † | `.claude/skills` | full (currency) |
| OpenClaw | `openclaw` | `~/.openclaw/skills` ‡ | `skills` | full (currency) |
| Hermes Agent | `hermes-agent` | `~/.hermes/skills` † | `.hermes/skills` | full (currency) |
| AdaL | `adal` | `~/.adal/skills` | `.adal/skills` | discover + add |
| AiderDesk | `aider-desk` | `~/.aider-desk/skills` | `.aider-desk/skills` | discover + add |
| Amp | `amp` | `~/.config/agents/skills` † | `.agents/skills` | discover + add |
| Antigravity | `antigravity` | `~/.gemini/antigravity/skills` | `.agents/skills` | discover + add |
| Antigravity CLI | `antigravity-cli` | `~/.gemini/antigravity-cli/skills` | `.agents/skills` | discover + add |
| AstrBot | `astrbot` | `~/.astrbot/data/skills` | `data/skills` | discover + add |
| Augment | `augment` | `~/.augment/skills` | `.augment/skills` | discover + add |
| Autohand Code CLI | `autohand-code` | `~/.autohand/skills` † | `.autohand/skills` | discover + add |
| Cline | `cline` | `~/.agents/skills` | `.agents/skills` | discover + add |
| CodeArts Agent | `codearts-agent` | `~/.codeartsdoer/skills` | `.codeartsdoer/skills` | discover + add |
| CodeBuddy | `codebuddy` | `~/.codebuddy/skills` | `.codebuddy/skills` | discover + add |
| Codemaker | `codemaker` | `~/.codemaker/skills` | `.codemaker/skills` | discover + add |
| Code Studio | `codestudio` | `~/.codestudio/skills` | `.codestudio/skills` | discover + add |
| Codex | `codex` | `~/.codex/skills` † | `.agents/skills` | discover + add |
| Command Code | `command-code` | `~/.commandcode/skills` | `.commandcode/skills` | discover + add |
| Continue | `continue` | `~/.continue/skills` | `.continue/skills` | discover + add |
| Cortex Code | `cortex` | `~/.snowflake/cortex/skills` | `.cortex/skills` | discover + add |
| Crush | `crush` | `~/.config/crush/skills` | `.crush/skills` | discover + add |
| Cursor | `cursor` | `~/.cursor/skills` | `.agents/skills` | discover + add |
| Deep Agents | `deepagents` | `~/.deepagents/agent/skills` | `.agents/skills` | discover + add |
| Devin for Terminal | `devin` | `~/.config/devin/skills` † | `.devin/skills` | discover + add |
| Dexto | `dexto` | `~/.agents/skills` | `.agents/skills` | discover + add |
| Droid | `droid` | `~/.factory/skills` | `.factory/skills` | discover + add |
| Eve | `eve` | — (project-only) | `agent/skills` | discover + add |
| Firebender | `firebender` | `~/.firebender/skills` | `.agents/skills` | discover + add |
| ForgeCode | `forgecode` | `~/.forge/skills` | `.forge/skills` | discover + add |
| Gemini CLI | `gemini-cli` | `~/.gemini/skills` | `.agents/skills` | discover + add |
| GitHub Copilot | `github-copilot` | `~/.copilot/skills` | `.agents/skills` | discover + add |
| Goose | `goose` | `~/.config/goose/skills` † | `.goose/skills` | discover + add |
| IBM Bob | `bob` | `~/.bob/skills` | `.bob/skills` | discover + add |
| iFlow CLI | `iflow-cli` | `~/.iflow/skills` | `.iflow/skills` | discover + add |
| inference.sh | `inference-sh` | `~/.inferencesh/skills` | `.inferencesh/skills` | discover + add |
| Jazz | `jazz` | `~/.jazz/skills` | `.jazz/skills` | discover + add |
| Junie | `junie` | `~/.junie/skills` | `.junie/skills` | discover + add |
| Kilo Code | `kilo` | `~/.kilocode/skills` | `.kilocode/skills` | discover + add |
| Kimi Code CLI | `kimi-code-cli` | `~/.agents/skills` | `.agents/skills` | discover + add |
| Kiro CLI | `kiro-cli` | `~/.kiro/skills` | `.kiro/skills` | discover + add |
| Kode | `kode` | `~/.kode/skills` | `.kode/skills` | discover + add |
| Lingma | `lingma` | `~/.lingma/skills` | `.lingma/skills` | discover + add |
| Loaf | `loaf` | `~/.agents/skills` | `.agents/skills` | discover + add |
| MCPJam | `mcpjam` | `~/.mcpjam/skills` | `.mcpjam/skills` | discover + add |
| Mistral Vibe | `mistral-vibe` | `~/.vibe/skills` † | `.vibe/skills` | discover + add |
| Moxby | `moxby` | `~/.moxby/skills` | `.moxby/skills` | discover + add |
| Mux | `mux` | `~/.mux/skills` | `.mux/skills` | discover + add |
| Neovate | `neovate` | `~/.neovate/skills` | `.neovate/skills` | discover + add |
| Ona | `ona` | `~/.ona/skills` | `.ona/skills` | discover + add |
| OpenCode | `opencode` | `~/.config/opencode/skills` † | `.agents/skills` | discover + add |
| OpenHands | `openhands` | `~/.openhands/skills` | `.openhands/skills` | discover + add |
| Pi | `pi` | `~/.pi/agent/skills` | `.pi/skills` | discover + add |
| Pochi | `pochi` | `~/.pochi/skills` | `.pochi/skills` | discover + add |
| PromptScript | `promptscript` | — (project-only) | `.agents/skills` | discover + add |
| Qoder | `qoder` | `~/.qoder/skills` | `.qoder/skills` | discover + add |
| Qoder CN | `qoder-cn` | `~/.qoder-cn/skills` | `.qoder/skills` | discover + add |
| Qwen Code | `qwen-code` | `~/.qwen/skills` | `.qwen/skills` | discover + add |
| Reasonix | `reasonix` | `~/.reasonix/skills` | `.reasonix/skills` | discover + add |
| Replit | `replit` | `~/.config/agents/skills` † | `.agents/skills` | discover + add |
| Roo Code | `roo` | `~/.roo/skills` | `.roo/skills` | discover + add |
| Rovo Dev | `rovodev` | `~/.rovodev/skills` | `.rovodev/skills` | discover + add |
| Tabnine CLI | `tabnine-cli` | `~/.tabnine/agent/skills` | `.tabnine/agent/skills` | discover + add |
| Terramind | `terramind` | `~/.terramind/skills` | `.terramind/skills` | discover + add |
| Tinycloud | `tinycloud` | `~/.tinycloud/skills` | `.tinycloud/skills` | discover + add |
| Trae | `trae` | `~/.trae/skills` | `.trae/skills` | discover + add |
| Trae CN | `trae-cn` | `~/.trae-cn/skills` | `.trae/skills` | discover + add |
| Universal | `universal` | `~/.config/agents/skills` † | `.agents/skills` | discover + add |
| Warp | `warp` | `~/.agents/skills` | `.agents/skills` | discover + add |
| Windsurf | `windsurf` | `~/.codeium/windsurf/skills` | `.windsurf/skills` | discover + add |
| Zed | `zed` | `~/.agents/skills` | `.agents/skills` | discover + add |
| Zencoder | `zencoder` | `~/.zencoder/skills` | `.zencoder/skills` | discover + add |
| Zenflow | `zenflow` | `~/.zencoder/skills` | `.zencoder/skills` | discover + add |

† The dir's root is env-overridable (the default shown applies when the variable is unset):
`$CLAUDE_CONFIG_DIR` (Claude Code), `$HERMES_HOME` (Hermes Agent), `$CODEX_HOME` (Codex), `$VIBE_HOME`
(Mistral Vibe), `$AUTOHAND_HOME` (Autohand Code CLI), and `$XDG_CONFIG_HOME` for the `~/.config`-based
harnesses (Amp, Devin for Terminal, Goose, OpenCode, Replit, Universal).
‡ OpenClaw also probes `~/.clawdbot/skills` and `~/.moltbot/skills`.

## Trust & security

A behavior you follow is code and prose that runs inside your agent, so integrity and consent are the whole
point of the tool. A bundle's identity is a **byte-exact sha256** over every file (different bytes are never
"the same"); a version you pin **is** that hash, so what you pin is exactly what you get; and **nothing lands
that was not disclosed and pinned**. Trust sits at the level a team already extends to its git host and CI:
every request is an authenticated principal, every mutation of shared state is attributed and audit-logged,
and access is database policy — a revocation takes effect immediately. Assurance is **visibility** — a fleet
dashboard and one-command revert — rather than client-side cryptography (there is no pointer signing or key
pinning; optional signing can layer on later without a redesign). What Topos does *not* do is judge whether
an approved behavior is safe to run — it guarantees disclosure and integrity, not a sandbox or a second
permission system.

The design behind this — trust boundaries, the consent + sync model — is in
[`ARCHITECTURE.md`](ARCHITECTURE.md); to run a plane safely, see [Self-hosting](#self-hosting-the-plane). To
report a vulnerability, see [`SECURITY.md`](SECURITY.md).

## Self-hosting the plane

The bundled compose file runs the WHOLE product — the web app (the one public surface), the vault (the
Rust plane, internal-network only, no published port), and Postgres:

```sh
docker compose up --build     # the app on http://localhost:3000; the vault stays internal
```

The app serves everything a team touches: sign-in and the dashboard, the review UI, the admin surfaces,
the shareable workspace addresses, and the device API itself (`/api/v1/…` — agents and the `topos` CLI
dial the app, which serves the directory row ops under a scoped database role and forwards byte,
enrollment, and governance ops to the vault). Nothing else needs to be reachable from outside.

### Stand up the first workspace

A brand-new deployment has no workspace yet. Open `http://localhost:3000` in a browser and claim it
(the first-run ownership claim), or mint the first identity in-band:

```sh
docker compose exec plane topos-plane mint-claim --workspace w_acme --display-name "Acme"
```

This prints a one-time `/i/` claim link (a bearer owner capability — store it like a secret). A single
`topos follow <claim-link>` stands the workspace up and seats that device as its first owner, who can then
`publish` and invite teammates (`topos invite <emails…>` seats them; they join by the workspace address —
following an address opens the app's sign-in + device-approval pages, email+password by default, no SMTP
needed).

### Configuration

`docker compose up` works out of the box for a local try-out. For a real (non-localhost) deployment, set:

- `TOPOS_PUBLIC_URL` — the public `https://…` origin (behind your reverse proxy). The workspace
  addresses, the sign-in/verification pages, and the API base the protocol card teaches clients all ride
  it.
- `TOPOS_WEB_AUTH_SECRET` and `TOPOS_INTERNAL_TOKEN` — the app's session secret and the app↔vault
  internal bearer. The compose file ships loud `change-me` defaults; replace both.
- `TOPOS_PLANE_DB_PASSWORD` / `TOPOS_WEB_DB_PASSWORD` — the two database roles' passwords (the plane
  owns the schema; the web role holds column-grain grants and writes policy rows only through the
  guarded SQL functions).
- `TOPOS_PLANE_ADMIN_TOKEN` *(optional)* — enables the review-required gate's operator route.
- `TOPOS_MAIL_SMTP_HOST` / `_PORT` / `_USER` / `_PASS` / `_FROM` *(optional)* — bring your own SMTP relay
  to turn the app's outbound mail on (set all five): invite notices really send, and emailed-passcode
  enrollment becomes available (advertise it with `TOPOS_PLANE_ENROLLMENT_METHOD=passcode`). Unset, mail
  is off and everything still works — invites share the workspace address, enrollment approves in the app.

The bundled `docker-compose.yml` is an annotated starting point (common vars with defaults; optional
features commented out). For the vault's full reference run `topos-plane --help`; the app's variables
are documented in [`web/CLAUDE.md`](web/CLAUDE.md).

Client-side: `TOPOS_DEBUG=1` prints each error's full source chain to stderr (the chain always lands in
`~/.topos/log.jsonl`); `TOPOS_HOME` overrides the `~/.topos` root.

### TLS

The app serves plain HTTP and is designed to sit behind a TLS-terminating reverse proxy (Caddy, nginx,
Traefik, or your platform's load balancer). Point the proxy at `http://web:3000` (the app is the only
public service), set `TOPOS_PUBLIC_URL` to your public `https://…` origin, and let the proxy own
certificates. The vault never needs a public route. (The vault's optional, default-off built-in ACME
listener still exists but is experimental and now redundant in the composed stack — the reverse proxy is
the supported path.)

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

The web app is a separate TypeScript workspace under [`web/`](web/) (React Router, bun):

```sh
cd web && bun install
bun run check      # biome + typecheck + the boundary/token/contract gates
bun test           # vitest (needs a Postgres; see web/CLAUDE.md)
bun run test:e2e   # playwright
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) to propose changes and [`ARCHITECTURE.md`](ARCHITECTURE.md) for the
design.

## License

Apache-2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Copyright 2026 The Topos Authors.
Contributions are inbound = outbound, no CLA (see [`CONTRIBUTING.md`](CONTRIBUTING.md)).
