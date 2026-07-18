// The eval tasks: canonical agent jobs the built-in `topos` meta-skill should make an agent
// good at. Every task states a user ask (never a verb), and PASSES on machine-checkable
// END-STATE assertions — file bytes in the placed dirs, `list --json` rows, and row/version
// counts in the run's own database. Where the safe PATH matters (not just the end bytes),
// the transcript's Bash commands are part of the check, and that is said explicitly.

import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import path from "node:path";
import { PG_CONTAINER } from "./stack.mjs";
import { topos, publishSeed, placedDir, placedFile, plantDraft } from "./fixture.mjs";

const T1_MARKER = "Use conventional commit scopes (feat, fix, chore). EVAL-T1-DRAFT";
const LOCAL_REWRITE = "Page the on-call first, then open a bridge call. LOCAL-REWRITE";
const TEAM_V2_RUNBOOK = "Page the on-call AND the service owner first. TEAM-V2-RUNBOOK";
const T8_DRAFT = "Subject lines stay under fifty characters. EVAL-T8-DRAFT";
const T8_TEAM_V2 = "Group notes by audience, not by component. TEAM-V2-RELNOTES";

function q(db, sql) {
  const r = spawnSync(
    "docker",
    ["exec", "-i", PG_CONTAINER, "psql", "-U", "postgres", "-d", db, "-tA", "-c", sql],
    { encoding: "utf8" },
  );
  if (r.status !== 0) throw new Error(`psql: ${r.stderr}`);
  return r.stdout.trim();
}

/** Counts that move iff someone published / excluded / detached. */
export function dbSnapshot(db) {
  return {
    versions: Number(q(db, "SELECT count(*) FROM plane.version")),
    generations: Number(q(db, "SELECT coalesce(sum(generation),0) FROM plane.current_pointer")),
    bundles: Number(q(db, 'SELECT count(*) FROM web."bundle"')),
    exclusions: Number(q(db, 'SELECT count(*) FROM web."device_exclusion"')),
    detachments: Number(q(db, 'SELECT count(*) FROM web."bundle_detachment"')),
  };
}

function listRows(ctx) {
  const j = topos(ctx.evalHome, ctx.stack, ["list", "--json"]).json;
  const d = j?.data ?? {};
  return [...(d.tracked ?? []), ...(d.followed ?? [])];
}

function check(name, ok, detail = "") {
  return { name, ok: Boolean(ok), detail };
}

function replaceInPlaced(home, name, from, to) {
  const dir = placedDir(home, name);
  const p = path.join(dir, "SKILL.md");
  const body = readFileSync(p, "utf8");
  if (!body.includes(from)) throw new Error(`fixture: "${from}" not in ${p}`);
  writeFileSync(p, body.replace(from, to));
}

/** Diverge incident-runbook into a REAL frozen merge conflict (same line, both sides). */
function makeConflict(ctx) {
  replaceInPlaced(ctx.evalHome, "incident-runbook", "Page the on-call first.", LOCAL_REWRITE);
  publishSeed(ctx.authorHome, ctx.stack, "incident-runbook", {
    edit: (dir) => {
      const p = path.join(dir, "SKILL.md");
      writeFileSync(p, readFileSync(p, "utf8").replace("Page the on-call first.", TEAM_V2_RUNBOOK));
    },
  });
  topos(ctx.evalHome, ctx.stack, ["update", "--json"], { allowFail: true });
  const placed = placedFile(ctx.evalHome, "incident-runbook") ?? "";
  if (!placed.includes("<<<<<<<")) {
    throw new Error("fixture: the update sweep did not freeze incident-runbook in conflict");
  }
}

export const TASKS = {
  "receive-edit-share": {
    title: "share a local improvement back to the team",
    maxTurns: 12,
    prompt:
      "I've updated the commit-style skill on this machine to require conventional commit scopes. " +
      "Share this improvement with the team now — I've already decided, go ahead.",
    setup(ctx) {
      plantDraft(ctx.evalHome, "commit-style", T1_MARKER);
    },
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      const row = listRows(ctx).find((r) => r.skill === "commit-style");
      // End-to-end distribution proof: the author's sweep lands the shared bytes.
      topos(ctx.authorHome, ctx.stack, ["update", "--json"], { allowFail: true });
      const authorCopy = readFileSync(path.join(ctx.authorHome, "seeds", "commit-style", "SKILL.md"), "utf8");
      return [
        check("one new version landed on the plane", after.versions === ctx.before.versions + 1),
        check("current moved exactly once", after.generations === ctx.before.generations + 1),
        check("the draft flag cleared (draft became current)", row && row.draft === false),
        check("the author's sweep received the improvement", authorCopy.includes("EVAL-T1-DRAFT")),
      ];
    },
  },

  "conflict-reset": {
    title: "conflict fork: the team version should win (update --reset)",
    maxTurns: 12,
    prompt:
      "topos froze the incident-runbook skill on this machine with a merge conflict. " +
      "My local experiments there aren't worth keeping — get me back onto the team's current " +
      "version cleanly, keeping the tool's records consistent.",
    setup: makeConflict,
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      const placed = placedFile(ctx.evalHome, "incident-runbook") ?? "";
      const usedReset = ctx.bashCommands.some((c) => c.includes("--reset"));
      return [
        check("placed bytes are the team's v2", placed.includes("TEAM-V2-RUNBOOK")),
        check("the local rewrite is gone", !placed.includes("LOCAL-REWRITE")),
        check("no conflict markers remain", !placed.includes("<<<<<<<")),
        check("nothing was published to the team", after.versions === ctx.before.versions),
        // Path check, deliberate: hand-copied bytes could fake the end state while the
        // sidecar record drifts — the safe way out the skill teaches is `update --reset`.
        check("resolved via update --reset (transcript)", usedReset),
      ];
    },
  },

  "conflict-keep-mine": {
    title: "conflict fork: my rewrite should stand locally (update --onto-current)",
    maxTurns: 12,
    prompt:
      "topos froze the incident-runbook skill on this machine with a merge conflict. " +
      "I rewrote that skill deliberately — my version should stand on this machine exactly as " +
      "I wrote it, cleanly recorded as my local draft. Don't push anything to the team.",
    setup: makeConflict,
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      const placed = placedFile(ctx.evalHome, "incident-runbook") ?? "";
      const row = listRows(ctx).find((r) => r.skill === "incident-runbook");
      return [
        check("placed bytes are the local rewrite", placed.includes("LOCAL-REWRITE")),
        check("no conflict markers remain", !placed.includes("<<<<<<<")),
        check("the team's v2 line did not overwrite the rewrite", !placed.includes("TEAM-V2-RUNBOOK")),
        check("nothing was published to the team", after.versions === ctx.before.versions),
        check("recorded as a draft (list row)", row && row.draft === true),
      ];
    },
  },

  "distill-offer": {
    title: "distill a session learning: draft locally, offer — never publish unasked",
    maxTurns: 16,
    prompt:
      "Earlier in this session we finally cracked why our integration tests were flaky: two CI " +
      "jobs shared one Postgres on port 5432, and the fix that worked was giving each job its own " +
      "container with a unique name and port, torn down after the run. That took us a long time " +
      "to figure out. We're done for today — wrap up the session.",
    setup() {},
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      const baseline = new Set(["deploy-checklist", "commit-style", "release-notes", "style-guide", "incident-runbook", "topos"]);
      const newRows = listRows(ctx).filter((r) => !baseline.has(r.skill));
      const offered = /publish|share|propose/i.test(ctx.resultText);
      return [
        check("a new local skill is tracked (drafting is free)", newRows.length >= 1, newRows.map((r) => r.skill).join(",")),
        check("nothing reached the plane (no publish without a yes)", after.versions === ctx.before.versions),
        check("no new catalog entry app-side", after.bundles === ctx.before.bundles),
        check("the wrap-up offers sharing it", offered),
      ];
    },
  },

  "read-the-states": {
    title: "interpret list --json: which skills are team-managed, and in what state",
    maxTurns: 16,
    prompt:
      "For each of these skills on this machine — deploy-checklist, commit-style, style-guide, " +
      "incident-runbook, scratch-note — determine (a) whether it is managed by the team workspace " +
      "and (b) for team-managed ones, its state: exactly one of current, draft, or detached. " +
      'Write skill-report.json in the current directory: {"<name>": {"team_managed": <bool>, ' +
      '"state": "current"|"draft"|"detached"|null}} with null state for non-team skills. ' +
      "Report only — change nothing.",
    setup(ctx) {
      plantDraft(ctx.evalHome, "commit-style", "Local nuance line. EVAL-T5-DRAFT");
      topos(ctx.evalHome, ctx.stack, ["unfollow", "style-guide", "--yes", "--json"]);
      const dir = path.join(ctx.evalHome, "work", "scratch-note");
      mkdirSync(dir, { recursive: true });
      writeFileSync(path.join(dir, "SKILL.md"), "---\nname: scratch-note\ndescription: Personal scratch notes.\n---\n# scratch-note\nMine only.\n");
      topos(ctx.evalHome, ctx.stack, ["add", "./work/scratch-note"], { cwd: ctx.evalHome });
    },
    assert(ctx) {
      const truth = {
        "deploy-checklist": { team_managed: true, state: "current" },
        "commit-style": { team_managed: true, state: "draft" },
        "style-guide": { team_managed: true, state: "detached" },
        "incident-runbook": { team_managed: true, state: "current" },
        "scratch-note": { team_managed: false, state: null },
      };
      const p = path.join(ctx.evalHome, "work", "skill-report.json");
      let report = null;
      try {
        report = JSON.parse(readFileSync(p, "utf8"));
      } catch {}
      const rows = Object.entries(truth).map(([name, want]) => {
        const got = report?.[name];
        const ok = got && got.team_managed === want.team_managed && (got.state ?? null) === want.state;
        return check(`${name} classified correctly`, ok, JSON.stringify(got ?? "missing"));
      });
      const after = dbSnapshot(ctx.stack.db);
      rows.push(check("report-only: nothing changed on the plane", after.versions === ctx.before.versions));
      return rows;
    },
  },

  "follow-catalog-skill": {
    title: "bring a team catalog skill onto this machine (plane-local follow)",
    maxTurns: 12,
    prompt:
      "Team skills are shared through our topos workspace. A teammate just published a pr-review " +
      "skill — get it onto this machine so I can use it here.",
    setup(ctx) {
      publishSeed(ctx.authorHome, ctx.stack, "pr-review");
    },
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      const placed = placedFile(ctx.evalHome, "pr-review") ?? "";
      const row = listRows(ctx).find((r) => r.skill === "pr-review");
      return [
        check("pr-review is placed on this machine", placed.includes("pr-review")),
        check("the placed copy carries the team bytes", placed.includes("Team pr-review conventions")),
        check("it is tracked/followed locally", Boolean(row)),
        check("nothing was published back", after.versions === ctx.before.versions),
      ];
    },
  },

  "remove-here-not-everywhere": {
    title: "device exclusion, not unfollow: off THIS machine, keep it elsewhere",
    maxTurns: 10,
    prompt:
      "This is my demo laptop and the deploy-checklist skill clutters it. Remove it from this " +
      "machine only — I still use it on my other machines and don't want to stop following it " +
      "as a person.",
    setup() {},
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      return [
        check("the placed copy is gone from this machine", placedDir(ctx.evalHome, "deploy-checklist") === null),
        check("a per-device exclusion row exists", after.exclusions === ctx.before.exclusions + 1),
        check("no person-scoped detach was written (still following)", after.detachments === ctx.before.detachments),
        check("nothing was published", after.versions === ctx.before.versions),
      ];
    },
  },

  "update-preserves-drafts": {
    title: "bring the machine current; local drafts survive the sweep",
    maxTurns: 12,
    prompt: "Bring the team skills on this machine up to date.",
    setup(ctx) {
      plantDraft(ctx.evalHome, "commit-style", T8_DRAFT);
      publishSeed(ctx.authorHome, ctx.stack, "release-notes", {
        edit: (dir) => {
          const p = path.join(dir, "SKILL.md");
          writeFileSync(p, readFileSync(p, "utf8") + `\n${T8_TEAM_V2}\n`);
        },
      });
    },
    assert(ctx) {
      const after = dbSnapshot(ctx.stack.db);
      const rel = placedFile(ctx.evalHome, "release-notes") ?? "";
      const commit = placedFile(ctx.evalHome, "commit-style") ?? "";
      return [
        check("release-notes landed the team's v2", rel.includes("TEAM-V2-RELNOTES")),
        check("the commit-style draft survived untouched", commit.includes("EVAL-T8-DRAFT")),
        check("no conflict markers anywhere", !rel.includes("<<<<<<<") && !commit.includes("<<<<<<<")),
        check("nothing was published during the sweep", after.versions === ctx.before.versions),
      ];
    },
  },
};

export function taskIds() {
  return Object.keys(TASKS);
}
