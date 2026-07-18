# The meta-skill eval — does the built-in `topos` skill make agents better at topos?

An offline, oak-style eval measuring the VALUE of the built-in `topos` meta-skill
(`skills/topos/`): the same headless agent, the same fixture machine, the same real plane —
one arm with the meta-skill placed, one arm with the product's own durable `remove topos`
opt-out. Pass/fail is decided by machine-checkable END-STATE assertions, never by grading prose.

**This directory is tooling, not product.** No crate, no workspace membership, nothing in CI —
`cargo build`, `cargo test`, `cargo xtask ci`, and every workflow job are byte-independent of it
(workspace members are an explicit list; the gates scan named dirs). It lives at the repo top
next to `tests/` because, like `tests/`, it drives the composed product stack — but unlike
`tests/`, running it costs real model tokens, so it is only ever run by hand.

## What a run does

Per (task, arm, repetition) — nothing shared between runs:

1. **A fresh database** on the eval's own Postgres container (`topos-bundle-d-pg`, port 5454,
   the compose file's digest-pinned `postgres:18` image), provisioned with the production
   role/schema recipe (`topos_plane` / `topos_web`, mirroring `scripts/compose-init-db.sh`).
2. **The real stack**: the `topos-plane` vault binary (self-migrates, internal token armed,
   loopback only) behind the web app's production build (`web/build`), first-boot setup + the
   real `/claim` ceremony.
3. **Two sandboxed homes**, both enrolled through the genuine browser-approval device flow
   (call-1 pends → the signed-in owner approves at `/verify` → the resumed `follow --yes`
   lands the seed set): an AUTHOR home that publishes seeds and moves `current`, and the EVAL
   home the driven agent operates.
4. **The task's state** (a planted draft, an upstream move, a frozen conflict, …) built with
   the real CLI, then the arm applied.
5. **Headless Claude Code** (`claude -p`, default model `claude-opus-4-8`) runs the task
   prompt, sandboxed to the fixture: `HOME`, `TOPOS_HOME`, and `CLAUDE_CONFIG_DIR` all point
   inside the run dir, `PATH` gets `target/debug` prepended, and `TOPOS_PLANE_URL` pins the
   fixture origin — the driven agent cannot touch the real user environment. Auth is the
   operator's own Claude Code sign-in, extracted at run time into the gitignored fixture
   config dir (never committed).
6. **Assertions** on the end state — placed-file bytes, `list --json` rows, and row/version
   counts in the run's own database — write one JSON line to `.runs/results.jsonl` with
   pass/fail, wall time, turns, token usage, and cost.

Two fixture decisions worth knowing (details + probes in `NOTEBOOK.md`):

- Placements are scoped to the Claude Code native skills dir via the product's own
  `follow <skill> --agent claude-code --yes`, because the driven Claude Code build discovers
  skills in `$CLAUDE_CONFIG_DIR/skills`, not the shared `.agents` dir.
- The session-start auto-update hook is stripped from the fixture settings in BOTH arms, so
  every state change during a task is the driven agent's doing — the eval measures the skill,
  not the ambient trigger.

## The tasks (`tasks.mjs`)

| id | the user's ask | passes when (end state) |
|---|---|---|
| `receive-edit-share` | "share my commit-style improvement with the team" | exactly one new plane version, `current` moved once, the draft flag cleared, and the AUTHOR's next sweep lands the improved bytes |
| `conflict-reset` | frozen conflict; "the team version should win" | placed bytes are the team's v2, no markers, nothing published, and the transcript shows `update --reset` (hand-copied bytes would leave the sidecar records inconsistent, so the path is part of the check) |
| `conflict-keep-mine` | frozen conflict; "my rewrite stands, as a local draft" | placed bytes are the local rewrite, no markers, nothing published, the `list` row is a draft |
| `distill-offer` | a session yielded a hard-won reusable fix; "wrap up" | a NEW local skill is tracked (drafting is free), NOTHING reached the plane (org-bound needs an explicit yes), and the wrap-up offers sharing |
| `read-the-states` | "classify these five skills: team-managed? current/draft/detached?" | the written `skill-report.json` matches the fixture's ground truth exactly; nothing changed |
| `follow-catalog-skill` | "a teammate published pr-review — get it onto this machine" | the team bytes are placed and tracked; nothing published back (all traffic stays on the loopback plane) |
| `remove-here-not-everywhere` | "off THIS machine only; I keep it elsewhere" | the placed copy is gone, a per-device exclusion row exists, NO person-scoped detach row was written |
| `update-preserves-drafts` | "bring the team skills up to date" | the behind skill lands the team's v2, the planted draft on another skill survives byte-exact, no conflicts, nothing published |

## Running it

```sh
# prerequisites (once):
cargo build -p topos -p topos-plane
cd web && bun install && bun run build && cd -
docker run -d --name topos-bundle-d-pg -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres \
  -p 5454:5432 postgres:18@sha256:4aabea78cf39b90e834caf3af7d602a18565f6fe2508705c8d01aa63245c2e20

# one task, both arms:
node evals/meta-skill/run.mjs --task follow-catalog-skill --arm both

# the full matrix (costs real tokens — see NOTEBOOK.md for the projection first):
node evals/meta-skill/run.mjs --task all --arm both --reps 3

# afterwards:
docker rm -f topos-bundle-d-pg
```

Results append to `evals/meta-skill/.runs/results.jsonl` (gitignored); conclusions are written
up by hand in `NOTEBOOK.md` — hypothesis, commands, numbers, verdict.

**Cost discipline:** every run is metered (`--max-turns` per task, `--max-budget-usd` per run).
Run ONE task end-to-end as a smoke first; project the full matrix from its measured usage
before funding it.
