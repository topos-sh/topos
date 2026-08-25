# `topos` command reference

> GENERATED from the `clap` command tree by `cargo xtask gen-cli-ref` — do not hand-edit. Change the CLI, re-run the command, and commit the result; the `--check` variant is the drift gate.

Every command prints human-readable text on a terminal, or exactly one JSON object with `--json` (for agents and scripts — it never prompts).

One rule covers all of them: **a command that reaches other people, discards local work, or trusts a new source shows a preview first.** Run it bare to see exactly what would happen, then add `--yes` to apply. Everything else applies immediately and prints what changed, along with the command that undoes it.

Exit codes: `0` — success. `1` — the operation was refused or failed (with `--json`, the `error` object says which and how to fix it). `2` — the command line itself was invalid.

`topos verify` reports what it found through its exit code as well, so a script can branch without parsing anything: `0` — the server is responding. `3` — the server asks for a sign-in, which is healthy; the line (and, under `--json`, `sign_in`) says whether your agent app can complete that sign-in on first use or somebody has to register a client or a token by hand first. `4` — the server is not reachable from this machine. `5` — something answered, but not as an MCP server. It still uses `1` for a refusal (no such bundle, the name is ambiguous, the bundle is a skill) and `2` for a bad command line.

## The `--json` envelope

Every `--json` run prints one object on stdout: `schema_version` (2), `command`, `ok`, a per-command `data` payload, `warnings`, `messages`, `next_actions`, and — on `ok: false` — an `error` (`code`, `outcome`, `retryable`, plus its own `next_actions`). Each `next_actions` entry is a ready-to-run step: `argv` is a complete command; `needs` lists any `<placeholder>` tokens you must substitute first; `mutates`, `needs_network`, and `risk_note` are safety metadata (absent means unknown). Treat `code` as an open vocabulary — run an unfamiliar action by its `argv` rather than rejecting it.

`messages` is the typed line channel: one `{code, kind, text}` entry per line, where `kind` is `failure` · `decision` · `advisory` · `disclosure`, `code` is the open SCREAMING_SNAKE vocabulary (absent when the producer has none), and `text` is the sentence a person reads. Machine-readable message codes now ride `messages[].code` — each entry is `{code, kind, text}`. The `warnings` array is unchanged in shape for now and will be retired in a future contract version; match on `messages[].code` rather than `warnings[]` string prefixes.

The full JSON-Schemas live under `contracts/schemas/` in the repository, with golden examples under `contracts/fixtures/json/`; the agents guide (topos.sh/docs/agents) shows worked examples.

## Agents (`-a`) and where files land

`-a <slug>` places a bundle in that agent's folder from the table below — the machine folder when you work machine-wide, the project folder when you work in a project. `--dest <folder>` names a folder literally instead. Where a row lists more than one machine folder, `-a` writes the first.

Some folders are cross-agent conventions that several agents read, so each is named here once rather than repeated on every row:

- `~/.agents/skills` — the machine folder read by cline, dexto, kimi-code-cli, loaf, warp, zed.
- `~/.config/agents/skills` — the machine folder read by amp, replit, universal.
- `.agents/skills` — the project folder read by amp, antigravity, antigravity-cli, cline, codex, cursor, deepagents, dexto, firebender, gemini-cli, github-copilot, kimi-code-cli, loaf, opencode, promptscript, replit, universal, warp, zed. A `shared` cell below means this folder.

| Agent | Machine folder | Project folder |
|---|---|---|
| `adal` | `~/.adal/skills` | `.adal/skills` |
| `aider-desk` | `~/.aider-desk/skills` | `.aider-desk/skills` |
| `antigravity` | `~/.gemini/antigravity/skills` | shared |
| `antigravity-cli` | `~/.gemini/antigravity-cli/skills` | shared |
| `astrbot` | `~/.astrbot/data/skills` | `data/skills` |
| `augment` | `~/.augment/skills` | `.augment/skills` |
| `autohand-code` | `~/.autohand/skills` | `.autohand/skills` |
| `bob` | `~/.bob/skills` | `.bob/skills` |
| `claude-code` | `~/.claude/skills` | `.claude/skills` |
| `codearts-agent` | `~/.codeartsdoer/skills` | `.codeartsdoer/skills` |
| `codebuddy` | `~/.codebuddy/skills` | `.codebuddy/skills` |
| `codemaker` | `~/.codemaker/skills` | `.codemaker/skills` |
| `codestudio` | `~/.codestudio/skills` | `.codestudio/skills` |
| `codex` | `~/.codex/skills` | shared |
| `command-code` | `~/.commandcode/skills` | `.commandcode/skills` |
| `continue` | `~/.continue/skills` | `.continue/skills` |
| `cortex` | `~/.snowflake/cortex/skills` | `.cortex/skills` |
| `crush` | `~/.config/crush/skills` | `.crush/skills` |
| `cursor` | `~/.cursor/skills` | shared |
| `deepagents` | `~/.deepagents/agent/skills` | shared |
| `devin` | `~/.config/devin/skills` | `.devin/skills` |
| `droid` | `~/.factory/skills` | `.factory/skills` |
| `eve` | — | `agent/skills` |
| `firebender` | `~/.firebender/skills` | shared |
| `forgecode` | `~/.forge/skills` | `.forge/skills` |
| `gemini-cli` | `~/.gemini/skills` | shared |
| `github-copilot` | `~/.copilot/skills` | shared |
| `goose` | `~/.config/goose/skills` | `.goose/skills` |
| `grok` | `~/.grok/skills` | `.grok/skills` |
| `hermes-agent` | `~/.hermes/skills` | `.hermes/skills` |
| `iflow-cli` | `~/.iflow/skills` | `.iflow/skills` |
| `inference-sh` | `~/.inferencesh/skills` | `.inferencesh/skills` |
| `jazz` | `~/.jazz/skills` | `.jazz/skills` |
| `junie` | `~/.junie/skills` | `.junie/skills` |
| `kilo` | `~/.kilocode/skills` | `.kilocode/skills` |
| `kimchi` | `~/.config/kimchi/harness/skills` | `.kimchi/skills` |
| `kiro-cli` | `~/.kiro/skills` | `.kiro/skills` |
| `kode` | `~/.kode/skills` | `.kode/skills` |
| `lingma` | `~/.lingma/skills` | `.lingma/skills` |
| `mcpjam` | `~/.mcpjam/skills` | `.mcpjam/skills` |
| `minimax-code` | `~/.minimax/skills` | `.minimax/skills` |
| `mistral-vibe` | `~/.vibe/skills` | `.vibe/skills` |
| `moxby` | `~/.moxby/skills` | `.moxby/skills` |
| `mux` | `~/.mux/skills` | `.mux/skills` |
| `neovate` | `~/.neovate/skills` | `.neovate/skills` |
| `ona` | `~/.ona/skills` | `.ona/skills` |
| `openclaw` | `~/.openclaw/skills` | `skills` |
| `opencode` | `~/.config/opencode/skills` | shared |
| `openhands` | `~/.openhands/skills` | `.openhands/skills` |
| `pi` | `~/.pi/agent/skills` | `.pi/skills` |
| `pochi` | `~/.pochi/skills` | `.pochi/skills` |
| `qoder` | `~/.qoder/skills` | `.qoder/skills` |
| `qoder-cn` | `~/.qoder-cn/skills` | `.qoder/skills` |
| `qwen-code` | `~/.qwen/skills` | `.qwen/skills` |
| `reasonix` | `~/.reasonix/skills` | `.reasonix/skills` |
| `roo` | `~/.roo/skills` | `.roo/skills` |
| `rovodev` | `~/.rovodev/skills` | `.rovodev/skills` |
| `tabnine-cli` | `~/.tabnine/agent/skills` | `.tabnine/agent/skills` |
| `terramind` | `~/.terramind/skills` | `.terramind/skills` |
| `tinycloud` | `~/.tinycloud/skills` | `.tinycloud/skills` |
| `trae` | `~/.trae/skills` | `.trae/skills` |
| `trae-cn` | `~/.trae-cn/skills` | `.trae/skills` |
| `windsurf` | `~/.codeium/windsurf/skills` | `.windsurf/skills` |
| `zcode` | `~/.zcode/skills` | `.zcode/skills` |
| `zencoder` | `~/.zencoder/skills` | `.zencoder/skills` |
| `zenflow` | `~/.zencoder/skills` | `.zencoder/skills` |

A `—` means the agent has no folder at that scope. An agent absent from the table reads only the shared folders named above.

### MCP server config files

An MCP-server bundle arrives as an entry in the agent's own MCP config rather than a skills folder. `-a <slug>` picks the file below; `--dest <file>` names one literally. Claude Code's machine entry is a topos-owned plugin folder, not a single file. A machine path starting `<application support>/` is the one directory that differs by platform — `~/Library/Application Support` on macOS, `%APPDATA%` on Windows, `~/.config` elsewhere; `-a <slug>` resolves it for the machine it runs on, and `--dest` takes that resolved path.

| Agent | Machine config file | Project config file |
|---|---|---|
| `claude-code` | `~/.claude/skills/topos-mcp/.mcp.json` | `.mcp.json` |
| `openclaw` | `~/.openclaw/openclaw.json` | — |
| `cline` | `~/.cline/data/settings/cline_mcp_settings.json` | — |
| `codex` | `~/.codex/config.toml` | `.codex/config.toml` |
| `cursor` | `~/.cursor/mcp.json` | `.cursor/mcp.json` |
| `gemini-cli` | `~/.gemini/settings.json` | `.gemini/settings.json` |
| `github-copilot` | `~/.copilot/mcp-config.json` | — |
| `goose` | `~/.config/goose/config.yaml` | — |
| `hermes-agent` | `~/.hermes/config.yaml` | — |
| `opencode` | `~/.config/opencode/opencode.json` | `opencode.json` |
| `roo` | `<application support>/Code/User/globalStorage/rooveterinaryinc.roo-cline/settings/mcp_settings.json` | `.roo/mcp.json` |
| `windsurf` | `~/.codeium/windsurf/mcp_config.json` | — |
| `zed` | `~/.config/zed/settings.json` | — |
| `vscode` | `<application support>/Code/User/mcp.json` | `.vscode/mcp.json` |
| `lm-studio` | `~/.lmstudio/mcp.json` | — |
| `claude-desktop` | `<application support>/Claude/claude_desktop_config.json` | — |

## Global options

These work before or after any command.

| Flag | What it does |
|---|---|
| `--json` | Print one JSON object instead of human text — for agents and scripts. Never prompts |
| `--workspace <WORKSPACE>` | Pick which workspace to act in when this machine is logged into more than one. Takes the workspace's name or id, and beats the machine default (`topos workspace use <name>`) for this one command. A command always acts on ONE workspace, never all of them: a bundle this machine already tracks acts on the workspace it came from, and anything else acts on the one this flag, `TOPOS_WORKSPACE`, or the default names. With a single login it is inferred |

## Documents topos prints

Each of these is the whole command: it prints one document and exits, reading nothing and dialing nothing.

| Flag | What it does |
|---|---|
| `--skill` | Print the built-in topos skill — the document that teaches an agent to drive this CLI — and exit. Works anywhere, including where topos cannot place it for your agents |
| `--schema` | Print the JSON Schema for every shape `--json` can answer with, as one document, and exit |

## Everyday commands

These act on this machine only.

### `topos status`

Check topos's health: your workspace logins and sessions, whether the auto-update triggers are registered, which `topos.toml` governs where you stand, and what needs attention — updates pending, deliveries not applied yet, edits of your own — each with the command that resolves it. `-g` reports your machine-wide set instead; `--all` both. For the skill inventory use `topos list`, and `topos list <skill>` for one skill in depth. Works offline and changes nothing. A bare `topos` on a terminal shows the same thing.

```
topos status [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `-g, --global` | Report your machine-wide set, even when run inside a project |
| `--all` | Report both this folder's scope and your machine-wide set in full |


### `topos login`

Log this machine in to topos. Opens your browser for a one-click approval, where you choose (or create) the workspace to join. The first login to a workspace records its feed line (`[workspaces] "<host>/<workspace>" = "latest"`) in `~/.topos/topos.toml` — from then on, whatever that workspace delivers to you installs here and stays updated by itself; delete the line (`topos remove -g @<workspace>`) and it stays deleted — login never re-adds it. Bare `topos login` uses topos.sh; name your own server when self-hosting, a workspace to go straight to it, or paste an invitation link. To join another workspace, log in again — already logged in to that server, it takes no browser.

```
topos login [OPTIONS] [ADDRESS]
```

| Argument / flag | What it does |
|---|---|
| `[ADDRESS]` | The server, workspace, or invitation link. Omitted, uses the default server (or resumes a login already awaiting approval) |
| `--wait <SECONDS>` | Wait for the browser approval before returning. Bare `--wait` waits until the code expires; `--wait <seconds>` caps it. On a terminal, login waits by default; piped, it prints the approval URL and returns — run `topos login` again to check |


### `topos logout`

Disconnect this machine from a workspace. Installed skills, your edits, and manifests stay — they just stop updating. `topos login <workspace-address>` reconnects.

```
topos logout [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `--all` | Log out of every workspace on this machine |


### `topos init`

Create a `topos.toml` in this folder. The file lists the skills everyone working in this project should have — commit it, and teammates' agents pick up the same set by themselves. With `-g`, creates your machine's own `~/.topos/topos.toml` instead, header only — `topos login` writes a workspace's feed line on this machine's first connection to it, and `topos add -g` records the rest. If the file already exists, nothing changes.

```
topos init [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `-g, --global` | Write the machine-wide file (`~/.topos/topos.toml`) instead of this folder's |


### `topos fmt`

Tidy a `topos.toml`: group and sort its lines into the standard layout. Comments survive; meaning never changes. Formats this folder's file, or your machine-wide one with `-g`.

```
topos fmt [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `-g, --global` | Format `~/.topos/topos.toml` instead of this folder's file |


### `topos add`

Get skills and keep them updated. The source can be a skill or channel from your workspace (`code-review`, `@acme/code-review`, `@acme/channels/backend`), a whole workspace's feed (`@acme`, with `-g`), a local folder (`./tools/my-skill`), or a public GitHub repo (`owner/repo` for every skill in it, `owner/repo/name` for one). Records one line in the nearest `topos.toml` at or above this folder — or in your machine-wide file (`~/.topos/topos.toml`) with `-g` — and installs right away. With no `topos.toml` covering this folder it stops and says so: `topos init` creates one here, or add `-g`. Every answer names the file it recorded into and, on a second line, the source it recorded: a workspace or GitHub reference, or the folder on this machine. A folder whose `SKILL.md` is a link into another folder adds that original — the folder the bytes actually live in. A plain name is looked for both in the skills already sitting in your agents' folders and in the catalogs of the workspaces you are connected to — when only a workspace has it, that is what you get. A GitHub source shows what it found and waits for `--yes`, every time — a skill is instructions your agent will follow, and that listing is there to be read. `add topos` restores the built-in topos bundle: it ships with the binary, so it records no line anywhere, and its receipt names the folders that took a copy instead of a file. `--kind mcp` SHARES AN MCP SERVER with your workspace and gets it here in the same command: the source is its registry name (`io.github.acme/weather`) or an https link to its `server.json`, and the workspace reads the document, rules on it, and answers with the name it shares the server as — your agents then get it as a tool endpoint in their own MCP config rather than as a skill folder. A server your workspace already shares needs no flag at all. A server only THIS machine runs is a line you write in your own `topos.toml` (`"./weather" = { kind = "mcp" }`), so a folder is not something `--kind mcp` takes; a folder that is plainly a server bundle refuses on the plain `add` rather than landing as a skill, and `--kind skill` on one adopts it as a skill anyway. By default a skill reaches every agent on the machine; `-a <agent>` (repeatable) installs it for just those agents, and `--dest <folder>` (repeatable) installs into an exact folder — together they freeze the row to exactly those destinations, recorded in the file so updates keep landing there. For an MCP source `-a` picks whose config file gets the entry. Adding `-a`/`--dest` to something you already have EXTENDS its destinations: the copies it already has stay, and the new ones are added. To take a destination away, use `remove -a`/`--dest`.

```
topos add [OPTIONS] <SOURCE>
```

| Argument / flag | What it does |
|---|---|
| `<SOURCE>` | What to add: a workspace skill, channel, or feed; a local folder; or a GitHub repo |
| `-s, --skill <NAME>` | When a GitHub repo holds several skills, pick which one(s) (repeatable; `'*'` = all) |
| `-a, --agent <SLUG>` | Install for this agent only (a slug like `codex`; repeatable). Recorded on the row, so updates keep the copy where you asked |
| `--dest <FOLDER>` | Install into this exact folder (repeatable; combined with `-a` the union is the destination set). An MCP server takes a known config file instead |
| `--kind <KIND>` | What the source IS: `skill` (the default) or `mcp`, a server to share with your workspace. Only needed for a new server — one your workspace already shares carries its own kind |
| `--as <BUNDLE>` | Manage this folder as a copy of a skill you already have (its name, or its full reference). Nothing in the folder changes; updates land here from now on. Folders only |
| `-g, --global` | Add it machine-wide (your `~/.topos/topos.toml`) instead of to this folder's file |
| `--yes` | Confirm adding from a GitHub source, after reading what it found (everything else applies immediately, and `--yes` changes nothing there) |


### `topos remove`

Stop getting skills here — the inverse of `add`. Edits the same file `add` would: the nearest `topos.toml` at or above this folder, or your machine-wide file with `-g` (dropping a line, or switching one feed-delivered skill "off" on this machine). It never reaches across that line — a skill your machine-wide file delivers is refused here, pointing at `-g`. Removing a line also uninstalls the copies it placed, in the same command — a copy you edited stays in place, disclosed. With `-a <agent>` or `--dest <folder>` only THAT destination is removed: the row keeps the rest, and removing the last one removes the row. Prints exactly what changed and how to undo it; asks first only when removing would lose local work or rewrite a whole channel/repo line.

```
topos remove [OPTIONS] [SKILL]...
```

| Argument / flag | What it does |
|---|---|
| `[SKILL]...` | The skill(s) to remove — or `@<workspace>` with `-g` to stop adopting its feed here |
| `-a, --agent <SLUG>` | Remove only this agent's copy (a slug like `codex`; repeatable) — the skill stays for every other agent |
| `--dest <FOLDER>` | Remove only the copy in this exact folder (repeatable; combined with `-a` the union is removed). An MCP source takes a config file |
| `-g, --global` | Edit your machine-wide file (`~/.topos/topos.toml`) instead of this folder's |
| `--via <REF>` | When more than one channel/repo line carries the skill, name WHICH line's rewrite you mean (its reference, e.g. `@acme/channels/backend`) — the ambiguity refusal lists the exact `--via` invocations |
| `--yes` | Confirm a removal that loses local work (unshared edits, or a local-only skill whose delete is permanent) |


### `topos update`

Fetch and apply the latest version of what you asked for, where you are standing: this folder's `topos.toml` when one covers it, and otherwise your machine-wide set (your own `topos.toml` and the skills your workspaces give you). `-g` updates the machine-wide set even from inside a project. The background auto-update that runs at the start of each agent session always covers both, so nothing goes stale while you work in one folder. Running it by hand checks everything now, including your GitHub lines — the background sweep checks those a few times a day rather than every session. Safe to run any time. `topos update <skill>` updates one skill; `topos update <skill>@<version>` puts that version's bytes back on this machine only.

```
topos update [OPTIONS] [TARGETS]...
```

| Argument / flag | What it does |
|---|---|
| `[TARGETS]...` | The skill(s) to update; `<skill>@<version>` restores that version's bytes locally. Omitted, everything in this scope is updated |
| `-g, --global` | Update only your machine-wide skills (`~/.topos/topos.toml` and your workspace feeds), even when run inside a project |
| `--reset` | Discard your local edits to a skill and take the team version. Shows what would be lost first; `--yes` applies |
| `-a, --agent <SLUG>` | With `--reset`: drop only this agent's copy of the edits (a slug like `codex`); every other copy keeps its own |
| `--dest <FOLDER>` | With `--reset`: drop only the edits in this exact folder — the folder as `topos list` prints it, or the one the `topos.toml` line names |
| `--yes` | Confirm an action that shows a preview first (like `--reset`) |
| `--keep-mine` | Finish a merge that stopped because you and your team changed the same lines. Topos puts one copy of the skill in a folder of its own — never a folder your agents read — and prints the path; in it, files you both changed hold both versions: marked up in place with `<<<<<<<` markers where the two can be lined up, and side by side otherwise. Edit that copy and run this to use it — or run it without touching the folder to keep your wording on those lines and take the team's other changes. Either way you get an ordinary draft on top of the team's version, which `topos publish` ships like any other. Only for a merge that has actually stopped — with nothing waiting, `topos update <skill>` is the command (add `-g` when the merge is in your machine-wide set). Takes exactly one skill |
| `--quiet` | Print nothing on stdout — the mode the session-start hook uses. The hook sweep always covers both scopes (this folder's and your machine-wide set), so `-g` has no effect here. Errors still go to stderr with a non-zero exit |
| `--ttl <SECONDS>` | With `--quiet`: skip the run entirely when one already completed within this many seconds, so hooks can fire often at no cost. `0` disables the throttle. Default 300; `TOPOS_UPDATE_TTL` changes the default |
| `--force` | Re-create managed skill folders that exist but are damaged — topos normally protects a changed folder as your own edit. Deleted folders come back on an ordinary `topos update` |


### `topos list`

See what's installed where you stand, per scope: each skill with its version, where it comes from, and its state. Inside a project the rows are that folder's `topos.toml`'s; `-g` lists your machine-wide set, `--all` both. `topos list <skill>` answers one skill in depth — which file and line (or which workspace's feed) delivers it, and where its files are. `--untracked` lists skills found in your agents' folders that topos does not manage yet; `-a <agent>` shows one agent's skill folders exactly as that agent reads them; `--remote` lists what your workspaces offer (needs a login). Works offline except `--remote`.

```
topos list [OPTIONS] [NAME]
```

| Argument / flag | What it does |
|---|---|
| `[NAME]` | One skill to answer in depth. Omitted, the full inventory |
| `-g, --global` | List your machine-wide set, even when run inside a project |
| `--all` | List both this folder's scope and your machine-wide set in full |
| `--untracked` | List the skills in your agents' folders that topos does not manage yet |
| `-a, --agent <SLUG>` | Show one agent's skill folders as that agent reads them (a slug like `cursor`) |
| `--remote` | Also list what your workspace(s) offer, with each skill's state on this machine. Needs a login |
| `--footprint` | Also list the files topos owns outside skill folders |
| `--channel <NAME>` | Only skills in this channel (repeatable) |
| `--skill <NAME>` | Only this skill (repeatable) |
| `--limit <N>` | Print at most this many rows per section (`0` = all). Default: unlimited on a terminal, 50 under `--json` |
| `--offset <N>` | Skip this many rows first — the next page's cursor |


### `topos diff`

Show what changed in a skill. Bare: your local edits against the version you last applied. With a version id: that version against the team's current. `<a>..<b>` compares two versions. Reads the copy where you are standing; `-g` reads your machine-wide copy even from inside a project. When the skill sits in more than one folder, `--dest <folder>` (or `-a <agent>`) reads the edits in that one — still against the version you last applied, so two such runs compare like for like.

```
topos diff [OPTIONS] <SKILL> [REF]
```

| Argument / flag | What it does |
|---|---|
| `<SKILL>` | The skill name |
| `-g, --global` | Read your machine-wide copy, even when run inside a project |
| `[REF]` | What to compare: a version id, or `<a>..<b>`. Omitted: your edits vs the version you last applied |
| `-a, --agent <SLUG>` | Read this agent's copy of the skill (a slug like `codex`) |
| `--dest <FOLDER>` | Read the copy in this exact folder — the folder as `topos list` prints it, or the one the `topos.toml` line names |
| `--max-bytes <BYTES>` | Cap the diff at this many bytes, cut at file boundaries (`0` = no cap). Default: unlimited on a terminal, 64 KiB under `--json` |


### `topos verify`

Check that an MCP server is actually working, right now, from this machine. Topos dials the address the bundle names (or runs its package) and asks the server for its tools, then prints what happened. Nothing is stored and nothing is installed — it is a live check, not a delivery state. A server that asks for a sign-in is healthy: topos holds no credential, and your agent app signs in on first use — unless the server takes only clients or tokens registered in advance, which the line says instead, because no agent can complete that sign-in by itself. Exit codes: `0` responding, `3` sign-in required, `4` not reachable, `5` reachable but not answering as an MCP server.

```
topos verify [OPTIONS] <NAME>
```

| Argument / flag | What it does |
|---|---|
| `<NAME>` | The MCP server bundle's name |
| `-g, --global` | Check the server your machine-wide scope holds, even when run inside a project |


### `topos log`

Show a skill's history — every version with its message and id.

```
topos log [OPTIONS] <SKILL>
```

| Argument / flag | What it does |
|---|---|
| `<SKILL>` | The skill name |
| `--limit <N>` | Print at most this many entries (`0` = all). Default: unlimited on a terminal, 20 under `--json` |
| `--offset <N>` | Skip this many entries first — the next page's cursor |

## Team commands

These reach your workspace, so each one previews first or confirms via its flag.

### `topos publish`

Share a skill with your team. A bare run is a preview — it shows where the skill would land and whether review is required, and changes nothing; add `--yes` to apply. Publishing again ships a new version; on a skill that requires review, a publish opens a proposal instead. Needs a login. When you have edited the same skill in more than one folder, a bare publish stops and asks which one you mean; `--dest <folder>` (or `-a <agent>`) answers it. The copy you do not pick keeps its edits and becomes an ordinary draft. A skill you have both in this project and machine-wide publishes from whichever copy holds the edits, and says which folder that was; `-g` names your machine-wide copy. A copy that already matches the published version is not an error — it says so and stops.

```
topos publish [OPTIONS] <TARGET>
```

| Argument / flag | What it does |
|---|---|
| `<TARGET>` | The skill to publish: a name, a folder, or `<name>@<version>` to pin the exact bytes |
| `-g, --global` | Publish your machine-wide copy, even when run inside a project |
| `-a, --agent <SLUG>` | Publish this agent's copy of the skill (a slug like `codex`) |
| `--dest <FOLDER>` | Publish the copy in this exact folder — the folder as `topos list` prints it, or the one the `topos.toml` line names |
| `--to <CHANNEL>` | Place the skill in this channel. It must already exist — channels are created in the browser. A brand-new skill with no `--to` lands in `everyone` |
| `--propose` | Ask for review instead of shipping directly — opens a proposal |
| `-m, --message <MSG>` | A short message saying what changed and why — it becomes the version's history line |
| `--yes` | Apply the previewed publish |


### `topos review`

See and settle proposals. Bare: your review inbox. With a proposal (`<skill>@<version>`): its diff. Add a verdict to settle it — `--approve` ships it, `--reject -m <reason>` declines it, `--withdraw` retracts your own.

```
topos review [OPTIONS] [TARGET]
```

| Argument / flag | What it does |
|---|---|
| `[TARGET]` | The proposal, as `<skill>@<version>`. Omitted: the inbox |
| `--approve` | Ship the proposal — it becomes the version everyone receives |
| `--reject` | Decline the proposal. Say why with `-m` — the author sees the reason |
| `--withdraw` | Retract your own open proposal |
| `-m, --message <MSG>` | The reason or note (required with `--reject`) |
| `--max-bytes <BYTES>` | Cap the shown diff at this many bytes, cut at file boundaries (`0` = no cap). Default: unlimited on a terminal, 64 KiB under `--json` |
| `--yes` | Not needed here — the verdict flag is the confirmation |


### `topos revert`

Roll the team back to an earlier version of a skill. `--to` names the good version to return to; everyone picks it up at their next update. Nothing is deleted, so a revert can itself be reverted. To roll back only this machine, use `topos update <skill>@<version>` instead.

```
topos revert [OPTIONS] <SKILL>
```

| Argument / flag | What it does |
|---|---|
| `<SKILL>` | The skill to roll back |
| `--to <TO>` | The version to return to — the good one, not the bad one. A full id, or a unique prefix of at least 8 characters |
| `-g, --global` | Act on your machine-wide copy of the skill, even when run inside a project |
| `--yes` | Apply the previewed revert (also confirms when that version is already live) |


### `topos protect`

Require review before a skill changes — or curation before a channel's contents change. `topos protect <skill>` turns it on; `topos protect <skill> open` turns it off (owners only).

```
topos protect [OPTIONS] <TARGET> [LEVEL]
```

| Argument / flag | What it does |
|---|---|
| `<TARGET>` | The skill or channel to protect |
| `[LEVEL]` | The level: `reviewed` (skill), `curated` (channel), or `open` to loosen. Omitted, protection is turned on |
| `--yes` | Apply the previewed change |


### `topos invite`

Invite teammates by email. Each address gets a single-use link that adds them to the workspace — they can accept in the browser, hand the mail to their agent, or run `topos login <invite-url>`. Owners only; the server must have mail configured. A bare `invite` just prints the workspace address.

```
topos invite [OPTIONS] [EMAIL]...
```

| Argument / flag | What it does |
|---|---|
| `[EMAIL]...` | The addresses to invite. Each link stays valid for 7 days; re-inviting sends a fresh one |
| `--skill <NAME>` | Set the invitee up with this skill from the start (at most one of `--skill`/`--channel`) |
| `--channel <NAME>` | Set the invitee up with this channel from the start |
| `--yes` | Send the previewed invitations |

## Maintenance

The binary and the installation itself.

### `topos self-update`

Update the `topos` binary itself to the latest release. The download's checksum and signature are always verified, and the swap is atomic. Your skills are untouched — they update with `topos update`.

```
topos self-update [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `--check` | Only report whether a newer release exists — download nothing |
| `--version <TAG>` | Install a specific release (e.g. v0.2.0) instead of the latest — downgrades included |


### `topos auth`

Check your sign-in state: `topos auth status`.

```
topos auth <COMMAND>
```


#### `topos auth status`

Show each workspace login and whether it still works, plus the state of the auto-update hooks. Changes nothing.


### `topos uninstall`

Remove topos from this machine. Deletes the auto-update hooks and topos's own state (`~/.topos/`, logins included); installed skill files are never touched. A bare run shows what would go; `--yes` applies. The binary itself is left in place — its path is printed so you can delete it with whatever installed it.

```
topos uninstall [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `--yes` | Apply the previewed uninstall |


## Aliases

- `topos pull` — an alias of `topos update`, kept for hooks installed by older versions.
- `topos upgrade` — deliberately refused with a pointer to both meanings: `topos update` refreshes your skills, `topos self-update` replaces the `topos` binary itself.
