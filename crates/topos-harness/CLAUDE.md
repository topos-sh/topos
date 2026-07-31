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
validator accepts); `--hook claude-code` is the one declaration today that opts into
`reloadSkills` so pulled skills go live same-session. `trigger_present` is the hook-health probe
`list`/`auth status` read — health is never claimed on faith. The shared contract everywhere:
`Active` only on stated evidence, else the entry is registered and the report floors at explicit
pull; fail-closed with zero writes on any unprovable config shape; ownership keys on the
sentinel/marker alone; every (un)install idempotent; topos never writes another program's
trust/consent state.

## What's here

- **`claude_code`** — the reference adapter: discover `~/.claude/skills/*/SKILL.md`;
  `placement_for` (sanitized display name → `<skill>-<workspace>` on collision → validated id);
  the idempotent strict-JSON `settings.json` SessionStart entry (matcher-free + `async: true`,
  sentinel-keyed `# topos:currency`, re-arm migrates older shapes in place, user handlers kept
  byte-identical). `$CLAUDE_CONFIG_DIR` honored and injected.
- **`hermes`** — session-boundary shell hooks (`on_session_start`/`on_session_reset`) in
  `config.yaml` via an anchored line-surgical merge (no YAML dep); Active only on evidence from
  Hermes's own consent allowlist. `$HERMES_HOME` injected.
- **`openclaw`** — native delivery into its default-watched skills root; the trigger is a silent
  1-minute OpenClaw cron registered argv-only through `CommandRunner` (declaration-key
  idempotent); Active only on a successful gateway round-trip.
- **`triggers`** — auto-update triggers for nine more registry harnesses over two shared bases:
  `cc_hooks` (generalized strict-JSON session-start merge: gemini-cli, cursor, droid; plus codex
  as a line-anchored TOML merge, never Active) and `file_drop` (one topos-owned marker-led file:
  github-copilot, opencode, goose, amp, cline). `adapter_for_slug`/`supported_slugs` is the seam
  the CLI's breadth arming sweep consumes; the one sweep spelling is composed from shared consts.
- **`coverage`** — whether a harness reads the shared `~/.agents/skills` dir, with PROVENANCE
  (`Probed`/`Docs`/`Unknown` — no evidence = not covered, fail closed); override table over an
  automatic derivation from the registry.
- **`registry`** — the baked ~73-harness table (detection + skills-root resolution;
  `detected_harnesses`), plus `choose_skill_dir`, the ONE placement-naming discipline every
  target dir follows (`topos` is `RESERVED_SKILL_DIR`, the built-in's name).

**The durable config write is the CLI's:** `install`/`remove` compute post-image bytes as a pure
merge and write through the injected `ConfigStore` port, so the adapters stay fault-injectable
without re-implementing durability. Each adapter's exact config shapes and evidence levels are
documented in its module doc; a pilot's exact build stays a MUST-VERIFY (a failed probe degrades
the report, never rebuilds an adapter).

Dependencies: `topos-core`, `topos-types`, `serde_json`, plus the platform std surface.
