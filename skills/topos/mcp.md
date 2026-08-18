# MCP servers (the other kind of bundle)

An MCP server is tools, not instructions — and it is not files at all. It is a CATALOG ENTRY the
workspace connects to (or a server the workspace wrote down itself), delivered as the `server.json`
document inline. The document names an ADDRESS every machine dials, a PACKAGE every machine runs
(npm or PyPI, at one pinned version), or both; it never carries a credential, and the sign-in stays
the agent's own.

```
topos add weather                              # one the workspace already shares — no flag
topos add --kind mcp io.github.acme/weather    # share a NEW one, and get it here
topos add --kind mcp https://acme.example/server.json
```

A workspace reference needs no flag — the workspace records what each bundle is. `--kind mcp` is
how a NEW server gets shared: the name (or the https link) is SUBMITTED to the workspace, which
reads the document, rules on it, and answers with the name it shares the server as; the row then
lands and the configs converge in the same command. A name the install's catalog carries is a
CONNECTION any member may make; anything else writes the workspace's OWN server down, which takes
an owner — relay that refusal rather than retrying it. With several workspaces logged in here, name
one: `topos --workspace <ws> add --kind mcp <name>`.

A server only THIS machine runs is not shared at all — it is a hand-written line in the person's own
`topos.toml` (`"~/work/weather" = { kind = "mcp" }`), read fresh on every `topos update`. A plain
`topos add ./that-folder` refuses rather than landing it as a skill and says both ways out;
`--kind skill` adopts that same folder as a skill.

NEVER `topos publish` a server, and never `topos log`/`revert`/`review`/`update <name>@<version>`
one: it holds no files and no version history here — this machine holds the one revision it was
given. All of those refuse by name. A connection follows the workspace's latest version by default;
a pinned one is delivered exactly, and a pinned version withdrawn from the catalog is still placed
and DISCLOSED on the receipt (`a pin is a promise`) — relay that line, do not act on it.

Placement is SILENT and per-agent: no skill folder is written — each detected agent gets ONE entry
in its own MCP config under an immutable `topos-…` key, so never hand-edit those entries or rename
them (a rename strands the agent's OAuth sign-in). An entry a human edited reads `drifted` and is
left byte-identical forever.

WHAT each agent gets is decided per agent: the address where the agent dials one, the address
through `npx -y mcp-remote@<pinned>` where it cannot, else the pinned package (npm before PyPI, and
only one spoken to over stdio). An empty environment slot travels as a NAME, written in the form
that agent reads (`${VAR}`, `{env:VAR}` for OpenCode, an `env_vars` list for Codex) — the name
travels, the value never does. What this machine cannot set up says so and retries on the next
sweep: `not placed: needs node, which is not on this machine.` ·
`not placed: this bundle is packaged as <type>, which this version of topos cannot set up yet.` ·
`not placed: this bundle's package serves over <transport>, which this version of topos cannot set up yet.`
Relay those lines — installing the runtime is the whole fix.

When a server misbehaves, run `topos verify <name>`: topos dials the address (or runs the package)
and asks the server for its tools, live, storing nothing. RELAY the one line it prints — it is the
whole answer, and its exit code repeats it: `responding (N tools)` = 0 ·
`sign-in required - healthy; your agent app completes sign-in on first use` = 3 (the RIGHT answer for
an OAuth server — topos holds no credential) ·
`sign-in required - this server accepts only pre-registered clients or tokens; an agent cannot complete sign-in by itself`
= 3 as well (also healthy, and NOT something to retry: tell the human they need a personal token or
an OAuth app registered with the vendor first, once per person or per organization) ·
`not reachable: <reason>` = 4 ·
`reachable but not answering as an MCP server: <detail>` = 5. A refusal (no such bundle, a skill)
exits 1.

The gate runs where the server is written down — the workspace — and its sentence is what the CLI
prints, so the terminal and the browser answer alike: `MCP_PACKAGE_UNPINNED` (a package without one
exact version), `MCP_NO_STREAMABLE_REMOTE` (neither a usable remote nor a package),
`MCP_INSECURE_URL`, `MCP_URL_TEMPLATE` (a `{placeholder}` endpoint), `MCP_SECRET_REFUSED` (a
credential in ANY form — `isSecret`, a value-less header, per-installation variables, or a literal
that merely looks like a token), `MCP_INVALID`, `MCP_NAME_TAKEN`. Never work around one by editing
the document to hide the shape — a shared bundle carries no credential, and the sign-in belongs to
the agent on the machine. A document THIS machine cannot place says `MCP_UNPLACEABLE` on the
receipt and leaves that bundle's standing entries exactly where they are.

RELAY the receipt's per-agent lines to the human, because the last step is theirs: Claude Code
loads next session (`/reload-plugins` reloads live, sign in with `/mcp`) · Codex needs a restart
and `codex mcp login <name>` · Cursor a restart · OpenCode a restart (it signs in on the first
401) · OpenClaw picks it up automatically (`openclaw mcp login <name>`) · Hermes takes
`/reload-mcp`. In a PROJECT scope only project-level configs are written — openclaw and
hermes-agent have none and report `not placed`, receiving the server through the machine scope
instead.
