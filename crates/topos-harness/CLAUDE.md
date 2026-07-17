# `topos-harness` — the `HarnessAdapter` port

The `HarnessAdapter` trait + the `ConfigStore` port + the harness impls. The one real client-side port.
Does discovery + byte-exact placement targeting + the currency-trigger (un)install (session-start for
Claude Code and Hermes; first-`topos`-touch for OpenClaw). The registered command everywhere is the ONE
byte-stable `topos update --quiet`, which self-throttles client-side (TTL + single-flight), so a trigger
may fire on every session-shaped event cheaply.

**Implemented:** the **Claude Code** reference adapter (`claude_code`) — `discover` (probe
`~/.claude/skills/*/SKILL.md`, confirm by existence, never parse frontmatter), `placement_for` (a pure
follower's first receive names the folder by the skill's **sanitized display name** — Claude Code invokes a
skill by its folder name — namespacing by workspace on a name collision and falling back to the validated
id; the display name + workspace slug are UNTRUSTED and routed through `sanitize_skill_dir` to one safe path
component, so they can never redirect the placement), `currency_kind` = `SessionStart`, and the idempotent,
content-blind `install_currency_trigger` /
`remove_currency_trigger` strict-JSON edit of `settings.json` (check-before-add against a `# topos:currency`
sentinel; fail-closed on a malformed/wrong-typed config; `uninstall_footprint` discloses the config path
only when our entry is present, and never as a delete target). The managed entry is a **matcher-free,
`async: true`** SessionStart group (an omitted matcher fires on every source — startup, resume, clear,
compact; async so the sweep never blocks a session event), and the quiet sweep answers with the
SessionStart hook-output JSON (`reloadSkills`) when it changed bytes, so pulled skills go live
same-session. A re-arm MIGRATES a sentinel-marked entry from any earlier shape (the old `topos pull`
command; the `matcher: startup` sync handler) to the canonical one in place — on a group also holding a
user's handler, OUR handler is EXTRACTED into its own matcher-free group (their matcher would pin it to
one source) and the user's group, handlers, and matcher stay byte-identical.
`$CLAUDE_CONFIG_DIR` (else `$HOME/.claude`) is honored and **injected** so tests never touch the real
config.

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

The **Hermes** adapter (`hermes`) is implemented too, mirroring the reference structurally — `discover`
over Hermes's mixed-depth `~/.hermes/skills/` shape (`<name>/` uncategorized, `<category>/<name>/` with the
category recorded as the placement's `layer`; support dirs under a skill dir pruned), `placement_for`
(a discovered dir verbatim; the no-discovery default `skills/general/<skill_id>`), `currency_kind` =
`SessionStart` (the **session-boundary shell hooks** — one `topos update --quiet` entry under
`on_session_start` AND `on_session_reset`, each a one-line YAML flow mapping carrying an explicit
`timeout: 30`; both events probed shell-executable on a real local v0.17.0 build — the retired per-turn
`pre_llm_call` registration paid subprocess latency every turn for freshness the session-start skill
loader could not consume, and a re-arm MIGRATES a sentinel-marked legacy entry to the session events in
place, failing closed with zero writes when a user's own event blocks share the region), and the
idempotent (un)install of those entries in `config.yaml`. The known one-beat-in residual is probed and
documented: the hook fires after Hermes assembles its per-process skill INDEX (a brand-new skill enters
the index next cold launch) while skill BODIES are read from disk at `skill_view()` time, so content
updates land immediately. The config is YAML (no YAML dep exists here), so the edit is an **anchored
line-surgical merge**: it handles only the shapes it can prove (the shipped `hooks: {}` default, an
absent/empty `hooks:` key, an absent file, its own sentinel-commented entry lines under recognized event
blocks) byte-preservingly and fails closed (`Degraded`, zero writes) on everything else. Hermes gates
hooks behind a one-time `(event, command)` consent allowlist it persists itself
(`shell-hooks-allowlist.json`), keyed per exact PAIR — so the event move re-prompts once per event, and
an old per-turn approval is deliberately NOT evidence; the adapter **never writes that consent** — it
only reads it (plus Hermes's `HERMES_ACCEPT_HOOKS` / `hooks_auto_accept: true` auto-accept signals) as
evidence, and reports `Active`+`SessionStart` **only** on evidence for the `on_session_start` pair, else
the entries are registered but the report degrades honestly to `ExplicitPullOnly` — never a fake "live"
hook. `$HERMES_HOME` (else `$HOME/.hermes`) and the acceptance evidence are **injected** so tests never
touch the real home or the env. The concrete config/allowlist shapes, the shell-executable event set,
and the hook-vs-index ordering were probed against a real local Hermes Agent v0.17.0 build; the pilot's
exact build stays a MUST-VERIFY (every filename/key/line is a named const; a failed probe degrades the
report, never rebuilds the adapter).

**Planned:** the byte-writing materialization (atomic dir-swap) lives in the CLI's update path, not here;
what remains for the adapters themselves is the two pilot readiness probes above, not code.

**ALL platform / harness-version dependencies live here** — the rest of the workspace stays
platform-agnostic.

**Content-blind.** The adapter answers only **where** (`discover` / `placement_for`) and **when**
(`currency_kind`); it never receives a skill's bytes, never hashes a bundle, never moves a skill file. The
only files it writes are its **own harness config surface** (`settings.json`; `openclaw.json` + the
topos-owned inject plugin file; `config.yaml`) — never a skill dir. v0 places a
skill's **exact bytes** with no frontmatter rewrite, no dialect translation between harnesses, so adding a
harness is a new impl (a directory mapping + a currency trigger), not a refactor anywhere else.

**The durable config write is the CLI's, not a second atomic-write here.** `install`/`remove` compute the
post-image bytes (a pure merge — strict-JSON for Claude Code, line-anchored for Hermes's YAML) and write
them through an injected [`ConfigStore`] port — the CLI implements that port by reusing its one
`temp → fsync → rename → fsync-dir` sequence, so the adapter's `&self` methods stay fault-injectable
without re-implementing durability.

Dependencies: `topos-core`, `topos-types`, `serde_json`, plus the platform std surface.
