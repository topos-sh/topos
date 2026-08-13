# `topos-harness` — the placement port + the trigger port

TWO client-side ports, one responsibility each, plus the `ConfigStore` + `CommandRunner` seams and
the harness impls:

- **`HarnessAdapter`** answers **where** — `id` / `discover` / `placement_for`. It writes nothing.
  (A target DIR is the registry ROW's: the CLI's placement engine resolves every detected
  harness's skills root from the table, so `placement_for` decides a dir only where no row can
  resolve — a client with no machine roots at all.)
- **`triggers::TriggerAdapter`** answers **when the update check fires** — `slug` / `install` /
  `remove` / `present` / `artifacts`, plus the two honesty knobs (`offline_probe_refusal`,
  `scrub_needs_live_harness`). It edits its own harness config surface and nothing else.

Both are **content-blind**: neither receives a skill's bytes, hashes a bundle, or writes a skill
dir. A harness may be served by both ports, by either alone, or by different machinery on each side;
a caller composes them and never learns which. v0 places exact bytes with no dialect translation, so
adding a harness is a new impl, not a refactor.

**ALL platform / harness-version dependencies live here** — the rest of the workspace stays
platform-agnostic.

## The trigger contract

Every registered trigger runs the ONE sweep `topos update --quiet` (self-throttled client-side, so
firing on every session-shaped event is cheap). `--hook <harness>` names the calling trigger and
selects the sweep's stdout dialect: UNMARKED is the schema-conservative default (`hookEventName` +
`additionalContext` only, nothing when there is nothing to say — what a strict hook-output
validator accepts); `claude-code` is the one spec declaring a `hook_dialect` today, opting into
`reloadSkills` so pulled skills go live same-session. `present` is the hook-health probe
`list`/`auth status` read, and `artifacts` is what `uninstall`'s describe discloses — every artifact
a scrub REACHES, as a `TriggerArtifact`: a topos-owned `Path` outside skill dirs (never a delete
target, and named only where it is confirmed ours right now — this is also the row
`list --footprint` prints), or an `OutOfProcess` registration in the harness's own program, named
unconditionally because proving one means running the harness. A preview discloses INTENT, so it can
never promise less than an apply touches; health is never claimed on faith and a path is never
claimed unconfirmed. The shared contract everywhere:
`Active` only on stated evidence, else the entry is registered and the report floors at explicit
pull; fail-closed with zero writes on any unprovable config shape; ownership keys on the
sentinel/marker alone; every (un)install idempotent; topos never writes another program's
trust/consent state.

Every trigger reports in ONE shape — `topos_types::TriggerReport`, built by the crate's single
`trigger_report` constructor, which is what applies the honest-kind floor (only an `Active` report
carries the instance's live `CurrencyKind`; every other state advertises the explicit-pull floor).
`triggers::TriggerAdapter` is the ONE port: the config-merge and file-drop instances implement it,
and so do `OpenClaw` (its scheduler) and `Hermes` (its config edit) — directly, on the same types
that carry their placement half — so a caller arms, scrubs, and probes by iterating registry rows and
never learns which machinery served which harness.

## What's here

- **`claude_code`** — the reference PLACEMENT adapter (it holds no port, writes nothing): discover
  `~/.claude/skills/*/SKILL.md` through the registry's one probe; `placement_for` (sanitized display
  name → `<skill>-<workspace>` on collision → validated id). Its trigger half is the `JsonHooksSpec`
  the module also declares, run by `triggers::cc_hooks` over the shared merge — matcher-free
  `settings.json` SessionStart, `async: true`, the `--hook claude-code` dialect marker,
  sentinel-keyed `# topos:currency`. `$CLAUDE_CONFIG_DIR` honored and injected.
- **`hermes`** — BOTH ports on one type: mixed-depth discovery/placement, plus session-boundary shell
  hooks (`on_session_start`/`on_session_reset`) in `config.yaml` via an anchored line-surgical merge
  (no YAML dep); Active only on evidence from Hermes's own consent allowlist. `$HERMES_HOME` injected.
- **`openclaw`** — BOTH ports on one type: native delivery into its default-watched skills root, plus
  a silent 1-minute OpenClaw cron registered argv-only through `CommandRunner` (declaration-key
  idempotent); Active only on a successful gateway round-trip, and the only trigger that refuses the
  offline presence probe (`offline_probe_refusal`) and needs a live harness to scrub.
- **`triggers`** — the ONE trigger port over nineteen harnesses: `cc_hooks` (the strict-JSON
  session-start merge — claude-code, gemini-cli, cursor, devin, droid, qoder, github-copilot —
  parameterized by a `JsonHooksSpec` whose per-harness deviations are knobs, never forks: the entry
  shape (grouped/flat + the group `matcher`), the handler key spellings (`command`/`timeout`, or
  Copilot's `bash`/`timeoutSec`), `handler_async`, `hook_dialect`; plus codex, the one instance
  needing a SECOND surface — its `hooks.json` entry through the same engine, and `[features] hooks
  = true` line-anchored into `config.toml`, the switch without which codex reads no hooks at all —
  and never Active) and `file_drop` (one topos-owned marker-led file: opencode, goose, amp, cline,
  grok's merged `hooks/*.json` dir, pi's auto-loaded extension, kilo's auto-loaded plugin), plus the
  two shared configs neither base fits — antigravity-cli (a hooks file keyed by NAME: topos owns the
  key `topos` wholesale, writes its handler list FLAT because a matcher group would invalidate the
  whole file, and byte-preserves every other key) and kimi-code-cli (a sentinel-led `[[hooks]]`
  block appended to its own `config.toml`) — and openclaw + hermes-agent implementing the port
  themselves. The line-anchored TOML reading codex and kimi share lives in `toml_lines`; every
  engine plans a pure `EditPlan` (bytes in, write-or-leave out), which is what makes fail-closed
  zero-writes a property of the type. `adapter_for_slug` is the seam the CLI's arming sweep consumes
  AND the only place machinery is named, so the trigger-capable set is a view over the registry, not
  a second list; the one sweep spelling is composed from shared consts (`GUARDED_SWEEP` +
  `SENTINEL` = `SHELL_SWEEP_LINE`).
- **`coverage`** — whether a harness reads the shared `~/.agents/skills` dir, with PROVENANCE
  (`Probed`/`Docs`/`Unknown` — no evidence = not covered, fail closed): the claim is a registry-row
  column, over an automatic derivation for a row carrying none.
- **`mcp`** — pure MCP-server config placement for sixteen harnesses; bytes in → an `EditPlan` out,
  the CLI owns ALL file I/O. An entry names one of TWO targets (`McpTarget`): an ADDRESS the
  harness dials, or a PROGRAM it runs on this machine (command + argv + env). Both are ordinary
  managed entries — same key contract, same fingerprint ledger, same drift rules — and the CALLER
  decides which one a given harness gets, rendering every machine-local spelling before it builds
  the entry: the `cmd /c` wrapper Windows needs for Node's shim commands (`windows_wrap`, skipped
  for a WSL UNC destination — and escaping every wrapped element for the EXTRA parse that wrapper
  costs, since `cmd` reads the tail again after the spawn API has encoded it and would otherwise
  split a bridged URL at its `&`; `cmd_unescape` is the inverse the address comparison runs), the
  bridge argv. An environment slot the MACHINE fills travels as a name (`EnvValue::Inherited`)
  and is spelled by the renderer, because the form is not just syntax: most harnesses read a
  reference inside the value (`EnvRef`), Codex names inherited variables in its own `env_vars`
  list and would hand a `${VAR}` in `env` straight to the server. `entry_value` answers `None` for
  the four pairs no vendor evidence covers — a program in Hermes's, Goose's and LM Studio's
  configs, and an ADDRESS in Claude Desktop's — and every driver turns that into a refusal, never a
  guess. The
  surfaces (user/project + dialect + reload copy + the read-only `conflict_paths` a harness ALSO
  reads servers from) and the CAPABILITY columns (does it dial an address, does it run a program,
  which env-reference syntax) are registry-row
  columns; `descriptor` holds the dialect + env-reference vocabularies and the filtered views,
  over three editing drivers: `jsonc_edit` (Cursor / Claude-project /
  OpenCode strict JSON + OpenClaw JSONC — and the Claude Code plugin dir's `.mcp.json` — through
  a lossless CST, comments and formatting preserved), `toml_patch` (Codex `[mcp_servers.*]` via
  `toml_edit`) and `yaml_splice` (Hermes one-line sentinel-marked flow entries, the `hermes.rs`
  line-surgical idiom); `plugin_dir` renders only what a driver cannot know — the constant
  `.claude-plugin/plugin.json` manifest and the fresh-dir shape. Ownership keys on the `topos-`
  key prefix PLUS the caller's `key → fingerprint` ledger (sha256 over a canonical structural
  rendering, so reflow never reads as drift); a prior-mismatched entry is `Drifted` and a
  ledger-less one `Foreign` — both untouched, always; per-dialect entry shapes are exact (a
  wrong key can brick a harness) and every edit is post-verified — any unprovable input or
  verification surprise yields `Unprovable` with ZERO byte changes. `apply` refuses BEFORE
  planning an edit when the input does not re-serialize byte-identical through its own dialect
  (a BOM, unusual line endings): the round-trip precondition is the dispatcher's, not each
  driver's discretion. Beside ownership sits the question ownership cannot answer — WHAT ELSE IS
  ALREADY THERE: `observe_entries` reads every entry a surface holds, whoever wrote it, as a name
  plus the server it points at (`canonical_address`: scheme+host lowercased, default port dropped,
  bare root path equal to none, query and fragment significant — and `local_address` for an entry
  that RUNS something: its command line after the Windows wrapper is stripped, or, when that
  command line is only a bridge, the URL it bridges, so bridging a server and dialing it are one
  address), optionally at a `.`-separated
  `selector` whose `*` spans a level (`projects.*.mcpServers`). An entry whose shape is not read
  keeps its name and claims no address; an unreadable surface answers `None`, which is never an
  empty answer.
- **`registry`** — the ONE ~76-harness table: every row carries its skills dirs, detection
  probes, MCP surfaces, and shared-dir claim, so a capability is a column rather than a table.
  **The rows are DATA** — `registry.toml` at the crate root, `include_str!`d in, parsed by
  `registry::format`, and served byte-identically by the web app (`cargo xtask gen-registry`
  vendors it into `web/public/`, and `--check` gates the copy). `known_harnesses()` resolves
  THREE levels once per process behind a `OnceLock` — `~/.topos/harness-registry/override.toml`,
  then a downloaded `registry.toml` strictly newer than the bundled one, then the bundled table —
  leaking the rows so every `&'static` signature is unchanged; each failure downgrades exactly
  one level with one stderr warning. The fences are all client-side and all red-tested:
  `schema_version` equality, a `min_engine_version` ceiling (`REGISTRY_ENGINE_VERSION` — a new
  COLUMN ships behind a bump, so a build that cannot act on it keeps its own bundled table), the
  BRIDGE fence (`[mcp_bridge]` names the program topos runs for a harness that dials nothing; a
  fenced file may restate the bundled pin and may never change it, and `mcp_bridge()` reads the
  bundled table regardless — a downloaded table decides WHO needs bridging, never WHAT runs),
  strictly-greater dotted-numeric versions with equal-version-different-bytes an ERROR, the
  known-slugs-only rule (a downloaded row naming a slug the BUNDLED table does not define is
  skipped — remote data never adds a write target, and a new harness still needs a release; the
  skip is the REFRESHER's line to say, once as the table lands, so an older binary does not warn
  on every command until it self-updates — an OVERRIDE's skipped row is warned about by the loader,
  because that file is one a person wrote), path validation on every downloaded dir (including the
  bare home dir, which is never a dir a fenced row may name), no absolute root outside the bundled
  table — and NO BUNDLED ROW NAMES ONE EITHER, because the served copy is those same bytes and a
  single refused dir refuses the whole file (the landability gate in `registry::format`'s suite is
  what keeps the refresh lane able to land) — and a 256 KB ceiling. The refresher is the client's (`topos::harness_registry`, on the
  forge lane's clock); there is no hot reload — the next process reads what landed.
  `TOPOS_HARNESS_REGISTRY=bundled` skips both machine-local levels — the workspace's
  `.cargo/config.toml` sets it, so everything cargo runs in a checkout answers for the commit.
  **THREE accessors, three questions.** `known_harnesses()` is what THIS MACHINE reads today, and
  everything acting on it joins there: discovery, detection, attribution, placement, arming.
  `teardown_harnesses()` is that table UNIONED with the bundled one (dedup by slug, the loaded row
  winning): a downloaded table may legitimately SHRINK, and arming honors the omission — but the
  hook a dropped row was armed with is still in that agent's config, so the uninstall preview, the
  scrub sweep and the footprint they share read the union or `uninstall --yes` would delete the
  sidecar and leave an orphaned hook. `bundled_harnesses()` is THIS BUILD's table alone — what the
  repo's own tooling (every `cargo xtask` generator and gate) and this crate's shape pins read, so
  a check answers for the commit and not for the developer's `~/.topos/harness-registry/`.
  Which harnesses topos can PLACE into and ARM is NOT a column — it is decided per port
  (`HarnessAdapter` impls, `triggers::adapter_for_slug`). Attribution is ONE query,
  `folder_readers` (with its `folder_reader_slugs` spelling): every INSTALLED harness whose user
  dir — or project dir under `cwd` — IS a given folder, table order in, sorted by slug out. It
  shares `discover_all`'s presence gate, so a shared dir names every installed claimant instead of
  the first row that claims it, and the untracked listing, `add`'s `<name>@<slug>` resolution, and
  `publish`'s `@<slug>` check all read the same answer. Discovery resolves its ROOTS once and
  canonicalizes them there (`skills_roots_owned`, shared by `discover_all` and the public
  `skills_roots` a caller probes for what discovery cannot confirm) — `$HOME` arrives with its
  symlinks and a `cwd` arrives already resolved, so one spelling at that boundary is what keeps
  every discovered path, every exclusion compare, and `folder_readers` agreeing on one directory.
  It owns the crate's ONE root vocabulary (`Root`) + resolver (`resolve_root`/`resolve_spec`/
  `config_root` — every env override read in one place) and the ONE skill-directory probe
  (`discover_skill_dirs`, over `child_dirs` + `is_skill_dir`, which a deeper shape composes rather
  than restates). Also `choose_skill_dir`, the ONE placement-naming discipline every target dir
  follows (`topos` is `RESERVED_SKILL_DIR`, the built-in's name).

**The durable config write is the CLI's:** `install`/`remove` compute post-image bytes as a pure
merge and write through the injected `ConfigStore` port, so the adapters stay fault-injectable
without re-implementing durability. Each adapter's exact config shapes and evidence levels are
documented in its module doc; a pilot's exact build stays a MUST-VERIFY (a failed probe degrades
the report, never rebuilds an adapter).

Dependencies: `topos-types`, `serde_json`, `jsonc-parser` (the mcp CST edit), `toml_edit`,
`sha2` + `hex` (the mcp fingerprint), plus the platform std surface.
