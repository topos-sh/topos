// Fixture homes for the meta-skill eval.
//
// Two sandboxed homes per run, both enrolled into the stack's workspace through the REAL
// device flow (call-1 pends → the signed-in owner approves at /verify → the re-invoked
// `follow --yes` persists the one bearer credential and lands the subscribed set):
//   - the AUTHOR home: publishes the seed skills and moves `current` for "behind"/conflict states;
//   - the EVAL home: the machine the driven agent operates. Its skills are scoped to the
//     claude-code native dir (`follow <skill> --agent claude-code --yes`) because the driven
//     Claude Code discovers skills in $CLAUDE_CONFIG_DIR/skills, not the shared dir (probed —
//     see NOTEBOOK.md), and the session-start auto-update hook is stripped so every state
//     change in a task is the DRIVEN AGENT's doing, identical in both arms.

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import path from "node:path";
import { WS_NAME, repoRoot } from "./stack.mjs";

export const SEEDS = ["deploy-checklist", "commit-style", "release-notes", "style-guide", "incident-runbook"];

const SEED_BODY = {
  "deploy-checklist": "Run the migration gate before every deploy.\nVerify the health endpoint after rollout.\n",
  "commit-style": "Write imperative one-line subjects.\nKeep the body honest about the why.\n",
  "release-notes": "Lead with the user-visible change.\nCredit the reporter of a fixed bug.\n",
  "style-guide": "Prefer plain words over jargon.\nOne idea per sentence.\n",
  "incident-runbook": "Page the on-call first.\nSnapshot logs before any restart.\nWrite the timeline as you go.\n",
};

/**
 * Environment for the topos CLI and the driven agent inside a fixture home. ALLOWLISTED, not
 * inherited: the driven agent must not see the operator's ambient env (API keys, tokens,
 * cloud credentials) — it gets the redirected home vars, a PATH with the repo binary first,
 * and a pinned bash so no operator shell config leaks in.
 */
export function homeEnv(home, stack) {
  const root = repoRoot();
  const env = {
    HOME: home,
    TOPOS_HOME: path.join(home, ".topos"),
    CLAUDE_CONFIG_DIR: path.join(home, ".claude"),
    TOPOS_PLANE_URL: stack.origin,
    PATH: `${path.join(root, "target/debug")}:${process.env.PATH}`,
    SHELL: "/bin/bash",
    TERM: process.env.TERM ?? "dumb",
  };
  for (const k of ["TMPDIR", "LANG", "LC_ALL"]) if (process.env[k]) env[k] = process.env[k];
  return env;
}

/** Run the topos binary in a home; returns {status, stdout, stderr, json} (json when parseable). */
export function topos(home, stack, args, { cwd, allowFail = false } = {}) {
  const root = repoRoot();
  const r = spawnSync(path.join(root, "target/debug/topos"), args, {
    cwd: cwd ?? home,
    env: homeEnv(home, stack),
    encoding: "utf8",
    timeout: 60_000,
  });
  let json = null;
  if (r.stdout) {
    try {
      json = JSON.parse(r.stdout);
    } catch {}
  }
  if (r.status !== 0 && !allowFail) {
    throw new Error(`topos ${args.join(" ")} → exit ${r.status}\nstdout: ${r.stdout}\nstderr: ${r.stderr}`);
  }
  return { status: r.status, stdout: r.stdout, stderr: r.stderr, json };
}

function newHome(dir) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(path.join(dir, ".claude", "skills"), { recursive: true });
  mkdirSync(path.join(dir, "work"), { recursive: true });
  return dir;
}

/** Enroll a home into the stack's workspace through the real device flow. */
export async function enroll(home, stack) {
  const call1 = topos(home, stack, ["follow", stack.address(), "--json"]);
  const pending = call1.json?.data?.pending;
  if (!pending?.user_code) {
    throw new Error(`follow call-1 did not pend: ${call1.stdout}`);
  }
  await stack.approveDevice(pending.user_code);
  const resumed = topos(home, stack, ["follow", "--yes", "--json"]);
  if (!resumed.json?.ok) throw new Error(`follow resume failed: ${resumed.stdout}`);
  return resumed.json;
}

/** Publish (or re-publish) a seed skill from the author home's working copy. */
export function publishSeed(authorHome, stack, name, { edit } = {}) {
  const seedDir = path.join(authorHome, "seeds", name);
  if (!existsSync(seedDir)) {
    mkdirSync(seedDir, { recursive: true });
    writeFileSync(
      path.join(seedDir, "SKILL.md"),
      `---\nname: ${name}\ndescription: Team ${name} conventions. Use when the task touches ${name.replace(/-/g, " ")}.\n---\n# ${name}\n\n${SEED_BODY[name] ?? "Team conventions.\n"}`,
    );
    writeFileSync(path.join(seedDir, "NOTES.md"), `Seed notes for ${name} (v1).\n`);
    topos(authorHome, stack, ["add", `./seeds/${name}`], { cwd: authorHome });
  }
  if (edit) edit(seedDir);
  const r = topos(authorHome, stack, ["publish", name, "--yes", "-m", edit ? `${name} v-next` : `${name} v1`, "--json"]);
  if (!r.json?.ok) throw new Error(`publish ${name} failed: ${r.stdout}`);
  return r.json;
}

/** The eval home's placed copy of a skill: native claude dir first, then the shared dir. */
export function placedDir(home, name) {
  for (const p of [path.join(home, ".claude", "skills", name), path.join(home, ".agents", "skills", name)]) {
    if (existsSync(p)) return p;
  }
  return null;
}

export function placedFile(home, name, file = "SKILL.md") {
  const dir = placedDir(home, name);
  if (!dir) return null;
  const p = path.join(dir, file);
  return existsSync(p) ? readFileSync(p, "utf8") : null;
}

/** Strip the session-start auto-update hook so state changes are the driven agent's doing. */
function stripSessionHook(home) {
  const settings = path.join(home, ".claude", "settings.json");
  if (!existsSync(settings)) return;
  const doc = JSON.parse(readFileSync(settings, "utf8"));
  if (doc.hooks) {
    delete doc.hooks.SessionStart;
    if (Object.keys(doc.hooks).length === 0) delete doc.hooks;
    writeFileSync(settings, JSON.stringify(doc, null, 2));
  }
}

/**
 * Build the base fixture for one run: claim the workspace, enroll both homes,
 * publish the seed set, land it on the eval home, scope placements native.
 */
export async function buildFixture(stack, runDir) {
  const authorHome = newHome(path.join(runDir, "author-home"));
  const evalHome = newHome(path.join(runDir, "eval-home"));
  await stack.claimOwner();

  await enroll(authorHome, stack);
  for (const name of SEEDS) publishSeed(authorHome, stack, name);

  await enroll(evalHome, stack); // lands the seed set via `everyone` in the same invocation
  for (const name of [...SEEDS, "topos"]) {
    topos(evalHome, stack, ["follow", name, "--agent", "claude-code", "--yes", "--json"]);
  }
  for (const name of [...SEEDS, "topos"]) {
    if (!placedFile(evalHome, name)) throw new Error(`fixture: ${name} not placed in the eval home`);
  }
  stripSessionHook(evalHome);
  return { authorHome, evalHome };
}

/** Plant a local draft in the eval home's placed copy (an edit ahead of the followed version). */
export function plantDraft(evalHome, name, marker) {
  const dir = placedDir(evalHome, name);
  const p = path.join(dir, "SKILL.md");
  writeFileSync(p, readFileSync(p, "utf8") + `\n## Local addition\n\n${marker}\n`);
}

/** Arm selection: `without` opts the device out of the built-in meta-skill, durably. */
export function applyArm(evalHome, stack, arm) {
  if (arm === "without") {
    topos(evalHome, stack, ["remove", "topos", "--yes", "--json"]);
    if (placedDir(evalHome, "topos")) throw new Error("arm=without: builtin still placed");
  } else if (!placedFile(evalHome, "topos")) {
    throw new Error("arm=with: builtin not placed");
  }
}
