# The manifest format

Top level: `schema = 1`, a project file's one `workspace = "<host>/<workspace>"` line, then the
kind sections. A row's KEY is its name (bare in a project — resolved against the workspace line;
spelled in full in the machine file); its VALUE says which version and, for repo skills and
folders, where it comes from:

```toml
schema = 1
workspace = "topos.sh/acme"

[skills]
code-review = "latest"                       # track the team's current version
deploy      = "<64-hex version>"             # one exact version
find-skills = "github:vercel-labs/skills"    # a repo skill (pin: github:o/r#<commit>)
my-skill    = "./tools/my-skill"             # a folder in this repo
big-skill   = { dest = [".agents/skills"] }

[mcp]
linear = "latest"

[channels]
backend = "latest"                           # a whole channel
```

A project also commits the generated `topos.lock` — exactly which version each row resolved to.
`topos install` places what the lock records (filling entries for new rows, never moving one);
`topos update` re-resolves the follow rows, rewrites the lock, and shows the diff — commit it.
An unknown section warns and is skipped (a newer topos wrote it), never a refusal.

Placement is ONE field: `dest`, an array of destinations. A row without it reaches every agent
PICKED at that scope, now and later (a project file answers to `<project>/.topos/agents.json` and
the machine file to `~/.topos/agents.json`, and neither falls back to the other; `topos agents`
prints the one where you stand); a row with it lands at exactly what it names, picked or not
(skill rows: folders; MCP rows: the agents' config files). One entry is not a destination:
**`"*"`** stands for the reach the row would have with no `dest` at all, recomputed on every run —
so `dest = ["*", "~/.codex/skills"]` reads "every agent picked here, plus that folder always".
`topos add … -a <agent>` SETS the row to exactly the folders that run names, and every agent it
names must be in the pick; `topos remove … -a <agent>` subtracts one. The machine file spells
machine paths (`~/…` or absolute), a project file spells relative paths inside the checkout.
Hand-edit the array and the next `topos install` converges it — a new entry installs, a dropped
entry uninstalls (edited copies kept, disclosed).

A CHANNEL row carries members of both kinds, so it takes two arrays: `dest` freezes its skill
members' folders and `mcp_dest` freezes its MCP members' config files. Each narrows only its own
kind; with no `mcp_dest` the channel's MCP members reach every MCP-capable agent PICKED here. In a
project, the lock freezes a channel to its resolved member list — a member the server adds arrives as a
`topos update` diff, never silently.

Two spellings are the MACHINE file's alone (a project file is a repo fact): the `[workspaces]`
row — `"topos.sh/acme" = "latest"`, everything that workspace currently gives you; `topos login`
writes it on the machine's first connection, and a deleted one stays deleted — and
`"<full-ref>" = "off"`, the one negative a file can state. Whatever the parser refuses, it
refuses by naming what that key accepts — never guess, read the message.
