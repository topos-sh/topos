# `topos-harness` — the `HarnessAdapter` port

The `HarnessAdapter` trait + the `ConfigStore` + `CommandRunner` ports + the harness impls — the
one client-side port. An adapter answers **where** (`discover` / `placement_for`) and **when**
(`currency_kind` + trigger (un)install); it is **content-blind**: it never receives a skill's
bytes, never hashes a bundle, never writes a skill dir — only its own harness config surface. v0
places exact bytes with no dialect translation, so adding a harness is a new impl, not a refactor.

**ALL platform / harness-version dependencies live here** — the rest of the workspace stays
platform-agnostic.

## The trigger contract

Every registered trigger runs the ONE sweep `topos update --quiet` (self-throttled client-side, so
firing on every session-shaped event is cheap). `--hook <harness>` names the calling trigger and
selects the sweep's stdout dialect: UNMARKED is the schema-conservative default (`hookEventName` +
`additionalContext` only, nothing when there is nothing to say — what a strict hook-output
validator accepts); `claude-code` is the one spec declaring a `hook_dialect` today, opting into
`reloadSkills` so pulled skills go live same-session. `trigger_present` is the hook-health probe
`list`/`auth status` read — health is never claimed on faith. The shared contract everywhere:
`Active` only on stated evidence, else the entry is registered and the report floors at explicit
pull; fail-closed with zero writes on any unprovable config shape; ownership keys on the
sentinel/marker alone; every (un)install idempotent; topos never writes another program's
trust/consent state.

Every trigger reports in ONE shape — `topos_types::TriggerReport`, built by the crate's single
`trigger_report` constructor, which is what applies the honest-kind floor. `triggers::TriggerAdapter`
is the ONE port: the config-merge and file-drop instances implement it directly, and the harnesses
whose trigger rides their full `HarnessAdapter` (OpenClaw's scheduler, Hermes's config edit) are
wrapped into it, so a caller arms, scrubs, and probes by iterating registry rows and never learns
which machinery served which harness.

## What's here

- **`claude_code`** — the reference adapter: discover `~/.claude/skills/*/SKILL.md` through the
  registry's one probe; `placement_for` (sanitized display name → `<skill>-<workspace>` on
  collision → validated id); the trigger is a `JsonHooksSpec` instance over the shared merge —
  matcher-free `settings.json` SessionStart, `async: true`, the `--hook claude-code` dialect
  marker, sentinel-keyed `# topos:currency`. `$CLAUDE_CONFIG_DIR` honored and injected.
- **`hermes`** — session-boundary shell hooks (`on_session_start`/`on_session_reset`) in
  `config.yaml` via an anchored line-surgical merge (no YAML dep); Active only on evidence from
  Hermes's own consent allowlist. `$HERMES_HOME` injected.
- **`openclaw`** — native delivery into its default-watched skills root; the trigger is a silent
  1-minute OpenClaw cron registered argv-only through `CommandRunner` (declaration-key
  idempotent); Active only on a successful gateway round-trip.
- **`triggers`** — the ONE trigger port over twelve harnesses: `cc_hooks` (the strict-JSON
  session-start merge — claude-code, gemini-cli, cursor, droid — parameterized by a `JsonHooksSpec`
  whose `handler_async` and `hook_dialect` knobs are the only per-harness deviations; plus codex as
  a line-anchored TOML merge, never Active) and `file_drop` (one topos-owned marker-led file:
  github-copilot, opencode, goose, amp, cline), with openclaw + hermes-agent wrapped in from their
  own adapters. `adapter_for_slug` is the seam the CLI's arming sweep consumes AND the only place
  machinery is named, so the trigger-capable set is a view over the registry, not a second list;
  the one sweep spelling is composed from shared consts.
- **`coverage`** — whether a harness reads the shared `~/.agents/skills` dir, with PROVENANCE
  (`Probed`/`Docs`/`Unknown` — no evidence = not covered, fail closed): the claim is a registry-row
  column, over an automatic derivation for a row carrying none.
- **`mcp`** — pure MCP-server config placement for six harnesses; bytes in → an `EditPlan` out,
  the CLI owns ALL file I/O. The surfaces (user/project + dialect + reload copy) are a registry-row
  column; `descriptor` holds the dialect vocabulary + the filtered views, over three editing
  drivers: `jsonc_edit` (Cursor / Claude-project /
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
  driver's discretion.
- **`registry`** — the ONE baked ~76-harness table: every row carries its skills dirs, detection
  probes, MCP surfaces, and shared-dir claim, so a capability is a column rather than a table.
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
