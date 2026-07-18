#!/usr/bin/env node
// The notebook's per-cell table, generated from the results log — so a notebook entry pastes
// numbers instead of hand-computing them.
//
//   node evals/meta-skill/report.mjs                          # everything in .runs/results.jsonl
//   node evals/meta-skill/report.mjs --since 2026-07-17T20:00:00Z
//   node evals/meta-skill/report.mjs --file some/results.jsonl
//
// Per (task, arm): pass x/n, median wall s, median turns, median output tokens, median API-equivalent cost —
// then per-arm totals. Tolerant of missing fields (older rows, errored runs). Rows carrying an
// `infra` marker (a rate-limited/never-booted cell the runner recorded honestly) are EXCLUDED
// from verdicts and medians and listed separately — an infra failure is not a model verdict.

import { readFileSync } from "node:fs";
import path from "node:path";
import { taskIds } from "./tasks.mjs";

const HERE = path.dirname(new URL(import.meta.url).pathname);

function parseArgs(argv) {
  const a = { file: path.join(HERE, ".runs", "results.jsonl"), since: null };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (k === "--file") a.file = argv[++i];
    else if (k === "--since") a.since = argv[++i];
    else throw new Error(`unknown arg: ${k}`);
  }
  return a;
}

function median(xs) {
  const v = xs.filter((x) => typeof x === "number" && Number.isFinite(x)).sort((a, b) => a - b);
  if (v.length === 0) return null;
  const mid = Math.floor(v.length / 2);
  return v.length % 2 ? v[mid] : (v[mid - 1] + v[mid]) / 2;
}

const fmt = {
  wall: (ms) => (ms == null ? "—" : `${(ms / 1000).toFixed(1)} s`),
  turns: (t) => (t == null ? "—" : String(Math.round(t * 10) / 10)),
  tok: (t) => (t == null ? "—" : String(Math.round(t))),
  cost: (c) => (c == null ? "—" : `$${c.toFixed(3)}`),
};

const args = parseArgs(process.argv.slice(2));
let raw = "";
try {
  raw = readFileSync(args.file, "utf8");
} catch {
  console.log(`no results file at ${args.file}`);
  process.exit(0);
}
const rows = [];
for (const line of raw.split("\n")) {
  if (!line.trim()) continue;
  try {
    rows.push(JSON.parse(line));
  } catch {}
}
const inScope = rows.filter((r) => !args.since || (r.at ?? "") >= args.since);
const infra = inScope.filter((r) => r.infra);
const runs = inScope.filter((r) => !r.infra);
if (runs.length === 0 && infra.length === 0) {
  console.log(`no result rows in ${args.file}${args.since ? ` since ${args.since}` : ""}`);
  process.exit(0);
}

// Task order: the current task set first (declaration order), then anything else in the log
// (older/renamed tasks) in first-seen order. Arms: with, then without, then anything else.
const order = [...taskIds()];
for (const r of runs) if (!order.includes(r.task)) order.push(r.task);
const armsSeen = ["with", "without"];
for (const r of runs) if (!armsSeen.includes(r.arm)) armsSeen.push(r.arm);

const lines = ["| task | arm | pass | wall | turns | out tok | api-equiv cost |", "|---|---|---|---|---|---|---|"];
for (const task of order) {
  for (const arm of armsSeen) {
    const cell = runs.filter((r) => r.task === task && r.arm === arm);
    if (cell.length === 0) continue;
    lines.push(
      `| ${task} | ${arm} | ${cell.filter((r) => r.pass).length}/${cell.length}` +
        ` | ${fmt.wall(median(cell.map((r) => r.wallMs)))}` +
        ` | ${fmt.turns(median(cell.map((r) => r.turns)))}` +
        ` | ${fmt.tok(median(cell.map((r) => r.tokens?.output)))}` +
        ` | ${fmt.cost(median(cell.map((r) => r.costUsd)))} |`,
    );
  }
}
console.log(lines.join("\n"));

const totals = armsSeen
  .map((arm) => {
    const a = runs.filter((r) => r.arm === arm);
    if (a.length === 0) return null;
    const cost = a.reduce((s, r) => s + (r.costUsd ?? 0), 0);
    return `${arm} ${a.filter((r) => r.pass).length}/${a.length} (total ${fmt.cost(cost)})`;
  })
  .filter(Boolean);
console.log(`\nTotals: ${totals.join(", ")}.`);

if (infra.length > 0) {
  const list = infra.map((r) => `${r.task}[${r.arm}]r${r.rep}`).join(", ");
  console.log(`Excluded ${infra.length} infra row${infra.length === 1 ? "" : "s"} (not verdicts): ${list}.`);
}
