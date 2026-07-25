# Topos documentation site

The source for the official Topos documentation, published at
**[topos.sh/docs](https://topos.sh/docs)**. Built with [Mintlify](https://mintlify.com); the content
is plain MDX, so it is also readable straight from this repository on GitHub.

```
docs/
├── docs.json              # site config: theme, colors, fonts, navigation
├── .mintignore            # files in this folder that are NOT site pages
├── index.mdx              # the landing page
├── quickstart.mdx  install.mdx
├── concepts/              # the model: skills, manifests, references, sessions, channels, governance
├── motions/               # task-oriented: connect, receive, publish, review, curate, import, roll back…
├── harnesses/             # how harness integration works + the supported table
├── self-hosting/          # deploy, first run, configuration, mail, TLS, Postgres, backups, upgrades, ops
├── reference/             # topos.toml, the JSON envelope, security model, glossary, troubleshooting
│   └── cli.mdx            # GENERATED — see below
├── cli.md                 # GENERATED, and NOT a site page (see .mintignore)
├── logo/  favicon.svg
└── scripts/sync-cli-reference.mjs
```

## Working on it locally

```sh
cd docs
npx mint dev --port 3333   # http://localhost:3333
npx mint broken-links
```

Requires Node 20+. Use an explicit `--port`: the product web app also defaults to 3000, and if both
are up the more specific bind wins, so you end up probing the wrong server.

## The generated CLI reference

There are two generated copies of the command reference, and neither is hand-written:

| File | What it is |
|---|---|
| `docs/cli.md` | Rendered from the real clap command tree by `cargo xtask gen-cli-ref`. Linked from the README, and byte-identical to `skills/topos/reference.md` (the built-in skill's copy). **Excluded from the site** by `.mintignore` so it is not published twice. |
| `docs/reference/cli.mdx` | The site page. Re-wraps the bytes of `docs/cli.md` with frontmatter and an intro. |

After changing the CLI:

```sh
cargo xtask gen-cli-ref                     # regenerates docs/cli.md (+ the skill's copy)
node docs/scripts/sync-cli-reference.mjs    # re-wraps it as the site page
```

To check for staleness (suitable for a CI gate):

```sh
node docs/scripts/sync-cli-reference.mjs --check
```

## What lives in `docs/` and what does not

`docs/` is the documentation **site**. Maintainer runbooks do not belong here — they would be
published as pages. The release runbook is `RELEASING.md` at the repository root; contributor
guidance is `CONTRIBUTING.md`; the design doc is `ARCHITECTURE.md`.

Anything that must sit in this folder without being published goes in `.mintignore`, which uses
`.gitignore` syntax and removes files from the site entirely (not merely from the navigation).

## Writing conventions

- **Say what is true today**, including gaps. Where something is not built, or is a known rough edge,
  name it in a `<Note>` or `<Warning>` rather than implying it works.
- **Prefer the code as the authority.** If a page and the code disagree, the code is right and the
  page gets fixed in the same change.
- **Vocabulary**: workspace · seat · session · manifest · profile · channel · skill · version ·
  `current` · draft · proposal. Not: device, fleet, follow/unfollow, channel membership. The
  glossary carries a retired-words table.
- **These docs are read by agents too.** Keep commands copy-pasteable and complete, keep tables
  parseable, and prefer explicit over clever.

## Deployment — topos.sh/docs

The site deploys from this subdirectory of the public repository, so docs travel with the code and
land in the same review. It is served at a **subpath** of the main site rather than a `docs.`
subdomain.

Setup, once:

1. **Mintlify dashboard → Git Settings** — connect this repository, enable *"docs.json is in a
   subdirectory"*, and set the path to `/docs` (no trailing slash).
2. **Mintlify dashboard → Custom domain** — enable the *"Host at"* toggle, domain `topos.sh`, base
   path `/docs`. The dashboard then generates a pre-configured Cloudflare Worker script.
3. **Cloudflare** — deploy that Worker in front of `topos.sh` and add the domain under the Worker's
   Domains & Routes. The Worker proxies `/docs*` to the Mintlify origin and passes everything else
   through to the product app untouched.

Two things the Worker must keep working, both already handled by Mintlify's generated script but
worth re-checking after any edit:

- **`/.well-known/*` must pass through**, both for ACME challenges and because the app serves the
  agent-skills discovery index there.
- The proxied request's `Host` header must be the `*.mintlify.site` target, not `topos.sh`, or
  domain verification fails.

Everything outside `/docs` — `/install`, `/agent`, `/api`, the protocol card, and every workspace
address — continues to be served by the product app. The workspace slug `docs` is already on the
reserved list, so no workspace can ever collide with the documentation path.

Pushes to `main` deploy automatically.

Mintlify additionally serves, at no extra effort, the machine-readable surfaces agents want: every
page as plain markdown at its URL + `.md`, plus `/llms.txt` and `/llms-full.txt`.
