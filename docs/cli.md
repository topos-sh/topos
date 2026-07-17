# `topos` command reference

> GENERATED from the `clap` command tree by `cargo xtask gen-cli-ref` — do not hand-edit. Change the CLI, re-run the command, and commit the result; the `--check` variant is the drift gate.

`topos` is the client an agent drives non-interactively. Every mutating verb is TWO-PHASE: a bare invocation DESCRIBES what would change (nothing is written), and `--yes` applies it in one shot (`revert` is the exception — `--yes` there also acknowledges a no-op). `--json` works on every verb and prints exactly one envelope on stdout (never a prompt). The exit status is one of three classes: `0` on success, `1` on a domain refusal or a failed operation (the envelope's `ok` + `error.outcome` distinguish a refusal from a transport fault), and `2` on a usage error (an unknown flag or a missing argument). The session-start currency hook runs `topos update --quiet`, which stays silent except a freshness one-liner and exits `0` on a network blip so a session never fails to start.

## Global options

These work before or after any verb.

| Flag | Value | Description |
|---|---|---|
| `--json` |  | Emit one JSON document on stdout (the agent surface) instead of human text. Never prompts |
| `--workspace` | `<WORKSPACE>` | Act in a specific workspace when this install follows skills from more than one on the same plane. Accepts the workspace's address NAME (what you joined by) or its opaque id. Selects the workspace for the ambient team verbs (a genesis `publish`, `invite`) and disambiguates a skill name shared across workspaces. Optional — with a single workspace it is inferred |

## Self-scoped verbs

### `topos follow`

```
topos follow [OPTIONS] [TARGETS]...
```

Follow a workspace, channel, or skill — enroll if needed, then subscribe two-phase (a bare invocation DESCRIBES what would land; `--yes` applies). Targets: a workspace address (`https://topos.sh/acme`, or a bare workspace name), a bare SERVER address with no workspace slug (`https://topos.example.com`, or the schemeless `topos.example.com`) — "the workspace that origin addresses", the single-tenant install form, a qualified path (`acme/channels/eng`, `acme/skills/deploy`), or a bare channel/skill name. A first follow enrolls this device: open the printed approval URL in a browser, check the code matches, and approve — the device then holds ONE credential for everything your seats reach. `follow <skill>` on a KNOWN followed skill places its disclosed first-receive offer (or resumes a skill `unfollow` paused). While an enrollment is pending, re-invoking `follow` RESUMES it

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[TARGETS]...` |  |  | The follow targets (addresses, qualified paths, or names). Omitted, it resumes a pending enrollment |
| `--channel` | `<NAME>` |  | Follow a channel by name (repeatable; kind-forced) |
| `--skill` | `<NAME>` |  | Follow a specific skill by name (repeatable; kind-forced) |
| `--yes` |  |  | Apply the described subscription (the one-shot consent). Bare = describe only |
| `--prefix-dirname` |  |  | Install a dirname-colliding skill under `<workspace>.<name>` instead of declining it |
| `--manual` |  |  | Adopt followed skills in confirm-each mode (a one-tap accept per new version) instead of auto |
| `--wait` | `<SECONDS>` |  | Block until the browser approval settles, finishing enrollment in ONE command. Bare `--wait` waits until the code expires; `--wait <seconds>` caps the wait. Put `--wait` AFTER any positional |


### `topos unfollow`

```
topos unfollow [OPTIONS] [TARGETS]...
```

Stop following a skill or channel — two-phase (bare describes what stops; `--yes` applies). Delivery ends on EVERY device of yours; local copies are KEPT as frozen copies (nothing is deleted) and `follow` re-attaches. A workspace cannot be left here (that is a web action), and the structural `everyone` cannot be left at all

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[TARGETS]...` |  |  | The channel/skill name(s) (or qualified paths) to stop following |
| `--channel` | `<NAME>` |  | Unfollow a channel by name (repeatable; kind-forced) |
| `--skill` | `<NAME>` |  | Unfollow a specific skill by name (repeatable; kind-forced) |
| `--yes` |  |  | Apply the described detach (the one-shot consent). Bare = describe only |


### `topos update`

```
topos update [OPTIONS] [TARGETS]...
```

Check for and apply updates to followed skills — the harness currency entry point. Bare = the sweep over every followed skill (the installed currency trigger runs `update --quiet`). `<skill>` accepts a pending update for one skill (or resumes a held one); `<skill>@<hash>` goes back to that version

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[TARGETS]...` |  |  | Optional target(s): `<name>` accepts a pending update / resumes a hold / resolves a divergence; `<name>@<hash>` goes back to that version's bytes. Omitted = sweep every followed skill |
| `--channel` | `<NAME>` |  | Update only this channel's skills (repeatable). Lands with the full resolution grammar |
| `--skill` | `<NAME>` |  | Update only this skill (repeatable). Lands with the full resolution grammar |
| `--reset` |  |  | Reset a followed skill to `current`, dropping local edits. Lands with the loss-led describe |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |
| `--onto-current` |  |  | Resolve a diverged draft the OTHER way: commit YOUR bytes straight onto `current`, DROPPING the pending three-way merge (the changes it would have merged are disclosed first). Requires exactly one `<skill>` target. Use when you want your version to win outright |
| `--quiet` |  |  | Emit nothing on stdout (the session-start hook's stdout is injected into the session). Errors still go to stderr with a non-zero exit. Overrides `--json` |
| `--ttl` | `<SECONDS>` |  | The quiet sweep's self-throttle window in seconds (`--quiet` only): a bare quiet sweep within this window of the last completed sweep is a silent no-op, so hooks may fire on every session event cheaply. `0` disables the throttle for this run. Default 300; `TOPOS_UPDATE_TTL` overrides the default. An explicit non-quiet `topos update` always runs the full sweep |


### `topos add`

```
topos add [OPTIONS] <SOURCE>
```

Adopt a skill into topos. The source is polymorphic: • a skill NAME (`deploy`, `deploy@claude-code`) — resolved against the untracked skills `topos list` discovers (`@<harness>` disambiguates across harnesses); • a PATH (`./skills/deploy`, `~/x`, `/abs`) — adopt that directory in place; • a REMOTE source (`owner/repo`, `owner/repo#<ref>`, an https://github.com URL) — fetch it. Local adopts are offline. A remote import fetches a public repo (no account); the source's trustworthiness is yours to verify

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `<SOURCE>` |  |  | The skill to adopt — a name, a path, or a remote `owner/repo`/github.com URL |
| `-s, --skill` | `<NAME>` |  | Pick a skill from a repo that holds several (repeatable; `'*'` = all). A lone skill needs none |
| `-a, --agent` | `<SLUG>` |  | The agent (harness) to land a remote import into (a registry slug, e.g. `cursor`; repeatable; `'*'` = all). Default: the active harness. Ignored for a local path / name adopt |
| `-g, --global` |  |  | Land a remote import in the harness's global/user skills dir instead of the project (cwd) dir |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |


### `topos remove`

```
topos remove [OPTIONS] [SKILL]...
```

Remove skills from this machine (or from specific agents). A followed skill becomes a per-device exclusion (your other devices keep receiving it); an untracked local copy is cleaned

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[SKILL]...` |  |  | The skill name(s) to remove |
| `-a, --agent` | `<SLUG>` |  | Remove only from these agents (harness slugs; repeatable; `'*'` = all) |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |


### `topos list`

```
topos list [OPTIONS] [NAME]...
```

Inventory the skills on this machine. By default also discovers **untracked** skills sitting in any known harness's skill dir (across the baked registry) that topos could `add`

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[NAME]...` |  |  | Narrow to one or more skills by name (errors if a name is ambiguous) |
| `--remote` |  |  | Also list skills available in the workspace(s) you follow (the remote catalog), annotated with your follow-state. Requires enrollment; `--workspace` (name or id) narrows |
| `--tracked` |  |  | Show only locally-tracked skills — skip discovery of untracked harness-dir skills |
| `--footprint` |  |  | Also report the paths topos owns outside skill directories |
| `--channel` | `<NAME>` |  | Narrow to one channel's skills (repeatable). Lands with the full resolution grammar |
| `--skill` | `<NAME>` |  | Narrow to a specific skill (repeatable). Lands with the full resolution grammar |


### `topos diff`

```
topos diff <SKILL> [REF]
```

Show a skill's change. Bare = draft ↔ current; `<hash>` / `@<hash>` reviews that version against current (`current..<hash>` — a proposal IS a version); `<a>..<b>` = version ↔ version. `--json` emits the target digest + `source: local\|plane`

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `<SKILL>` |  |  | The skill name |
| `[REF]` |  |  | The optional ref: `<hash>` / `@<hash>` / `current..<hash>` / `<a>..<b>`. Omitted = draft ↔ current |


### `topos log`

```
topos log <SKILL>
```

Show a skill's local action log + embedded-git history

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `<SKILL>` |  |  | The skill name |

## Team-scoped verbs

### `topos publish`

```
topos publish [OPTIONS] <TARGET>
```

Ship a draft to the team, ADDING the skill to topos first if it isn't tracked yet. `publish` moves `current` to your draft (or genesis-creates a never-published skill); `--propose` opens a PR without moving `current`. Pin the bytes with an optional `@<digest>` suffix. Needs enrollment — un-enrolled, it refuses with "run `topos follow <workspace-address>` first". Roster-gated

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `<TARGET>` |  |  | The skill to publish: a tracked NAME, an untracked `<skill>` / `<skill>@<harness>` to adopt from discovery, or a `<dir>` to adopt in place — optionally pinned as `<source>@<digest>` |
| `--to` | `<CHANNEL>` |  | Place the skill's reference into this channel (created on first use; a curated channel needs reviewer+). A brand-new skill with no `--to` lands in `everyone` |
| `--propose` |  |  | Open a proposal (a PR) instead of moving `current` |
| `-m, --message` | `<MSG>` |  | The commit message for this version (threaded into the candidate commit id) |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |


### `topos review`

```
topos review [OPTIONS] [TARGET]
```

Resolve a proposal (the `gh pr review` model). `--approve` moves `current` to the candidate (a compare-and-set on its base; a stale base re-dos); `--reject` declines a proposal (reviewer, `-m <reason>` required); `--withdraw` retracts your own open proposal. A bare `review` (no target / no verdict) is the review inbox/describe (lands later). Roster-gated

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[TARGET]` |  |  | The proposal to resolve, as `<skill>@<hash>`. Omitted = the review inbox (lands later) |
| `--approve` |  |  | Approve the proposal — move `current` to the candidate |
| `--reject` |  |  | Reject the proposal (needs `-m <reason>`) |
| `--withdraw` |  |  | Withdraw your own open proposal |
| `-m, --message` | `<MSG>` |  | The reject reason / withdrawal note (required with `--reject`) |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |


### `topos revert`

```
topos revert [OPTIONS] <SKILL>
```

Undo a release for the TEAM: move `current` to the older version named by `--to` — a **forward** pointer-move (nothing deleted; invertible). `--to <hash>` is the sole source of the GOOD version you go back TO (not the bad one). Team-only — the local go-back is `update <skill>@<hash>`. Roster-gated

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `<SKILL>` |  |  | The skill to revert |
| `--to` | `<TO>` |  | The GOOD version id (64-char hex, or a unique ≥8-char prefix) to restore — the destination, NOT the bad version |
| `--yes` |  |  | Apply the described revert; also acknowledges a no-op (good's bytes already are `current`). Bare = describe only |


### `topos channel`

```
topos channel [OPTIONS] [ARGS]...
```

Group skills into channels. `channel add <channel> <skill>...` places a skill's reference into a channel (created on first placement); `channel remove <channel> <skill>...` removes it. Curated channels need reviewer+

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[ARGS]...` |  |  | The channel subcommand and its args: `add <channel> <skill>...` or `remove <channel> <skill>...` |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |


### `topos protect`

```
topos protect [OPTIONS] <TARGET> [LEVEL]
```

Set a skill's (or channel's) protection level. Bare tightens to `reviewed` (skill) / `curated` (channel) — reviewer+; `open` loosens it back — owner

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `<TARGET>` |  |  | The skill or channel to protect |
| `[LEVEL]` |  |  | The level (`reviewed` / `curated` / `open`); omitted tightens to the reviewed/curated default |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |


### `topos invite`

```
topos invite [OPTIONS] [EMAIL]...
```

Seat emails as invited members of the workspace (a roster write). Every CLI invitee starts as a member; joining is `follow <address>` plus proof of the invited email. Requires prior enrollment. A bare `invite` (no emails) reads the workspace address + policy (lands later)

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[EMAIL]...` |  |  | The emails to invite (folded to canonical form; seeded onto the roster as `invited`) |
| `--channel` | `<NAME>` |  | Pre-place each invitee into this channel (repeatable) |
| `--yes` |  |  | Apply without the describe step. Parses today; the two-phase describe lands later |

## Maintenance

### `topos self-update`

```
topos self-update [OPTIONS]
```

Update the `topos` binary itself to the latest release, verifying the download's sha256 against the release SHA256SUMS (never skippable) and replacing the running binary atomically. A MAINTENANCE command — it touches no skills, no plane, no account. (Skills are updated by `topos update`.)

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `--check` |  |  | Only check whether a newer release exists; report and exit without downloading or replacing |
| `--version` | `<TAG>` |  | Install a specific release tag (e.g. v0.2.0) instead of the latest — allows a pinned downgrade |


### `topos auth`

```
topos auth <COMMAND>
```

Manage this install's sign-in: `auth login [<server>]`, `auth logout`, `auth status`


#### `topos auth login`

```
topos auth login [OPTIONS] [SERVER_URL]
```

Re-enroll this machine (the same browser-approval device flow `follow` runs, minus a follow target): approve in the browser and this device's ONE credential is re-minted — it covers every workspace your seats reach. On an already-enrolled install the new credential REPLACES the stored one. An optional `<server>` names the server (default https://topos.sh; TOPOS_PLANE_URL overrides). A never-enrolled install joins with `topos follow <workspace-address>` instead

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `[SERVER_URL]` |  |  | The server URL to sign in to (optional; the enrolled plane, else the hosted default) |
| `--wait` | `<SECONDS>` |  | Block until the browser approval settles in ONE command. Bare `--wait` waits until the code expires; `--wait <seconds>` caps the wait |


#### `topos auth logout`

```
topos auth logout [OPTIONS]
```

Sign out of this install: revoke this device in each workspace (best-effort), delete the stored credential — skills, follows, and drafts stay. Two-phase (bare describes; `--yes` applies)

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `--yes` |  |  | Apply the described sign-out |


#### `topos auth status`

```
topos auth status
```

Show who you are, per-workspace access health, hook health, and reporting posture. Side-effect-free


### `topos uninstall`

```
topos uninstall [OPTIONS]
```

Remove topos from this machine — two-phase (bare describes what goes; `--yes` applies). Scrubs the session-start currency hook from the harness config and deletes the `~/.topos/` sidecar tree (the signed-in credential lives there and goes with it). SKILL FILES IN AGENT DIRS ARE LEFT UNTOUCHED — uninstall never deletes a skill byte. The `topos` binary is NOT self-deleted; remove it with the installer you used (or `rm` its printed path). Needs no sign-in

| Argument / flag | Value | Default | Description |
|---|---|---|---|
| `--yes` |  |  | Apply the described uninstall (the one-shot consent). Bare = describe only |


## Renamed verbs

- `topos pull` is a hidden alias of `topos update` (armed session-start hooks in the field still invoke `pull`); the `--json` envelope always reads `update`.
- `topos upgrade` is intentionally ambiguous and refuses with a disambiguation: `topos update` refreshes followed skills, while `topos self-update` replaces the `topos` binary.
