#!/usr/bin/env node
// The two-arm runner: drive headless Claude Code through each task, meta-skill placed vs absent.
//
//   node run.mjs --task follow-catalog-skill --arm both
//   node run.mjs --task all --arm both --reps 3
//   node run.mjs --task all --arm both --dry-run          # fixtures + assertions, zero model spend
//   node run.mjs --task all --arm both --reps 3 --jobs 4  # concurrent lanes (see README ceiling)
//
// Per (task, arm, rep): a FRESH database + stack + fixture (nothing shared across runs), the
// arm applied (`without` = the product's own durable `remove topos` opt-out), Claude Code
// invoked headless and SANDBOXED to the fixture home (HOME / TOPOS_HOME / CLAUDE_CONFIG_DIR
// all point inside the run dir; the driven agent never touches the real user environment),
// then the task's end-state assertions run and a JSON line lands in .runs/results.jsonl.
//
// --dry-run boots the stack, builds the fixture, applies the arm, and executes the task's
// assertions with NO agent (and no credential seeding): it proves fixtures build and
// assertions run without spending a token. Verdicts are printed but never recorded — most
// positive tasks legitimately fail with no agent, and the guard cells legitimately pass.
//
// --jobs N runs cells on N concurrent lanes. Every cell is already fully isolated — its own
// database (name minted per process+counter), its own OS-assigned ports, its own homes and
// scratch under its own run dir — so lanes share only the eval's Postgres container (each
// cell a separate database inside it) and the results log. Cells run as CHILD processes (the
// in-process cell code is synchronous by design); each appends its whole result line itself
// with one O_APPEND write, so concurrent lines never interleave. A cell whose failure looks
// like infrastructure (provider rate limit / overload, a port that never came healthy) is
// retried ONCE, then recorded honestly as an `infra` row that report.mjs excludes from
// verdicts. --jobs 1 (the default) is today's serial in-process path.
//
// Auth for the driven agent is extracted AT RUN TIME from the operator's own Claude Code
// sign-in (macOS keychain, else the config-dir credentials file) into the gitignored fixture
// config dir — never committed, wiped with the run dir.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, appendFileSync, existsSync, chmodSync, rmSync, openSync, closeSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { repoRoot, startStack } from "./stack.mjs";
import { buildFixture, applyArm, homeEnv } from "./fixture.mjs";
import { TASKS, taskIds, dbSnapshot } from "./tasks.mjs";

const DEFAULT_MODEL = "claude-opus-4-8";
const HERE = path.dirname(new URL(import.meta.url).pathname);
const RUNS = path.join(HERE, ".runs");
const IS_CHILD = Boolean(process.env.TOPOS_EVAL_CHILD);
const INFRA_EXIT = 86;

/** Failures that are the harness/provider's fault, never the driven model's verdict. */
const INFRA_RE = /\b429\b|rate.?limit|overloaded|usage limit|too many requests|quota exceeded|never healthy|ECONNREFUSED|ETIMEDOUT/i;

class InfraError extends Error {}

function isInfra(e) {
  return e instanceof InfraError || INFRA_RE.test(String(e?.message ?? ""));
}

function parseArgs(argv) {
  const a = { task: "all", arm: "both", model: DEFAULT_MODEL, reps: 1, rep: null, dryRun: false, jobs: 1 };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (k === "--task") a.task = argv[++i];
    else if (k === "--arm") a.arm = argv[++i];
    else if (k === "--model") a.model = argv[++i];
    else if (k === "--reps") a.reps = Number(argv[++i]);
    // --rep <n> runs ONE repetition labeled n (in the run-dir name AND the result row). An
    // external per-cell driver that loops reps itself must pass this, or every single-run
    // invocation defaults to rep 1 and the on-disk run dirs collide on the `-r1` suffix.
    else if (k === "--rep") a.rep = Number(argv[++i]);
    else if (k === "--dry-run") a.dryRun = true;
    else if (k === "--jobs") a.jobs = Number(argv[++i]);
    else throw new Error(`unknown arg: ${k}`);
  }
  if (!Number.isInteger(a.jobs) || a.jobs < 1) throw new Error("--jobs must be a positive integer");
  return a;
}

/** Seed the fixture config dir with the operator's own Claude Code sign-in. */
function seedClaudeAuth(evalHome) {
  const cfg = path.join(evalHome, ".claude");
  const realCfg = process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), ".claude");
  let creds = null;
  const kc = spawnSync("security", ["find-generic-password", "-s", "Claude Code-credentials", "-w"], {
    encoding: "utf8",
  });
  if (kc.status === 0 && kc.stdout.trim()) creds = kc.stdout.trim();
  if (!creds && existsSync(path.join(realCfg, ".credentials.json"))) {
    creds = readFileSync(path.join(realCfg, ".credentials.json"), "utf8");
  }
  if (!creds) throw new Error("no Claude Code credentials found (keychain or config dir) — sign in first");
  const credPath = path.join(cfg, ".credentials.json");
  writeFileSync(credPath, creds);
  chmodSync(credPath, 0o600);

  const seed = {};
  const realState = path.join(realCfg, ".claude.json");
  if (existsSync(realState)) {
    const src = JSON.parse(readFileSync(realState, "utf8"));
    for (const k of ["oauthAccount", "hasCompletedOnboarding", "userID"]) if (k in src) seed[k] = src[k];
  }
  seed.hasCompletedOnboarding = true;
  writeFileSync(path.join(cfg, ".claude.json"), JSON.stringify(seed, null, 2));
}

/** Remove the seeded operator credential from the fixture as soon as the agent run ends. */
function wipeClaudeAuth(evalHome) {
  for (const f of [".credentials.json", ".claude.json"]) {
    rmSync(path.join(evalHome, ".claude", f), { force: true });
  }
}

/** The repo checkout must be byte-untouched by the driven agent (skills/, Cargo inputs, CI). */
function repoStatus() {
  const r = spawnSync("git", ["-C", repoRoot(), "status", "--porcelain"], { encoding: "utf8" });
  if (r.status !== 0) throw new Error(`git status failed: ${r.stderr}`);
  return r.stdout;
}

/** Run headless Claude Code sandboxed to the fixture home; parse the stream-json transcript. */
function driveAgent({ evalHome, stack, prompt, model, maxTurns, transcriptPath }) {
  const args = [
    "-p",
    prompt,
    "--model",
    model,
    "--output-format",
    "stream-json",
    "--verbose",
    "--max-turns",
    String(maxTurns),
    "--max-budget-usd",
    "5",
    "--dangerously-skip-permissions",
    "--no-session-persistence",
  ];
  const started = Date.now();
  const r = spawnSync("claude", args, {
    cwd: path.join(evalHome, "work"),
    env: homeEnv(evalHome, stack),
    encoding: "utf8",
    timeout: 15 * 60 * 1000,
    maxBuffer: 64 * 1024 * 1024,
  });
  const wallMs = Date.now() - started;
  writeFileSync(transcriptPath, r.stdout ?? "");
  if (r.error) throw new Error(`claude spawn failed: ${r.error.message}`);

  let result = null;
  const bashCommands = [];
  for (const line of (r.stdout ?? "").split("\n")) {
    if (!line.trim()) continue;
    let ev;
    try {
      ev = JSON.parse(line);
    } catch {
      continue;
    }
    if (ev.type === "result") result = ev;
    if (ev.type === "assistant") {
      for (const block of ev.message?.content ?? []) {
        if (block.type === "tool_use" && block.name === "Bash" && block.input?.command) {
          bashCommands.push(block.input.command);
        }
      }
    }
  }
  if (!result) throw new Error(`no result event (exit ${r.status}); stderr: ${(r.stderr ?? "").slice(0, 2000)}`);
  if (result.is_error && /not logged in/i.test(result.result ?? "")) {
    throw new Error("driven agent is not authenticated — auth seeding failed");
  }
  if (result.is_error && INFRA_RE.test(result.result ?? "")) {
    // A rate-limited/overloaded run carries no model verdict — surface it as infrastructure
    // (retried once by the driver), never as a recorded pass/fail.
    throw new InfraError(`provider limit during agent run: ${(result.result ?? "").slice(0, 200)}`);
  }
  const u = result.usage ?? {};
  return {
    wallMs,
    apiMs: result.duration_ms ?? null,
    turns: result.num_turns ?? null,
    costUsd: result.total_cost_usd ?? null,
    tokens: {
      input: u.input_tokens ?? 0,
      output: u.output_tokens ?? 0,
      cacheCreation: u.cache_creation_input_tokens ?? 0,
      cacheRead: u.cache_read_input_tokens ?? 0,
    },
    resultText: result.result ?? "",
    isError: Boolean(result.is_error),
    bashCommands,
  };
}

async function runOne(taskId, arm, model, rep, dryRun) {
  const task = TASKS[taskId];
  if (!task) throw new Error(`unknown task ${taskId}; known: ${taskIds().join(", ")}`);
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const runDir = path.join(RUNS, `${stamp}-${taskId}-${arm}-r${rep}`);
  mkdirSync(runDir, { recursive: true });
  console.log(`\n=== ${taskId} [${arm}] rep ${rep}${dryRun ? " (dry run)" : ""} → ${runDir}`);

  const stack = await startStack(path.join(runDir, "stack"));
  try {
    const { authorHome, evalHome } = await buildFixture(stack, runDir);
    const ctx = { stack, authorHome, evalHome };
    await task.setup(ctx);
    applyArm(evalHome, stack, arm);
    ctx.before = dbSnapshot(stack.db);

    if (dryRun) {
      // No agent, no credential seeding, no spend: exercise the assertions against the
      // untouched fixture. Print the checks (positive tasks are EXPECTED to miss; the guard
      // cells are expected to pass) — record nothing.
      ctx.resultText = "";
      ctx.bashCommands = [];
      const checks = await task.assert(ctx);
      console.log(`DRY   fixtures built, ${checks.length} assertions executed`);
      for (const c of checks) console.log(`  [${c.ok ? "ok" : "MISS"}] ${c.name}${c.detail ? ` — ${c.detail}` : ""}`);
      return { task: taskId, arm, rep, dryRun: true, checks };
    }

    seedClaudeAuth(evalHome);
    const repoBefore = repoStatus();

    let metrics;
    try {
      metrics = driveAgent({
        evalHome,
        stack,
        prompt: task.prompt,
        model,
        maxTurns: task.maxTurns,
        transcriptPath: path.join(runDir, "transcript.jsonl"),
      });
    } finally {
      wipeClaudeAuth(evalHome);
    }
    ctx.resultText = metrics.resultText;
    ctx.bashCommands = metrics.bashCommands;

    const checks = await task.assert(ctx);
    // Run-level invariants, independent of the task: an errored agent result never passes,
    // and the driven agent must not have touched the repo checkout itself.
    checks.push({ name: "agent finished without error", ok: !metrics.isError, detail: "" });
    checks.push({
      name: "repo checkout untouched by the driven agent",
      ok: repoStatus() === repoBefore,
      detail: "",
    });
    const pass = checks.every((c) => c.ok);
    const record = {
      task: taskId,
      arm,
      rep,
      model,
      pass,
      checks,
      wallMs: metrics.wallMs,
      turns: metrics.turns,
      costUsd: metrics.costUsd,
      tokens: metrics.tokens,
      agentError: metrics.isError,
      runDir,
      at: new Date().toISOString(),
    };
    appendFileSync(path.join(RUNS, "results.jsonl"), JSON.stringify(record) + "\n");
    console.log(`${pass ? "PASS" : "FAIL"}  wall=${(metrics.wallMs / 1000).toFixed(1)}s turns=${metrics.turns} cost=$${metrics.costUsd?.toFixed(4)}`);
    for (const c of checks) console.log(`  [${c.ok ? "ok" : "MISS"}] ${c.name}${c.detail ? ` — ${c.detail}` : ""}`);
    return record;
  } finally {
    await stack.teardown();
  }
}

/** Record a cell whose failure was infrastructure, honestly and excludably. */
function recordInfra(cell, model, message) {
  const record = {
    task: cell.task,
    arm: cell.arm,
    rep: cell.rep,
    model,
    pass: false,
    infra: String(message).slice(0, 300),
    at: new Date().toISOString(),
  };
  appendFileSync(path.join(RUNS, "results.jsonl"), JSON.stringify(record) + "\n");
  return record;
}

/** Serial driver (also the child path): in-process cells; infra failures retried once. */
async function runSerial(cells, args) {
  const records = [];
  for (const cell of cells) {
    let attempt = 0;
    for (;;) {
      try {
        records.push(await runOne(cell.task, cell.arm, args.model, cell.rep, args.dryRun));
        break;
      } catch (e) {
        if (IS_CHILD && isInfra(e)) {
          // The parent lane owns retries; hand the classification back via the exit code.
          console.error(`INFRA: ${e.message}`);
          process.exit(INFRA_EXIT);
        }
        if (!isInfra(e) || attempt >= 1) {
          if (isInfra(e) && !args.dryRun) records.push(recordInfra(cell, args.model, e.message));
          if (isInfra(e)) {
            console.error(`infra failure (recorded, not a verdict): ${cell.task} [${cell.arm}] r${cell.rep} — ${e.message}`);
            break;
          }
          throw e;
        }
        attempt++;
        console.error(`infra failure, retrying ${cell.task} [${cell.arm}] r${cell.rep} once — ${e.message}`);
      }
    }
  }
  return records;
}

/** Parallel driver: N lanes of child processes pulling cells from one queue. */
async function runParallel(cells, args) {
  const queue = cells.map((c) => ({ ...c, attempt: 0 }));
  const outcomes = [];
  const lane = async (laneId) => {
    for (;;) {
      const cell = queue.shift();
      if (!cell) return;
      const logPath = path.join(RUNS, `cell-${cell.task}-${cell.arm}-r${cell.rep}.log`);
      const fd = openSync(logPath, "a");
      if (cell.attempt > 0) appendFileSync(logPath, "\n---- retry after infra failure ----\n");
      const childArgs = [
        path.join(HERE, "run.mjs"),
        "--task", cell.task,
        "--arm", cell.arm,
        "--rep", String(cell.rep),
        "--model", args.model,
      ];
      if (args.dryRun) childArgs.push("--dry-run");
      const code = await new Promise((resolve) => {
        const p = spawn(process.execPath, childArgs, {
          env: { ...process.env, TOPOS_EVAL_CHILD: "1" },
          stdio: ["ignore", fd, fd],
        });
        p.on("exit", (c) => resolve(c ?? 1));
        p.on("error", () => resolve(1));
      });
      closeSync(fd);
      const tail = (() => {
        try {
          return readFileSync(logPath, "utf8").slice(-4000);
        } catch {
          return "";
        }
      })();
      if (code === 0) {
        const verdict = tail.match(/^(PASS|FAIL|DRY)\b.*$/m)?.[0] ?? "done";
        console.log(`[lane ${laneId}] ${cell.task} [${cell.arm}] r${cell.rep}: ${verdict}`);
        outcomes.push({ ...cell, status: /^FAIL/.test(verdict) ? "fail" : "pass" });
      } else if ((code === INFRA_EXIT || INFRA_RE.test(tail)) && cell.attempt < 1) {
        console.log(`[lane ${laneId}] ${cell.task} [${cell.arm}] r${cell.rep}: infra failure — retrying once (${logPath})`);
        queue.push({ ...cell, attempt: cell.attempt + 1 });
      } else {
        const reason = tail.match(/^INFRA: .*$/m)?.[0] ?? `child exit ${code} — see ${logPath}`;
        console.log(`[lane ${laneId}] ${cell.task} [${cell.arm}] r${cell.rep}: ${reason} (recorded as infra, not a verdict)`);
        if (!args.dryRun) recordInfra(cell, args.model, reason);
        outcomes.push({ ...cell, status: "infra" });
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(args.jobs, cells.length) }, (_, i) => lane(i + 1)));
  return outcomes;
}

const args = parseArgs(process.argv.slice(2));
const tasks = args.task === "all" ? taskIds() : args.task.split(",").map((t) => t.trim()).filter(Boolean);
for (const t of tasks) if (!TASKS[t]) throw new Error(`unknown task ${t}; known: ${taskIds().join(", ")}`);
const arms = args.arm === "both" ? ["with", "without"] : [args.arm];
mkdirSync(RUNS, { recursive: true });

const reps = args.rep != null ? [args.rep] : Array.from({ length: args.reps }, (_, i) => i + 1);
const cells = [];
for (const rep of reps) for (const t of tasks) for (const arm of arms) cells.push({ task: t, arm, rep });

if (args.jobs > 1 && !IS_CHILD) {
  const started = Date.now();
  const outcomes = await runParallel(cells, args);
  const by = (s) => outcomes.filter((o) => o.status === s).length;
  console.log(
    `\n${args.dryRun ? "dry run: " : ""}${by("pass")} passed, ${by("fail")} failed, ${by("infra")} infra ` +
      `of ${outcomes.length} cells in ${((Date.now() - started) / 60000).toFixed(1)} min on ${args.jobs} lanes`,
  );
  if (!args.dryRun) console.log("per-cell tables: node evals/meta-skill/report.mjs");
} else {
  const records = await runSerial(cells, args);
  if (args.dryRun) {
    console.log(`\ndry run: ${records.length} cells exercised (fixtures built, assertions ran; nothing recorded)`);
  } else {
    const passed = records.filter((r) => r.pass).length;
    console.log(`\n${passed}/${records.length} runs passed`);
  }
}
