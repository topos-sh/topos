#!/usr/bin/env node
// The two-arm runner: drive headless Claude Code through each task, meta-skill placed vs absent.
//
//   node run.mjs --task follow-catalog-skill --arm both
//   node run.mjs --task all --arm both --reps 3
//
// Per (task, arm, rep): a FRESH database + stack + fixture (nothing shared across runs), the
// arm applied (`without` = the product's own durable `remove topos` opt-out), Claude Code
// invoked headless and SANDBOXED to the fixture home (HOME / TOPOS_HOME / CLAUDE_CONFIG_DIR
// all point inside the run dir; the driven agent never touches the real user environment),
// then the task's end-state assertions run and a JSON line lands in .runs/results.jsonl.
//
// Auth for the driven agent is extracted AT RUN TIME from the operator's own Claude Code
// sign-in (macOS keychain, else the config-dir credentials file) into the gitignored fixture
// config dir — never committed, wiped with the run dir.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, appendFileSync, existsSync, chmodSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { repoRoot, startStack } from "./stack.mjs";
import { buildFixture, applyArm, homeEnv } from "./fixture.mjs";
import { TASKS, taskIds, dbSnapshot } from "./tasks.mjs";

const DEFAULT_MODEL = "claude-opus-4-8";
const HERE = path.dirname(new URL(import.meta.url).pathname);
const RUNS = path.join(HERE, ".runs");

function parseArgs(argv) {
  const a = { task: "all", arm: "both", model: DEFAULT_MODEL, reps: 1 };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (k === "--task") a.task = argv[++i];
    else if (k === "--arm") a.arm = argv[++i];
    else if (k === "--model") a.model = argv[++i];
    else if (k === "--reps") a.reps = Number(argv[++i]);
    else throw new Error(`unknown arg: ${k}`);
  }
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

async function runOne(taskId, arm, model, rep) {
  const task = TASKS[taskId];
  if (!task) throw new Error(`unknown task ${taskId}; known: ${taskIds().join(", ")}`);
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const runDir = path.join(RUNS, `${stamp}-${taskId}-${arm}-r${rep}`);
  mkdirSync(runDir, { recursive: true });
  console.log(`\n=== ${taskId} [${arm}] rep ${rep} → ${runDir}`);

  const stack = await startStack(path.join(runDir, "stack"));
  try {
    const { authorHome, evalHome } = await buildFixture(stack, runDir);
    const ctx = { stack, authorHome, evalHome };
    await task.setup(ctx);
    applyArm(evalHome, stack, arm);
    seedClaudeAuth(evalHome);
    ctx.before = dbSnapshot(stack.db);
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

const args = parseArgs(process.argv.slice(2));
const tasks = args.task === "all" ? taskIds() : [args.task];
const arms = args.arm === "both" ? ["with", "without"] : [args.arm];
mkdirSync(RUNS, { recursive: true });

const records = [];
for (let rep = 1; rep <= args.reps; rep++) {
  for (const t of tasks) {
    for (const arm of arms) {
      records.push(await runOne(t, arm, args.model, rep));
    }
  }
}
const passed = records.filter((r) => r.pass).length;
console.log(`\n${passed}/${records.length} runs passed`);
