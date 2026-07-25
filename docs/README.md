# Topos documentation site

The source for the official Topos documentation. Built with [Mintlify](https://mintlify.com); the
content is plain MDX, so it is also readable straight from this repository on GitHub.

```
docs-site/
├── docs.json              # site config: theme, colors, fonts, navigation
├── index.mdx              # the landing page
├── quickstart.mdx  install.mdx  for-agents.mdx
├── concepts/              # the model: skills, manifests, references, sessions, channels, governance
├── motions/               # task-oriented: connect, receive, publish, review, curate, import, roll back…
├── harnesses/             # how harness integration works + the supported table
├── self-hosting/          # deploy, first run, configuration, mail, TLS, Postgres, backups, upgrades, ops
├── reference/             # topos.toml, the JSON envelope, security model, glossary, troubleshooting
├── cli/reference.mdx      # GENERATED — see below
├── logo/  favicon.svg
└── scripts/sync-cli-reference.mjs
```

## Working on it locally

```sh
cd docs-site
npx mint dev            # http://localhost:3000
npx mint broken-links   # link check
```

Requires Node 20+.

## The generated CLI reference

`cli/reference.mdx` is **not hand-written**. It is derived from `docs/cli.md`, which is itself
generated from the real clap command tree by `cargo xtask gen-cli-ref`. One renderer, so the site can
never describe a flag the binary does not have.

After changing the CLI:

```sh
cargo xtask gen-cli-ref                          # regenerates docs/cli.md (+ the skill's copy)
node docs-site/scripts/sync-cli-reference.mjs    # re-wraps it as the site page
```

To check for staleness (suitable for a CI gate):

```sh
node docs-site/scripts/sync-cli-reference.mjs --check
```

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

## Deployment

The site is designed to deploy from this subdirectory of the public repository, so docs travel with
the code and land in the same review.

Setup, once: connect the repository in the Mintlify dashboard, enable **"docs.json is in a
subdirectory"** under Git Settings, and set the path to `/docs-site` (no trailing slash). Pushes to
`main` then deploy automatically.

Mintlify additionally serves, at no extra effort, the machine-readable surfaces agents want: every
page as plain markdown at its URL + `.md`, plus `/llms.txt` and `/llms-full.txt`.
