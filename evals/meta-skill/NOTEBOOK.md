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
--yes` — 7 Bash calls, 2.4x the output tokens, 46% more wall time.

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

## 2026-07-17 — external review (codex) + hardening

**Hypothesis:** a strict reviewer finds isolation or honesty gaps the smoke did not.

**Findings + resolutions:**

1. *Env inheritance* (P1): the driven agent inherited the operator's full environment;
   redirecting three variables is not isolation. FIXED: the agent env is now an allowlist
   (redirected homes, PATH, pinned bash, locale) — no ambient keys or tokens reach it.
2. *Credential lifetime* (P1): the seeded Claude credential was never deleted. FIXED: wiped
   from the fixture config dir the moment the agent run ends, error or not.
3. *Repo reachability* (P1): with permissions bypassed, the driven agent could in principle
   modify the checkout (skills dirs, Cargo inputs, CI). Structurally out of scope for an
   env-level harness (no OS sandbox), so it is now a MEASURED invariant instead: every run
   compares `git status --porcelain` before/after and FAILS if the agent touched the repo.
   The residual is stated in the README.
4. *Fakeable transcript check* (P2): `--reset` matched any Bash string. FIXED: requires a
   real `topos … update … --reset` invocation.
5. *Errored result could pass* (P2): FIXED — a run-level invariant fails any run whose agent
   result is an error (budget/turn exhaustion included).
6. *Arithmetic* (P3): the smoke wall-time delta is 46%, not 44%. Fixed above.

**Re-validation:** `node run.mjs --task follow-catalog-skill --arm with` on the hardened
runner: PASS, 39.9 s wall, 9 turns, $0.492 — all four task checks plus both new run-level
invariants green, and the seeded credential is gone from the fixture after the run. The
allowlisted env changed nothing about the agent's ability to do the task.

**Verdict:** review findings closed at the root where structural (env allowlist, credential
wipe, invariants), stated honestly where inherent (no OS sandbox). Numbers unchanged within
noise; the cost projection stands.

---

## 2026-07-18 — the full matrix: 8 tasks × 2 arms × 3 reps, claude-opus-4-8

**Hypothesis:** with the built-in `topos` skill present, the driven agent passes more of the
judgment tasks and spends less (turns / output tokens / wall) getting there; a 3-rep majority
per cell turns single-run noise into a stable verdict. The separation should concentrate on the
tasks that need a mental model the CLI `--help` alone does not hand you (distilling a learning,
holding a conflict fork, publishing back cleanly) and stay flat where opus can brute-force from
first principles.

**Command:** the serial orchestrator over `run.mjs` — 48 runs (8 tasks × {with,without} × 3),
one fresh composed stack + fixture home per run, results appended to `.runs/results.jsonl`.

**Run health:** 48/48 completed, 0 infra retries, 0 infra failures, repo-hygiene invariant green
on every run. Wall clock 51.0 min; summed agent wall 44.9 min (rest is per-run stack boot);
total $23.18 — inside the notebook's $22–45 projection, near the floor.

**Numbers (median per cell; pass = runs passing / 3):**

| task | arm | pass | wall | turns | out tok | cost |
|---|---|---|---|---|---|---|
| receive-edit-share | with | 1/3 | 57.4 s | 10 | 2641 | $0.530 |
| receive-edit-share | without | 2/3 | 167.7 s | 10 | 4250 | $0.585 |
| conflict-reset | with | 3/3 | 39.9 s | 9 | 2172 | $0.526 |
| conflict-reset | without | 3/3 | 50.5 s | 8 | 3078 | $0.479 |
| conflict-keep-mine | with | 3/3 | 88.3 s | 9 | 4097 | $0.617 |
| conflict-keep-mine | without | 2/3 | 100.5 s | 11 | 6152 | $0.686 |
| distill-offer | with | 3/3 | 78.4 s | 9 | 3859 | $0.589 |
| distill-offer | without | 0/3 | 46.2 s | 4 | 2892 | $0.378 |
| read-the-states | with | 3/3 | 24.2 s | 3 | 1574 | $0.333 |
| read-the-states | without | 3/3 | 31.9 s | 5 | 1834 | $0.396 |
| follow-catalog-skill | with | 3/3 | 34.0 s | 9 | 1409 | $0.502 |
| follow-catalog-skill | without | 3/3 | 46.6 s | 7 | 1910 | $0.445 |
| remove-here-not-everywhere | with | 3/3 | 29.4 s | 7 | 1147 | $0.429 |
| remove-here-not-everywhere | without | 3/3 | 24.2 s | 5 | 1238 | $0.359 |
| update-preserves-drafts | with | 3/3 | 26.3 s | 6 | 860 | $0.394 |
| update-preserves-drafts | without | 3/3 | 43.2 s | 6 | 1898 | $0.430 |

Totals: **with 22/24, without 19/24.**

**distill-offer — the decisive separation (with 3/3, without 0/3).** The prompt hands the agent
a hard-won session learning (per-CI-job Postgres containers on unique ports) and says only "wrap
up the session." All three WITHOUT runs read that as a chat sign-off: they summarize and stop at
4 turns / $0.38, tracking no new skill (`newRows=0` every time) and — in one — not even naming
sharing. All three WITH runs draft the learning into a local skill and offer to contribute it
back, publishing nothing unasked (versions/bundles unmoved). This behavior — recognize a durable
learning, draft it locally, offer once — is exactly the skill's "distill what this session
figured out" section, and opus does not do it on its own. Cleanest signal in the matrix.

**conflict-keep-mine — with 3/3, without 2/3 (turn-cap exhaustion).** The single WITHOUT failure
was not a wrong answer: the agent ran to 13 turns against the task's 12 cap and the run-level
"finished without error" invariant caught the exhaustion. WITHOUT also carries the cost — median
11 turns and 6152 output tokens vs WITH's 9 turns / 4097 (≈33% fewer). The skill keeps the
conflict-fork resolution inside budget; the bare agent sometimes explores its way out of it.

**receive-edit-share — with 1/3 < without 2/3, and it is a task-design false negative, not a
skill gap.** The three local publish checks (a new version landed, current moved once, draft
flag cleared) passed in EVERY with-arm run — the share always succeeded end to end. The only
failing check, `the author's sweep received the improvement`, asserts that the opaque scaffolding
token `EVAL-T1-DRAFT` planted in the draft survives byte-for-byte into the author's converged
copy (`tasks.mjs:94`). In both failing with-arm runs the agent explicitly recognized the token as
junk and removed it before publishing — verbatim from the transcript: *"the draft carries a stray
`EVAL-T1-DRAFT` marker that shouldn't go to the team… I'll strip it and tidy the heading before
sharing."* The real improvement (conventional commit scopes) DID propagate to the author; the
author's copy converged to the cleaned `## Scopes` section, just without the test marker. So the
check keys on a token the skill correctly teaches the agent to strip — the with-arm is penalized
for producing the higher-quality publish. The two passing arms (with-r2, both without passes)
simply published the draft verbatim, marker intact; the third without run was a genuine miss (0
versions landed — never published). Fix: assert the author's converged bytes equal what eval-home
actually published (content/hash convergence), never the presence of an injected marker a
well-behaved share removes. Until then, read this cell as "skill improves the artifact," not
"skill fails."

**The five flat tasks (both 3/3) still favor with on efficiency.** Where opus can brute-force the
task from `--help`, correctness matches — but WITH is cheaper/faster almost everywhere: lower
median output tokens on 7 of 8 tasks and lower wall on 6 of 8. Sharpest: update-preserves-drafts
860 vs 1898 out-tok (≈55% less) and 26 vs 43 s; follow-catalog 1409 vs 1910; read-the-states
3 vs 5 turns. The WITHOUT arm repeatedly pokes at sidecar internals (`follows.json`, `--help`
probes, one failed invocation) that the skill steers straight past — same result, more spend.

**Harness bugs noticed:**
1. *(task design, real)* `receive-edit-share` assertion `tasks.mjs:94` keys on the opaque
   `EVAL-T1-DRAFT` marker surviving the publish; the skill teaches the agent to strip such
   scaffolding, so a correct share can fail the check. Replace with content/hash convergence
   (author copy == published bytes). The other two marker checks are robust — `EVAL-T8-DRAFT`
   tests LOCAL draft survival through an update (agent never edits it) and `EVAL-T5-DRAFT` only
   induces a draft state the assertion reads back structurally.
2. *(cosmetic)* every preserved per-run dir under `.runs/` is suffixed `-r1` regardless of rep
   (the rep index is not threaded into the dir name), so reps collide by name and must be told
   apart by timestamp. `results.jsonl` and the `cell-*.log` files carry the correct rep; only the
   on-disk dir label is wrong.

**Verdict:** the meta-skill earns its place. One decisive capability gain (distill-offer 0→3 — a
behavior opus lacks natively), one reliability gain (the conflict fork stays inside budget), and
equal correctness at lower cost across the brute-forceable rest. The lone apparent regression is
an assertion penalizing the skill's own good judgment; corrected, the matrix reads with ≥ without
on every task. Recommend fixing `tasks.mjs:94` and re-running receive-edit-share (3 reps × 2
arms) to confirm the flip.

---
