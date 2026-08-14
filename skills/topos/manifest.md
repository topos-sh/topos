# The manifest format

One `[bundles]` namespace. An entry's key IS its reference, joined with `/` — so a flat quoted key
and a grouped section spell the same row:

```toml
[bundles]
"topos.sh/acme/code-review" = "*"                          # track the team's current version
"topos.sh/acme/deploy" = "<64-hex version>"                # one exact version
"topos.sh/acme/channels/backend" = "*"                     # a whole channel
"github.com/vercel-labs/skills/find-skills" = "e173b8c"    # a commit (7-40 hex)
"./tools/my-skill" = "*"                                   # a folder in this repo
"topos.sh/acme/big-skill" = { version = "*", dest = [".agents/skills"] }
```

Placement is ONE field: `dest`, an array of destinations. A row without it reaches every agent
this machine has, now and later; a row with it lands at exactly what it names, detected or not
(skill rows: folders; MCP rows: the agents' config files). One entry is not a destination:
**`"*"`** stands for the reach the row would have with no `dest` at all, recomputed on every run —
so `dest = ["*", "~/.codex/skills"]` reads "every agent this machine has, plus that folder
always", and a row whose only field is `dest = ["*"]` is written as the plain `"*"` row. It is
what `topos add … -a <agent>` joins onto a row that named no destinations: destinations EXTEND,
and narrowing is `topos remove … -a <agent>`. The global file spells machine paths (`~/…` or
absolute), a project file spells relative paths inside the checkout. Hand-edit the array and the
next `topos update` converges it — a new entry installs, a dropped entry uninstalls (edited
copies kept, disclosed).

A CHANNEL row carries members of both kinds, so it takes two arrays: `dest` freezes its skill
members' folders and `mcp_dest` freezes its MCP members' config files. Each narrows only its own
kind; with no `mcp_dest` the channel's MCP members reach every MCP-capable agent.

Two spellings are the GLOBAL file's alone (a project manifest is a repo fact): a two-segment
`"topos.sh/acme" = "*"` row — everything that workspace currently gives you; `topos login` writes
it on the machine's first connection, and a deleted one stays deleted — and `"<ref>" = "off"`,
the one negative a file can state. Whatever the parser refuses, it refuses by naming what that
reference accepts — never guess, read the message.
