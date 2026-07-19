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
curl -fsSL https://topos.sh/install | sh
```

**Share a skill with your team** — from a machine that has the skill locally:

```sh
topos add ~/.claude/skills/pr-describe   # adopt it (offline; no account)
topos follow https://topos.sh/acme       # enroll this device in your workspace; approve in the browser
topos publish pr-describe                # move `current` to your draft; prints the share line
```

Create your workspace in the browser (at [topos.sh](https://topos.sh), or self-host — see below), then
`topos follow <address>` enrolls this device (approve in the browser; it completes on its own). `publish`
needs an enrolled device — un-enrolled, it refuses and tells you to `follow` your workspace address first.
Each publish prints the workspace **address** (`https://topos.sh/<name>`) teammates paste to follow. To pin
the exact bytes being shipped, add a `@<digest>` suffix (`topos publish pr-describe@<digest>`, where
`topos list --json` prints each digest) — the publish then refuses on any mismatch.

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
curl -fsSL https://topos.sh/install | sh
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

### The built-in `topos` skill

Agents shouldn't need this README — so topos teaches them itself, with its own mechanism. A **built-in
skill named `topos`** ships inside the binary and lands in your agents' skill directories the moment
topos wires into a harness: what topos is, how to check what's managed (`topos list`), how updates
arrive, and how to share an improvement back (`publish` / `publish --propose`) — plus the complete
generated verb reference (the same bytes as `docs/cli.md`, rendered from the CLI itself so it can
never drift). It re-syncs with the binary on every update sweep; hand edits are overwritten (your
*other* skills' drafts are sacred — this one documents the binary). Don't want it?
`topos remove topos --yes` opts the device out durably; `topos follow topos` brings it back. The name
`topos` is reserved everywhere, so no workspace skill can ever shadow it.

The skill's source lives at the top of this repo — [`skills/topos/`](skills/topos/) — so it also
works as a plain downloadable skill with no topos installed: `npx skills add topos-sh/topos` places
the same three files (`SKILL.md`, the generated `reference.md`, and `INSTALL.md`, which covers
installing the CLI). If you later install topos, one explicit `topos follow topos --yes` hands the
downloaded copy to it — recognized by its provenance marker, its bytes snapshotted first, kept
current from then on. Nothing takes over a pre-existing directory silently.

## Skill discovery across harnesses

`topos list` scans the skills directory of every agent harness it knows about — not just the ones it fully
drives — so it surfaces *untracked* skills sitting in any harness's folder, ready to `topos add` (adopt the
bytes; offline, no account). Support comes in three tiers:

- **auto-update** — topos installs an update trigger inside the harness itself (a session-start hook, or a
  scheduled job where that is what the harness offers), so followed skills refresh silently and are current
  before the agent uses them. Where marked *, the harness asks its own one-time confirmation before it will
  run the trigger; until you grant it, that harness behaves like the **delivery** tier.
- **delivery** — topos installs no trigger inside that harness, but every followed skill is still placed
  into its skills dir whenever the harness is detected (one shared `~/.agents/skills` copy where the harness
  reads that dir, a native copy otherwise) and refreshed by every update sweep on the machine — whether a
  hook in an auto-update harness, a scheduled job, or a manual `topos update` ran it. The harness picks up
  changes on its own scan (many rescan at session start).
- **discover + add** — project-scoped conventions with no user-level dir: topos finds and adopts skills
  there, but has no machine-wide place to deliver into.

The directory conventions below are sourced from [`vercel-labs/skills`](https://github.com/vercel-labs/skills)
(MIT). User-scope dirs resolve under `$HOME`; project-scope dirs are relative to the directory you run in.

| Harness | Slug | User-scope dir | Project-scope dir | topos support |
|---|---|---|---|---|
| Claude Code | `claude-code` | `~/.claude/skills` † | `.claude/skills` | auto-update |
| OpenClaw | `openclaw` | `~/.openclaw/skills` ‡ | `skills` | auto-update |
| Hermes Agent | `hermes-agent` | `~/.hermes/skills` † | `.hermes/skills` | auto-update |
| AdaL | `adal` | `~/.adal/skills` | `.adal/skills` | delivery |
| AiderDesk | `aider-desk` | `~/.aider-desk/skills` | `.aider-desk/skills` | delivery |
| Amp | `amp` | `~/.config/agents/skills` † | `.agents/skills` | auto-update |
| Antigravity | `antigravity` | `~/.gemini/antigravity/skills` | `.agents/skills` | delivery |
| Antigravity CLI | `antigravity-cli` | `~/.gemini/antigravity-cli/skills` | `.agents/skills` | delivery |
| AstrBot | `astrbot` | `~/.astrbot/data/skills` | `data/skills` | delivery |
| Augment | `augment` | `~/.augment/skills` | `.augment/skills` | delivery |
| Autohand Code CLI | `autohand-code` | `~/.autohand/skills` † | `.autohand/skills` | delivery |
| Cline | `cline` | `~/.agents/skills` | `.agents/skills` | auto-update |
| CodeArts Agent | `codearts-agent` | `~/.codeartsdoer/skills` | `.codeartsdoer/skills` | delivery |
| CodeBuddy | `codebuddy` | `~/.codebuddy/skills` | `.codebuddy/skills` | delivery |
| Codemaker | `codemaker` | `~/.codemaker/skills` | `.codemaker/skills` | delivery |
| Code Studio | `codestudio` | `~/.codestudio/skills` | `.codestudio/skills` | delivery |
| Codex | `codex` | `~/.codex/skills` † | `.agents/skills` | auto-update * |
| Command Code | `command-code` | `~/.commandcode/skills` | `.commandcode/skills` | delivery |
| Continue | `continue` | `~/.continue/skills` | `.continue/skills` | delivery |
| Cortex Code | `cortex` | `~/.snowflake/cortex/skills` | `.cortex/skills` | delivery |
| Crush | `crush` | `~/.config/crush/skills` | `.crush/skills` | delivery |
| Cursor | `cursor` | `~/.cursor/skills` | `.agents/skills` | auto-update |
| Deep Agents | `deepagents` | `~/.deepagents/agent/skills` | `.agents/skills` | delivery |
| Devin for Terminal | `devin` | `~/.config/devin/skills` † | `.devin/skills` | delivery |
| Dexto | `dexto` | `~/.agents/skills` | `.agents/skills` | delivery |
| Droid | `droid` | `~/.factory/skills` | `.factory/skills` | auto-update |
| Eve | `eve` | — (project-only) | `agent/skills` | discover + add |
| Firebender | `firebender` | `~/.firebender/skills` | `.agents/skills` | delivery |
| ForgeCode | `forgecode` | `~/.forge/skills` | `.forge/skills` | delivery |
| Gemini CLI | `gemini-cli` | `~/.gemini/skills` | `.agents/skills` | auto-update * |
| GitHub Copilot | `github-copilot` | `~/.copilot/skills` | `.agents/skills` | auto-update |
| Goose | `goose` | `~/.config/goose/skills` † | `.goose/skills` | auto-update * |
| IBM Bob | `bob` | `~/.bob/skills` | `.bob/skills` | delivery |
| iFlow CLI | `iflow-cli` | `~/.iflow/skills` | `.iflow/skills` | delivery |
| inference.sh | `inference-sh` | `~/.inferencesh/skills` | `.inferencesh/skills` | delivery |
| Jazz | `jazz` | `~/.jazz/skills` | `.jazz/skills` | delivery |
| Junie | `junie` | `~/.junie/skills` | `.junie/skills` | delivery |
| Kilo Code | `kilo` | `~/.kilocode/skills` | `.kilocode/skills` | delivery |
| Kimi Code CLI | `kimi-code-cli` | `~/.agents/skills` | `.agents/skills` | delivery |
| Kiro CLI | `kiro-cli` | `~/.kiro/skills` | `.kiro/skills` | delivery |
| Kode | `kode` | `~/.kode/skills` | `.kode/skills` | delivery |
| Lingma | `lingma` | `~/.lingma/skills` | `.lingma/skills` | delivery |
| Loaf | `loaf` | `~/.agents/skills` | `.agents/skills` | delivery |
| MCPJam | `mcpjam` | `~/.mcpjam/skills` | `.mcpjam/skills` | delivery |
| Mistral Vibe | `mistral-vibe` | `~/.vibe/skills` † | `.vibe/skills` | delivery |
| Moxby | `moxby` | `~/.moxby/skills` | `.moxby/skills` | delivery |
| Mux | `mux` | `~/.mux/skills` | `.mux/skills` | delivery |
| Neovate | `neovate` | `~/.neovate/skills` | `.neovate/skills` | delivery |
| Ona | `ona` | `~/.ona/skills` | `.ona/skills` | delivery |
| OpenCode | `opencode` | `~/.config/opencode/skills` † | `.agents/skills` | auto-update |
| OpenHands | `openhands` | `~/.openhands/skills` | `.openhands/skills` | delivery |
| Pi | `pi` | `~/.pi/agent/skills` | `.pi/skills` | delivery |
| Pochi | `pochi` | `~/.pochi/skills` | `.pochi/skills` | delivery |
| PromptScript | `promptscript` | — (project-only) | `.agents/skills` | discover + add |
| Qoder | `qoder` | `~/.qoder/skills` | `.qoder/skills` | delivery |
| Qoder CN | `qoder-cn` | `~/.qoder-cn/skills` | `.qoder/skills` | delivery |
| Qwen Code | `qwen-code` | `~/.qwen/skills` | `.qwen/skills` | delivery |
| Reasonix | `reasonix` | `~/.reasonix/skills` | `.reasonix/skills` | delivery |
| Replit | `replit` | `~/.config/agents/skills` † | `.agents/skills` | delivery |
| Roo Code | `roo` | `~/.roo/skills` | `.roo/skills` | delivery |
| Rovo Dev | `rovodev` | `~/.rovodev/skills` | `.rovodev/skills` | delivery |
| Tabnine CLI | `tabnine-cli` | `~/.tabnine/agent/skills` | `.tabnine/agent/skills` | delivery |
| Terramind | `terramind` | `~/.terramind/skills` | `.terramind/skills` | delivery |
| Tinycloud | `tinycloud` | `~/.tinycloud/skills` | `.tinycloud/skills` | delivery |
| Trae | `trae` | `~/.trae/skills` | `.trae/skills` | delivery |
| Trae CN | `trae-cn` | `~/.trae-cn/skills` | `.trae/skills` | delivery |
| Universal | `universal` | `~/.config/agents/skills` † | `.agents/skills` | — (the shared dir itself) |
| Warp | `warp` | `~/.agents/skills` | `.agents/skills` | delivery |
| Windsurf | `windsurf` | `~/.codeium/windsurf/skills` | `.windsurf/skills` | delivery |
| Zed | `zed` | `~/.agents/skills` | `.agents/skills` | delivery |
| ZCode | `zcode` | `~/.zcode/skills` | `.zcode/skills` | delivery |
| Zencoder | `zencoder` | `~/.zencoder/skills` | `.zencoder/skills` | delivery |
| Zenflow | `zenflow` | `~/.zencoder/skills` | `.zencoder/skills` | delivery |

† The dir's root is env-overridable (the default shown applies when the variable is unset):
`$CLAUDE_CONFIG_DIR` (Claude Code), `$HERMES_HOME` (Hermes Agent), `$CODEX_HOME` (Codex), `$VIBE_HOME`
(Mistral Vibe), `$AUTOHAND_HOME` (Autohand Code CLI), and `$XDG_CONFIG_HOME` for the `~/.config`-based
harnesses (Amp, Devin for Terminal, Goose, OpenCode, Replit, Universal).
‡ OpenClaw also probes `~/.clawdbot/skills` and `~/.moltbot/skills`.
* The trigger is installed, but this harness requires its own one-time approval before running it
(Codex trusts hooks in-app via `/hooks`; Gemini CLI confirms new hooks; Goose enables plugins itself).
topos reports the trigger honestly as not-yet-active until then.

## Trust & security

A behavior you follow is code and prose that runs inside your agent, so integrity and consent are the whole
point of the tool. A bundle's identity is a **byte-exact sha256** over every file (different bytes are never
"the same"); a version you pin **is** that hash, so what you pin is exactly what you get; and **nothing lands
that was not disclosed and pinned**. Trust sits at the level a team already extends to its git host and CI:
every request is authenticated (a signed-in person or an enrolled device), every mutation of shared state is
attributed and audit-logged, and access is database policy — a revocation takes effect immediately. Assurance is **visibility** — a fleet
dashboard and one-command revert — rather than client-side cryptography (there is no pointer signing or key
pinning; optional signing can layer on later without a redesign). What Topos does *not* do is judge whether
an approved behavior is safe to run — it guarantees disclosure and integrity, not a sandbox or a second
permission system.

The design behind this — trust boundaries, the consent + sync model — is in
[`ARCHITECTURE.md`](ARCHITECTURE.md); to run it safely, see [Self-hosting](#self-hosting). To
report a vulnerability, see [`SECURITY.md`](SECURITY.md).

## Self-hosting

The bundled compose file runs the WHOLE product — the web app (the one public surface), the vault (the
Rust plane, internal-network only, no published port), and Postgres:

```sh
docker compose up --build     # the app on http://localhost:3000; the vault stays internal
```

The app serves everything a team touches: sign-in and the dashboard, the review UI, the admin surfaces,
the shareable workspace addresses, and the device API itself (`/api/v1/…` — agents and the `topos` CLI dial
the app). The app owns identity and the whole directory in its own database schema; only the byte and
pointer operations of a publish forward to the vault over an internal network lane. Nothing else needs to
be reachable from outside.

### First run: claim the workspace

The first boot mints your workspace and prints **one** setup link to the app logs:

```
→ Finish setup: http://localhost:3000/claim?code=…
```

Open it in a browser and create the first account (email + password) — that seats you as the workspace
**owner**. (In CI or an automated deploy, preset the code with `TOPOS_SETUP_CODE` to skip reading the logs;
`TOPOS_SETUP_LINK_FILE` also mirrors the line to a file.) The link dies on first use, and the code is only
ever stored as its hash.

From there you `publish` (after `topos follow <your-address>` enrolls a device) and grow the team:

- **Invite teammates** once SMTP is armed (see below): `topos invite <emails…>`, or the roster page. Each
  person signs up through the invite mail, then runs `topos follow <workspace-address>` and approves the new
  device at `<origin>/verify` (behind their password). Approval mints that device's one credential; updates
  then land at session start.
- **No SMTP?** Registration stays closed by design — the claim owner is the only account. Flip
  `registration = 'open'` on the workspace policy page to let anyone with the address sign up (off by
  default), or arm SMTP to invite.

### Configuration

`docker compose up` works out of the box for a local try-out. For a real (non-localhost) deployment, set:

- `TOPOS_PUBLIC_URL` — the public `https://…` origin (behind your reverse proxy). The workspace addresses,
  the sign-in/verification pages, the printed setup link, and the API base the protocol card teaches clients
  all ride it.
- `TOPOS_WEB_AUTH_SECRET` and `TOPOS_INTERNAL_TOKEN` — the app's session-signing secret (≥ 32 chars) and
  the app↔vault internal bearer. The compose file ships loud `change-me` defaults; replace both.
- `TOPOS_PLANE_DB_PASSWORD` / `TOPOS_WEB_DB_PASSWORD` — the two database roles' passwords. There is one role
  per application, each owning its own schema: the vault owns `plane` (byte custody), the app owns `web`
  (identity + the directory) and reads `plane` read-only.
- `TOPOS_WORKSPACE_NAME` — the first workspace's address slug (renameable later in the product; defaults to
  `team`). `TOPOS_SETUP_CODE` presets the claim code for CI/IaC; `TOPOS_SETUP_LINK_FILE` mirrors the printed
  setup line to a file.
- `TOPOS_MAIL_SMTP_HOST` / `_PORT` / `_USER` / `_PASS` / `_FROM` *(optional)* — bring your own SMTP relay,
  all five or none. Armed, outbound mail turns on: invites really send (and the invited sign-up verifies
  through the mailbox before its seat binds), and password-reset mail works. Unset, mail is off and the core
  loop still works — sign-in, publishing, and the claim ceremony need no mail. A mail-less solo owner who
  forgets their password runs the one-shot `web/scripts/mint-recovery-code.mjs` in the container to print a
  recovery code.

The bundled `docker-compose.yml` is an annotated starting point (common vars with defaults; optional
features commented out). For the vault's full reference run `topos-plane --help`; the app's variables are
documented in [`web/CLAUDE.md`](web/CLAUDE.md).

Client-side: `TOPOS_DEBUG=1` prints each error's full source chain to stderr (the chain always lands in
`~/.topos/log.jsonl`); `TOPOS_HOME` overrides the `~/.topos` root.

### Backups

Two volumes hold all durable state, and the **database is the source of truth for everything except bytes**:

- **The Postgres volume** carries identity, the directory, policy, proposals, receipts, and audit — back it
  up with `pg_dump` (or a volume snapshot). This is the one to guard.
- **The vault's `plane-data` volume** is only the git object store and the large-object store — the
  content-addressed bytes of every version. Snapshot it too, ideally **before** the database so no pointer
  can name a byte the snapshot missed.

Nothing else lives on disk — there are no secret files to back up beside the volumes.

At rest, the plane's signing key and enrollment secret are plaintext `0600` files inside its data
volume. Disk or volume encryption is the operator's responsibility. If those files are lost, enrolled
devices must re-enroll.

### Bring your own Postgres

To point at a managed/external database instead of the bundled `db`, first create the two roles, the two
schemas, the search paths, and the cross-lane grants — `scripts/compose-init-db.sh` is the exact recipe,
runnable once against your server. Then set each service's `DATABASE_URL` (the vault connects as
`topos_plane`, the app as `topos_web`) and start with `--no-deps` so the bundled `db` stays down. A
networked Postgres should append `?sslmode=require`. Each application migrates its own schema on startup.

### TLS

The app serves plain HTTP and is designed to sit behind a TLS-terminating reverse proxy (Caddy, nginx,
Traefik, or your platform's load balancer). Point the proxy at `http://web:3000` (the app is the only public
service), set `TOPOS_PUBLIC_URL` to your public `https://…` origin, and let the proxy own certificates. The
vault never needs a public route.

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
bun run check      # biome + typecheck + the boundary/email/token/contract gates
bun run test       # vitest (needs a Postgres; see web/CLAUDE.md — not `bun test`, bun's own runner)
bun run test:e2e   # playwright
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) to propose changes and [`ARCHITECTURE.md`](ARCHITECTURE.md) for the
design.

## License

Apache-2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Copyright 2026 The Topos Authors.
Contributions are inbound = outbound, no CLA (see [`CONTRIBUTING.md`](CONTRIBUTING.md)).
