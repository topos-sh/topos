# `topos-harness` — the `HarnessAdapter` port

The `HarnessAdapter` trait + the `ConfigStore` port + the harness impls. The one real client-side port.
Does discovery + byte-exact placement targeting + the currency-trigger (un)install.

**Implemented:** the **Claude Code** reference adapter (`claude_code`) — `discover` (probe
`~/.claude/skills/*/SKILL.md`, confirm by existence, never parse frontmatter), `placement_for`,
`currency_kind` = `SessionStart`, and the idempotent, content-blind `install_currency_trigger` /
`remove_currency_trigger` strict-JSON edit of `settings.json` (check-before-add against a `# topos:currency`
sentinel; fail-closed on a malformed/wrong-typed config; `uninstall_footprint` discloses the config path
only when our entry is present, and never as a delete target). `$CLAUDE_CONFIG_DIR` (else `$HOME/.claude`)
is honored and **injected** so tests never touch the real config.

The **OpenClaw** adapter (`openclaw`) is implemented too, mirroring the reference over its two config
artifacts: `discover` probes `~/.openclaw/skills/*/SKILL.md` the same way; `currency_kind` =
`FirstToposTouch` (honestly weaker — the topos-owned bootstrap-inject plugin file shows its
last-refreshed state, so updates surface on the first `topos` touch, never at bare session open;
`session_start` is observer-only and cron is never a currency path; the per-touch refresh of the inject
CONTENT is the sync engine's follow-on, not yet wired — the installed surface claims no update and
points at `topos pull` until it lands); install registers the plugin's path
in `openclaw.json`'s `bootstrap-extra-files` via a fresh-array (immutable-replace) edit + writes the
inert marker-carrying plugin file; every capacity failure (disabled inject flag, blown char budget,
malformed/wrong-typed config, a foreign file squatting on the plugin path) degrades to
`TriggerState::Degraded` with the `ExplicitPullOnly` floor and NO write; remove scrubs the entry first
and unlinks only the marker-confirmed file. **Build-first behind the trait:** its concrete config bytes
(key names, plugin format, char budget, gateway auto-watch) stay PROVISIONAL until a readiness probe
against the pilot's exact OpenClaw build — the checklist is in the `openclaw` module doc, and the CLI
never selects this adapter in production until then.

**Planned:** the **Hermes** concrete config bytes stay build-first behind the trait until the pilot's
real build is probed; the byte-writing materialization (atomic dir-swap) lives in the CLI's update path,
not here.

**ALL platform / harness-version dependencies live here** — the rest of the workspace stays
platform-agnostic.

**Content-blind.** The adapter answers only **where** (`discover` / `placement_for`) and **when**
(`currency_kind`); it never receives a skill's bytes, never hashes a bundle, never moves a skill file. The
only files it writes are its **own harness config surface** (`settings.json`; `openclaw.json` + the
topos-owned inject plugin file) — never a skill dir. v0 places a
skill's **exact bytes** with no frontmatter rewrite, no dialect translation between harnesses, so adding a
harness is a new impl (a directory mapping + a currency trigger), not a refactor anywhere else.

**The durable config write is the CLI's, not a second atomic-write here.** `install`/`remove` compute the
post-image bytes (a pure strict-JSON merge) and write them through an injected [`ConfigStore`] port — the
CLI implements that port by reusing its one `temp → fsync → rename → fsync-dir` sequence, so the adapter's
`&self` methods stay fault-injectable without re-implementing durability.

Dependencies: `topos-core`, `topos-types`, `serde_json`, plus the platform std surface.
