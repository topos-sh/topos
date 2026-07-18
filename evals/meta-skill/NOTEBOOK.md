# Lab notebook — meta-skill eval

Append-only. Each entry: hypothesis / commands / numbers / verdict. Raw per-run records live in
`.runs/results.jsonl` (gitignored); this file carries the numbers that matter and the reasoning.

---

## 2026-07-17 — harness bring-up probes

**Hypothesis:** the composed stack (fresh DB → `topos-plane` binary → web production build →
real claim + device-flow enrollment) can be driven entirely out of process, and a headless
Claude Code can be sandboxed to a fixture home without touching the operator's environment.

**Probes + findings:**

1. *Sandboxed auth.* `claude -p` with `HOME`/`CLAUDE_CONFIG_DIR` redirected reports
   "Not logged in": with `HOME` moved, the macOS keychain search list is empty. Extracting the
   operator's keychain item (`security find-generic-password -s "Claude Code-credentials" -w`)
   into `$CLAUDE_CONFIG_DIR/.credentials.json` (0600) authenticates. A haiku probe returned
   the expected reply at $0.016. The runner does this extraction at run time; the file lives
   in the gitignored run dir only.
2. *Skill discovery.* Claude Code 2.1.214 lists a probe skill placed in
   `$CLAUDE_CONFIG_DIR/skills/` but NOT one in `$HOME/.agents/skills/` — while the placement
   engine treats claude-code as covered by the shared dir, so an unscoped follow lands bytes
   where this Claude Code build never looks. Fixture answer: scope every followed skill (and
   the built-in) to the native dir with `follow <name> --agent claude-code --yes` — the
   product's own device-local placement policy. Worth a product look someday: if current
   Claude Code builds don't read the shared dir, the coverage claim may be stale.
3. *Determinism.* The session-start auto-update hook is stripped from the fixture settings in
   both arms; with it armed, `update --quiet` at session start would auto-converge exactly the
   states several tasks measure.
4. *Full dry run (no model).* Stack boot → claim → two enrollments → 5 seeds published →
   eval home lands them → conflict fixture froze `incident-runbook` with real diff3 markers.
   `list --json` rows carry `{skill, draft, status, source}`; the frozen conflict reads
   `status: "draft", draft: true`; the built-in reads `source: "built-in"`. DB snapshot after
   fixture: versions=5, generations=5, bundles=5, exclusions=0, detachments=0 — the
   assertion helpers key off these counts moving (or not moving).

**Verdict:** harness works end to end without spending model tokens; ready for a smoke task.

---

## 2026-07-17 — smoke: `follow-catalog-skill`, both arms, claude-opus-4-8

**Hypothesis:** the harness produces honest pass/fail + metrics on a real model run in both
arms; the meta-skill shows up as a cleaner, cheaper path even where opus can brute-force the
task without it.

**Command:** `node run.mjs --task follow-catalog-skill --arm both`

**Numbers (1 repetition each):**

| arm | pass | wall | turns | cost | input | output | cache write | cache read |
|---|---|---|---|---|---|---|---|---|
| with | PASS (4/4 checks) | 34.6 s | 7 | $0.460 | 12 | 987 | 31,917 | 231,371 |
| without | PASS (4/4 checks) | 50.5 s | 8 | $0.466 | 14 | 2,329 | 27,927 | 255,997 |

**Command paths (from the transcripts):** the WITH arm ran the skill's own playbook — survey
(`topos list --json`, `list --remote --json`), two-phase follow (bare describe, then
`follow pr-review --yes`), verify — 4 Bash calls, all `--json` envelopes. The WITHOUT arm
explored: `which topos`, read `~/.topos/follows.json` directly (sidecar internals the skill
steers around), `follow --help`, one failed invocation (`follow --skill pr-review` with no
enrollment context), then recovered via the qualified path `follow acme/skills/pr-review
--yes` — 7 Bash calls, 2.4x the output tokens, 44% more wall time.

**Verdict:** harness PROVEN end to end on a real model — honest assertions (the without arm
legitimately passed; opus can discover the CLI from `--help` on an easy task), and the
meta-skill's value on this task shows as path quality: fewer calls, no failed invocation,
describe-before-apply, machine envelopes. Pass-rate separation should come from the harder
tasks (the conflict fork, the distill consent bar, remove-vs-unfollow) — that is what the
full matrix measures.

**Full-matrix cost projection (instead of running it):**

- Matrix: 8 tasks x 2 arms x 3 repetitions (3 because a single stochastic agent run is
  noise; majority-of-3 gives a stable per-cell verdict) = **48 runs**.
- The smoke run processed ~264k tokens (with) / ~286k (without), ≈ $0.46 per run, ~87%
  cache reads. The smoke task is among the cheapest (low turn cap, no conflict state);
  turn caps across the suite average 12.75 vs the smoke's 12, and the hard tasks will run
  closer to their caps, especially without the skill.
- Central estimate at 1.5x smoke usage per run:
  48 runs x ~275k tokens x 1.5 ≈ **20M processed tokens** (cache-read dominated),
  48 x $0.46 x 1.5 ≈ **$33**.
- Bounds: $22 (if every task behaved like the smoke) to ~$45 (2x, hard tasks hitting turn
  caps). Add ~$0.70 per extra single-task iteration during assertion tuning.
- Wall time: ~2.5 min per run including stack boot ≈ 2 hours serial for the matrix.

---
