# Topos

[![CI](https://github.com/topos-sh/topos/actions/workflows/ci.yml/badge.svg)](https://github.com/topos-sh/topos/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Topos keeps every AI agent on your team up to date.

It distributes skills, memory files, and other context bundles to each agent based on its role, and lets anyone contribute improvements back.
With topos you can set up and maintain useful agents for non-technical people or create a cross-project deterministic context for coding agents or create a cloud agent and update it with new behaviors on the fly.

Topos knows all major harnesses: Claude Code, Cursor, Codex, OpenClaw, Hermes and [70+ more](https://topos.sh/docs/harnesses) so you / your team can use different agents.

Use it with [Topos Cloud](https://topos.sh) or [self-host](#self-hosting).

## Quickstart

**1. Set up.** Paste this to the agent you already use - it installs the CLI and logs in, and
the one browser approval stays yours:

```text
Set up Topos for us: fetch https://topos.sh/agent and follow it.
```

Or by hand (macOS and Linux, WSL2 on Windows; no sudo):

```sh
curl -fsSL https://topos.sh/install | sh
topos login
```

Your browser opens: sign in, pick your workspace - or create it right there - and one click
connects. A workspace is your team's shared home for skills; its address is
`https://topos.sh/<name>`. Already invited? The invitation link works as the address:
`topos login <invite-url>`.

That is the whole setup: this machine receives everything the workspace has for you, and
updates apply silently at the start of each agent session.

**2. Share a skill.** Point `publish` at any skill folder you already have - here, a Claude
Code one. A bare run is a preview - it prints what would happen and changes nothing; `--yes`
applies it:

```sh
topos publish ~/.claude/skills/pr-describe
topos publish pr-describe --yes
```

The skill is now the team's: one name, one history, one version everyone gets. Every teammate's
machine picks it up on its own - they run nothing.

**3. Bring the team in.**

```sh
topos invite dana@acme.com --yes
```

The invitation mail carries everything they need, including a line to paste to their agent.
(Owners only. Self-hosted installs need SMTP configured first.)

## Everyday use

The CLI is built for agents to drive, not for hands - using it directly works, but can be
frustrating. Agents already know it: the built-in `topos` skill lands in their skills
directories at setup. So tell your agent what you want and let it run the commands. The goal is
for Topos to be invisible: all of this happens while everyone just works. In the flows below,
`# Ask your agent:` lines are the natural-language version of the commands beneath them.

### Steer a non-technical teammate's agent

Dana in sales never opens a terminal, but her agent could be doing her CRM follow-ups. Your
team's `sales` channel carries the CRM skills (channels are created in the browser). Invite
her - from the workspace's Members page in the browser, or:

```sh
# Ask your agent: "Invite dana@acme.com and set her up with our sales skills."
topos invite dana@acme.com --channel sales --yes
```

The mail she receives carries one line for her agent, and she pastes it:

```text
Set up Topos for us: fetch https://topos.sh/agent and follow it. Our invite: https://topos.sh/acme/invite/…
```

Her agent installs the CLI, joins the workspace, and subscribes her to `sales` - one browser
visit is hers: create her account, then approve the machine. The channel's skills are on her
machine before her next session.

Later the CRM changes, so you update the skill in the channel:

```sh
# Ask your agent: "The CRM adds a discount field - update crm-follow-up."
topos publish crm-follow-up -m "Fill the new discount field" --yes
```

At Dana's next session her agent already works the new way - she did nothing, and you never
touched her machine. Protect the skill, and her agent's improvements come back as proposals
you approve.

### Pin a project's skills

A backend team wants every agent working in its repo - teammates' and CI's - on the same
skills:

```sh
# Ask your agent: "Set this repo up with our code-review standard, our
# db-migrations skill, and find-skills from vercel-labs."
topos add code-review
topos add db-migrations
topos add vercel-labs/skills/find-skills --yes
```

The `--yes` confirms a GitHub source this machine has not used before. The commands install the
skills and write `topos.toml` at the repo root - commit it, and every machine in the checkout
converges on the same set. The entries it adds:

```toml
[bundles]
"topos.sh/acme/code-review" = "*"                      # the team's current version
"topos.sh/acme/db-migrations" = "*"
"github.com/vercel-labs/skills/find-skills" = "*"      # tracks the repo; moves on `topos update`
```

One command refreshes every skill on the machine - agents with hooks run it themselves at
session start ([which agents](https://topos.sh/docs/harnesses)):

```sh
topos update
```

### Roll out an internal tool change company-wide

The internal customer-lookup service ships v2 - new endpoints, token auth. One publish updates
every agent that carries the skill:

```sh
# Ask your agent: "Our customer-lookup API moved to v2 - update the skill and roll it out."
topos publish customer-lookup -m "v2 endpoints, token auth" --yes
```

Next session, Support, Sales, and Engineering agents call v2. On a protected skill the publish
lands as a proposal, and a reviewer approves it:

```sh
topos review customer-lookup@<version> --approve
```

If v2 misbehaves, one command moves everyone back:

```sh
topos revert customer-lookup --to <version> --yes
```

### Keep a cloud agent current

A scheduled agent triages new support tickets every night - labels them, routes escalations -
with no human at the keyboard. Enroll it once, somewhere `~/.topos` persists (the login is one
browser approval; naming the workspace goes straight to it):

```sh
curl -fsSL https://topos.sh/install | sh
topos login acme
```

Still on that machine, provision it with its skill and a dedicated channel - anything the team
later places in that channel reaches this agent too:

```sh
# Ask your agent: "Set this machine up with ticket-triage and the triage-bot channel."
topos add -g ticket-triage
topos add -g @acme/channels/triage-bot
```

Every run then starts with a refresh:

```sh
topos update
```

From here its behavior is steered from anywhere, live at the next run - no redeploy. One way:
update a skill it carries:

```sh
# Ask your agent: "Triage keeps missing escalations - fix the ticket-triage skill."
topos publish ticket-triage -m "Escalate anything from enterprise accounts" --yes
```

The other: give it a new capability by publishing into its channel:

```sh
# Ask your agent: "Write a skill for duplicate-ticket detection and publish it to triage-bot."
topos publish dedupe-tickets --to triage-bot --yes
```

One rule across all commands: anything that reaches other people, throws away local work, or
adds from a GitHub repository you have not used before previews first - a bare run prints what
would change and changes nothing, and `--yes` applies it. Everything else applies immediately
and prints an undo command. Agents use the same commands with `--json`, which never prompts.

## Docs

**[topos.sh/docs](https://topos.sh/docs)** - quickstart, getting skills, publishing, review,
running a team, self-hosting, and the full
[CLI reference](https://topos.sh/docs/cli). Written for humans and agents alike:
every page is also plain markdown at its URL + `.md`, [topos.sh/agent](https://topos.sh/agent)
is the setup walkthrough an agent can follow on its own, and
[topos.sh/docs/agents](https://topos.sh/docs/agents) covers the JSON output and consent rules.

Agents on an enrolled machine need none of this: the built-in `topos` skill in their skills
directories already covers what Topos is, the commands, and when to offer sharing something
back. If your repo has an `AGENTS.md`, one line routing agents to that skill is enough:

```md
Team skills here are managed by topos - read the `topos` skill before working with them (not installed? https://topos.sh/agent).
```

## Self-hosting

The whole product runs from one compose file, and your workspace address is simply your
origin. Both images are prebuilt and multi-arch, so there is nothing to clone and nothing to
compile.

```sh
curl -fsSL https://topos.sh/compose.yml -o docker-compose.yml
printf 'TOPOS_WEB_AUTH_SECRET=%s\nTOPOS_INTERNAL_TOKEN=%s\n' \
  "$(openssl rand -hex 32)" "$(openssl rand -hex 32)" > .env
docker compose up -d
```

The two secrets are required even for a laptop try-out. Open `http://localhost:3000` in a
browser - the first browser visit creates the workspace and prints a one-time setup link to the
logs:

```sh
docker compose logs web | grep 'Finish setup:'
```

Open that link and create the first account - it makes you the owner. Then connect a machine
and use it exactly like the quickstart above:

```sh
topos login http://localhost:3000
```

That is a complete single-team install. For a real deployment - reverse proxy and TLS, mail for
invitations, upgrades, backups, your own Postgres - follow the guide:
[topos.sh/docs/self-host](https://topos.sh/docs/self-host).

## Contributing

Build and test instructions are in [`CONTRIBUTING.md`](CONTRIBUTING.md); the design is in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## License

Apache-2.0 - see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). No CLA; contributions are under
the same license.
