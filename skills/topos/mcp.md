# MCP servers (the other kind of bundle)

A bundle whose one file is a `server.json` is an MCP server — tools, not instructions. The document
names an ADDRESS every machine dials, a PACKAGE every machine runs (npm or PyPI, at one pinned
version), or both; it never carries a credential, and the sign-in stays the agent's own. There are
exactly TWO doors:

```
topos add weather                        # a server the workspace publishes — no flag needed
topos add --kind mcp ./tools/weather     # a folder holding a server.json — applies, undo-led
topos publish weather                    # share it: same verb, same consent bar as a skill
```

A workspace reference needs no flag — the catalog records what each bundle is. `--kind mcp` is for a
FOLDER, which is just a folder until somebody says what it holds; without it a `server.json`-rooted
folder refuses rather than landing as a skill, and `--kind skill` adopts that same folder as a
skill. topos fetches NOTHING: a registry name or an https link to a document is refused. To bring a
server the workspace does not have yet, the human adds it on the web (its MCP servers page takes a
registry name, a URL, or the pasted document), then every machine adds it by name. Placement is
SILENT and per-agent: no
skill folder is written — each detected agent gets ONE entry in its own MCP config under an
immutable `topos-…` key, so never hand-edit those entries or rename them (a rename strands the
agent's OAuth sign-in). An entry a human edited reads `drifted` and is left byte-identical forever.

WHAT each agent gets is decided per agent: the address where the agent dials one, the address
through `npx -y mcp-remote@<pinned>` where it cannot, else the pinned package (npm before PyPI). An
empty environment slot is written as a REFERENCE to a variable of that name (`${VAR}`, or
`{env:VAR}` for OpenCode) — the name travels, the value never does. What this machine cannot set up
says so and retries on the next sweep: `not placed: needs node, which is not on this machine.` ·
`not placed: this bundle is packaged as <type>, which this version of topos cannot set up yet.`
Relay those lines — installing the runtime is the whole fix.

The gate is the same client-side and server-side, and refuses BEFORE anything is written:
`MCP_PACKAGE_UNPINNED` (a package without one exact version), `MCP_NO_STREAMABLE_REMOTE`
(neither a usable remote nor a package), `MCP_INSECURE_URL`,
`MCP_URL_TEMPLATE` (a `{placeholder}` endpoint), `MCP_SECRET_REFUSED` (a credential in ANY form —
`isSecret`, a value-less header, per-installation variables, or a literal that merely looks like a
token), `MCP_INVALID`, `MCP_NAME_TAKEN` (publish only). Never work around one by editing the
document to hide the shape — a shared bundle carries no credential, and the sign-in belongs to the
agent on the machine.

RELAY the receipt's per-agent lines to the human, because the last step is theirs: Claude Code
loads next session (`/reload-plugins` reloads live, sign in with `/mcp`) · Codex needs a restart
and `codex mcp login <name>` · Cursor a restart · OpenCode a restart (it signs in on the first
401) · OpenClaw picks it up automatically (`openclaw mcp login <name>`) · Hermes takes
`/reload-mcp`. In a PROJECT scope only project-level configs are written — openclaw and
hermes-agent have none and report `not placed`, receiving the server through the machine scope
instead.
