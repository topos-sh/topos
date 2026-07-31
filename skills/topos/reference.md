# `topos` command reference

> GENERATED from the `clap` command tree by `cargo xtask gen-cli-ref` — do not hand-edit. Change the CLI, re-run the command, and commit the result; the `--check` variant is the drift gate.

Every command prints human-readable text on a terminal, or exactly one JSON object with `--json` (for agents and scripts — it never prompts).

One rule covers all of them: **a command that reaches other people, discards local work, or trusts a new source shows a preview first.** Run it bare to see exactly what would happen, then add `--yes` to apply. Everything else applies immediately and prints what changed, along with the command that undoes it.

Exit codes: `0` — success. `1` — the operation was refused or failed (with `--json`, the `error` object says which and how to fix it). `2` — the command line itself was invalid.

## The `--json` envelope

Every `--json` run prints one object on stdout: `schema_version` (1), `command`, `ok`, a per-command `data` payload, `warnings`, `next_actions`, and — on `ok: false` — an `error` (`code`, `outcome`, `retryable`, plus its own `next_actions`). Each `next_actions` entry is a ready-to-run step: `argv` is a complete command; `needs` lists any `<placeholder>` tokens you must substitute first; `mutates`, `needs_network`, and `risk_note` are safety metadata (absent means unknown). Treat `code` as an open vocabulary — run an unfamiliar action by its `argv` rather than rejecting it. The full JSON-Schemas live under `contracts/schemas/` in the repository, with golden examples under `contracts/fixtures/json/`; the agents guide (topos.sh/docs/agents) shows worked examples.

## Global options

These work before or after any command.

| Flag | What it does |
|---|---|
| `--json` | Print one JSON object instead of human text — for agents and scripts. Never prompts |
| `--workspace <WORKSPACE>` | Pick which workspace to act in when this machine is logged into more than one. Takes the workspace's name or id. With a single login it is inferred |

## Everyday commands

These act on this machine only.

### `topos status`

See what topos manages on this machine: each skill with its version and where it comes from — per scope (this folder's `topos.toml`, and your own machine-wide set) — plus your workspace logins and whether auto-update is armed. `topos status <skill>` answers one skill in depth: which file and line (or which workspace's feed) delivers it, and where its files are. Works offline and changes nothing. A bare `topos` on a terminal shows the same thing

```
topos status [BUNDLE]
```

| Argument / flag | What it does |
|---|---|
| `[BUNDLE]` | One skill to answer in depth. Omitted, the full table |


### `topos login`

Log this machine in to topos. Opens your browser for a one-click approval, where you choose (or create) the workspace to join; from then on, that workspace's skills arrive and stay updated by themselves. Bare `topos login` uses topos.sh; name your own server when self-hosting, a workspace to go straight to it, or paste an invitation link. To join another workspace, log in again — already logged in to that server, it takes no browser

```
topos login [OPTIONS] [ADDRESS]
```

| Argument / flag | What it does |
|---|---|
| `[ADDRESS]` | The server, workspace, or invitation link. Omitted, uses the default server (or resumes a login already awaiting approval) |
| `--wait <SECONDS>` | Wait for the browser approval before returning. Bare `--wait` waits until the code expires; `--wait <seconds>` caps it. On a terminal, login waits by default; piped, it prints the approval URL and returns — run `topos login` again to check |


### `topos logout`

Disconnect this machine from a workspace. Installed skills, your edits, and manifests stay — they just stop updating. `topos login <address>` reconnects

```
topos logout [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `--all` | Log out of every workspace on this machine |


### `topos init`

Create a `topos.toml` in this folder. The file lists the skills everyone working in this project should have — commit it, and teammates' agents pick up the same set by themselves. With `-g`, writes your machine's own `~/.topos/topos.toml` instead, spelling out what each connected workspace already delivers so you can edit it line by line. If the file already exists, nothing changes

```
topos init [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `-g, --global` | Write the machine-wide file (`~/.topos/topos.toml`) instead of this folder's |


### `topos fmt`

Tidy a `topos.toml`: group and sort its lines into the standard layout. Comments survive; meaning never changes. Formats this folder's file, or your machine-wide one with `-g`

```
topos fmt [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `-g, --global` | Format `~/.topos/topos.toml` instead of this folder's file |


### `topos add`

Get skills and keep them updated. The source can be a skill or channel from your workspace (`code-review`, `@acme/code-review`, `@acme/channels/backend`), a whole workspace's feed (`@acme`, with `-g`), a local folder (`./tools/my-skill`), or a public GitHub repo (`owner/repo` for every skill in it, `owner/repo/name` for one). Records one line in this folder's `topos.toml` — or in your machine's own file with `-g` — and installs right away. A GitHub source you have never used before shows what it found and waits for `--yes`. `add topos` restores the built-in topos skill

```
topos add [OPTIONS] <SOURCE>
```

| Argument / flag | What it does |
|---|---|
| `<SOURCE>` | What to add: a workspace skill, channel, or feed; a local folder; or a GitHub repo |
| `-s, --skill <NAME>` | When a GitHub repo holds several skills, pick which one(s) (repeatable; `'*'` = all) |
| `-a, --agent <SLUG>` | Which agent to install a GitHub import for (a slug like `cursor`; repeatable; `'*'` = all). Default: the agent detected here |
| `-g, --global` | Add it machine-wide (your `~/.topos/topos.toml`) instead of to this folder's file |
| `--yes` | Confirm adding from a GitHub source this machine has never used before (everything else applies immediately, and `--yes` changes nothing there) |


### `topos remove`

Stop getting skills here — the inverse of `add`. Edits the same `topos.toml` (or your machine-wide file with `-g`: dropping a line, or switching one feed-delivered skill "off" on this machine) and prints exactly what changed and how to undo it. Asks first only when removing would lose local work or rewrite a whole channel/repo line

```
topos remove [OPTIONS] [SKILL]...
```

| Argument / flag | What it does |
|---|---|
| `[SKILL]...` | The skill(s) to remove — or `@<workspace>` with `-g` to stop adopting its feed here |
| `-g, --global` | Edit your machine-wide file (`~/.topos/topos.toml`) instead of this folder's |
| `--via <REF>` | When more than one channel/repo line carries the skill, name WHICH line's rewrite you mean (its reference, e.g. `@acme/channels/backend`) — the ambiguity refusal lists the exact `--via` invocations |
| `--yes` | Confirm a removal that loses local work (unshared edits, or a local-only skill whose delete is permanent) |


### `topos update`

Fetch and apply the latest version of everything this folder's `topos.toml` asks for and everything your workspaces give you. Runs by itself at the start of each agent session; safe to run by hand any time. `topos update <skill>` updates one skill; `topos update <skill>@<version>` puts that version's bytes back on this machine only

```
topos update [OPTIONS] [TARGETS]...
```

| Argument / flag | What it does |
|---|---|
| `[TARGETS]...` | The skill(s) to update; `<skill>@<version>` restores that version's bytes locally. Omitted, everything is updated |
| `--reset` | Discard your local edits to a skill and take the team version. Shows what would be lost first; `--yes` applies |
| `--yes` | Confirm an action that shows a preview first (like `--reset`) |
| `--onto-current` | Resolve a conflicted skill by keeping your bytes exactly as they are, skipping the merge with the team's changes (what the merge would have brought is shown first). Takes exactly one skill |
| `--quiet` | Print nothing on stdout — the mode the session-start hook uses. Errors still go to stderr with a non-zero exit |
| `--ttl <SECONDS>` | With `--quiet`: skip the run entirely when one already completed within this many seconds, so hooks can fire often at no cost. `0` disables the throttle. Default 300; `TOPOS_UPDATE_TTL` changes the default |
| `--rebuild` | Rebuild every managed skill folder from topos's own store: your unshared edits are saved first, then each folder is re-created fresh. Fixes a folder someone deleted or broke by hand |


### `topos list`

List the skills on this machine — the ones topos manages, plus untracked skills found in your agents' skill folders that you could `add`

```
topos list [OPTIONS] [NAME]...
```

| Argument / flag | What it does |
|---|---|
| `[NAME]...` | Show only these skills |
| `--remote` | Also list what your workspace(s) offer, with each skill's state on this machine. Needs a login |
| `--tracked` | Only skills topos manages — skip discovery of untracked ones |
| `--footprint` | Also list the files topos owns outside skill folders |
| `--channel <NAME>` | Only skills in this channel (repeatable) |
| `--skill <NAME>` | Only this skill (repeatable) |
| `--limit <N>` | Print at most this many rows per section (`0` = all). Default: unlimited on a terminal, 50 under `--json` |
| `--offset <N>` | Skip this many rows first — the next page's cursor |


### `topos diff`

Show what changed in a skill. Bare: your local edits against the team version. With a version id: that version against the team's. `<a>..<b>` compares two versions

```
topos diff [OPTIONS] <SKILL> [REF]
```

| Argument / flag | What it does |
|---|---|
| `<SKILL>` | The skill name |
| `[REF]` | What to compare: a version id, or `<a>..<b>`. Omitted: your edits vs the team version |
| `--max-bytes <BYTES>` | Cap the diff at this many bytes, cut at file boundaries (`0` = no cap). Default: unlimited on a terminal, 64 KiB under `--json` |


### `topos log`

Show a skill's history — every version with its message and id

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

Share a skill with your team. A bare run is a preview — it shows where the skill would land and who would receive it, and changes nothing; add `--yes` to apply. Publishing again ships a new version; on a skill that requires review, a publish opens a proposal instead. Needs a login

```
topos publish [OPTIONS] <TARGET>
```

| Argument / flag | What it does |
|---|---|
| `<TARGET>` | The skill to publish: a name, a folder, or `<name>@<version>` to pin the exact bytes |
| `--to <CHANNEL>` | Place the skill in this channel. It must already exist — channels are created in the browser. A brand-new skill with no `--to` lands in `everyone` |
| `--propose` | Ask for review instead of shipping directly — opens a proposal |
| `-m, --message <MSG>` | A short message saying what changed and why — it becomes the version's history line |
| `--yes` | Apply the previewed publish |


### `topos review`

See and settle proposals. Bare: your review inbox. With a proposal (`<skill>@<version>`): its diff. Add a verdict to settle it — `--approve` ships it, `--reject -m <reason>` declines it, `--withdraw` retracts your own

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

Roll the team back to an earlier version of a skill. `--to` names the good version to return to; everyone picks it up at their next update. Nothing is deleted, so a revert can itself be reverted. To roll back only this machine, use `topos update <skill>@<version>` instead

```
topos revert [OPTIONS] <SKILL>
```

| Argument / flag | What it does |
|---|---|
| `<SKILL>` | The skill to roll back |
| `--to <TO>` | The version to return to — the good one, not the bad one. A full id, or a unique prefix of at least 8 characters |
| `--yes` | Apply the previewed revert (also confirms when that version is already live) |


### `topos protect`

Require review before a skill changes — or curation before a channel's contents change. `topos protect <skill>` turns it on; `topos protect <skill> open` turns it off (owners only)

```
topos protect [OPTIONS] <TARGET> [LEVEL]
```

| Argument / flag | What it does |
|---|---|
| `<TARGET>` | The skill or channel to protect |
| `[LEVEL]` | The level: `reviewed` (skill), `curated` (channel), or `open` to loosen. Omitted, protection is turned on |
| `--yes` | Apply the previewed change |


### `topos invite`

Invite teammates by email. Each address gets a single-use link that adds them to the workspace — they can accept in the browser, hand the mail to their agent, or run `topos login <invite-url>`. Owners only; the server must have mail configured. A bare `invite` just prints the workspace address

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

Update the `topos` binary itself to the latest release. The download's checksum and signature are always verified, and the swap is atomic. Your skills are untouched — they update with `topos update`

```
topos self-update [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `--check` | Only report whether a newer release exists — download nothing |
| `--version <TAG>` | Install a specific release (e.g. v0.2.0) instead of the latest — downgrades included |


### `topos auth`

Check your sign-in state: `topos auth status`

```
topos auth <COMMAND>
```


#### `topos auth status`

Show each workspace login and whether it still works, plus the state of the auto-update hooks. Changes nothing


### `topos uninstall`

Remove topos from this machine. Deletes the auto-update hooks and topos's own state (`~/.topos/`, logins included); installed skill files are never touched. A bare run shows what would go; `--yes` applies. The binary itself is left in place — its path is printed so you can delete it with whatever installed it

```
topos uninstall [OPTIONS]
```

| Argument / flag | What it does |
|---|---|
| `--yes` | Apply the previewed uninstall |


## Aliases

- `topos pull` — an alias of `topos update`, kept for hooks installed by older versions.
- `topos upgrade` — deliberately refused with a pointer to both meanings: `topos update` refreshes your skills, `topos self-update` replaces the `topos` binary itself.
