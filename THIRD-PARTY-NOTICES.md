# Third-party notices

Topos is Apache-2.0 (see `LICENSE`). A few small pieces of it were **adapted from, or grounded in,
other open-source projects** — a rule, a table of field names, a platform quirk somebody else
worked out first. None of those projects' code ships here verbatim; every item below was
reimplemented in this repo's own idiom, and is listed with what was taken and where it came from.

Runtime dependencies carry their own licenses and are not repeated here — `cargo tree` and
`bun pm ls` list them.

## MCP client configuration shapes

**[farion1231/cc-switch](https://github.com/farion1231/cc-switch)** — MIT, Copyright (c) 2025 Jason
Young.
Adapted: the Windows rule for spawning Node's shim commands — the list of commands that are batch
files there (`npx`, `npm`, `yarn`, `pnpm`, `node`, `bun`, `deno`), the `cmd /c <command> <args…>`
wrapper shape, and the WSL UNC exception (`\\wsl$\`, `\\wsl.localhost\`), including its documented
limit that a mapped drive letter is undetectable. Reimplemented in
`crates/topos-harness/src/mcp/mod.rs`.

**[neiii/bridle](https://github.com/neiii/bridle)** — MIT, Copyright (c) 2025 d0.
Grounding: the per-harness MCP entry field tables — Claude Code's `command`/`args`/`env`, OpenCode's
`type: "local"` with the program and its arguments as ONE `command` array and `environment` beside
it, and the per-harness environment-variable reference spellings (`${VAR}` / `{env:VAR}`). Used as
evidence for the shapes rendered in `crates/topos-harness/src/mcp/`; no code copied.

**[gleanwork/mcp-config](https://github.com/gleanwork/mcp-config)** — MIT, Copyright (c) 2025 Glean
Technologies Inc.
Grounding: the client descriptors that name each agent's config file, its servers property, and its
stdio property mapping — Codex's `mcp_servers` with `command`/`args`/`env`, Cursor's `mcpServers`,
OpenCode's `local`/`remote` type values. Used as a second independent source for the same shapes; no
code copied.

**[geelen/mcp-remote](https://github.com/geelen/mcp-remote)** — MIT, Copyright (c) 2025 Cloudflare,
Inc.
Ground truth for the bridge Topos runs for agents that cannot dial an MCP address themselves: the
`--header` argument grammar, the `${VAR}` expansion mcp-remote performs on header values from its
own environment, and the documented Cursor/Windows workaround of keeping the space out of the
argument and inside the environment value. The package itself is executed via `npx`, never vendored.

## Server document rules

**[modelcontextprotocol/registry](https://github.com/modelcontextprotocol/registry)** — MIT, moving
to Apache-2.0 (contributions whose authors have not consented to relicensing remain MIT).
Grounding: the `server.json` schema revision this codebase mirrors, and the publish-time validation
rules it applies (version pinning per registry type, the structured argument and environment-input
shapes). Reimplemented in `bins/topos/src/mcp_validate.rs` and `web/app/lib/mcp/validate.server.ts`;
no code copied.
