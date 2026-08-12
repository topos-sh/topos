import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

/**
 * RED TESTS for the two source gates — each checker is driven over a fixture tree with a
 * planted violation and must FIRE (and stay green on a clean fixture). A gate that cannot go
 * red is decoration; this suite is the proof it has teeth.
 */

const WEB_ROOT = resolve(__dirname, "..", "..");
const BOUNDARY = join(WEB_ROOT, "scripts", "check-boundary.mjs");
const EMAIL = join(WEB_ROOT, "scripts", "check-email-authz.mjs");

interface GateRun {
  code: number;
  output: string;
}

function runGate(script: string, appDir: string): GateRun {
  try {
    const output = execFileSync("node", [script, appDir], { encoding: "utf8", stdio: "pipe" });
    return { code: 0, output };
  } catch (error) {
    const failure = error as { status?: number; stdout?: string; stderr?: string };
    return { code: failure.status ?? 1, output: `${failure.stdout ?? ""}${failure.stderr ?? ""}` };
  }
}

let fixtures: string[] = [];

function fixtureApp(files: Record<string, string>): string {
  const dir = mkdtempSync(join(tmpdir(), "topos-gate-"));
  fixtures.push(dir);
  const app = join(dir, "app");
  for (const [rel, text] of Object.entries(files)) {
    const full = join(app, rel);
    mkdirSync(join(full, ".."), { recursive: true });
    writeFileSync(full, text);
  }
  return app;
}

afterEach(() => {
  for (const dir of fixtures) {
    rmSync(dir, { recursive: true, force: true });
  }
  fixtures = [];
});

/** A planted violation and the substrings the gate's complaint must name. */
interface FiringCase {
  name: string;
  files: Record<string, string>;
  needles: string[];
}

/** The RED half, written once: the gate exits 1 and says what it caught. */
function expectFires(script: string, fc: FiringCase): void {
  const run = runGate(script, fixtureApp(fc.files));
  expect(run.code).toBe(1);
  for (const needle of fc.needles) {
    expect(run.output).toContain(needle);
  }
}

/** A case table as `%s` tuples: vitest truncates a `$name` interpolation at 40 characters, so a
 * long title would report the same truncated text for two different cases. */
function titled(cases: FiringCase[]): [string, FiringCase][] {
  return cases.map((c) => [c.name, c]);
}

const BOUNDARY_VIOLATIONS: FiringCase[] = [
  {
    name: "randomBytes outside the two mints",
    files: { "lib/rogue.server.ts": 'import { randomBytes } from "node:crypto";\n' },
    needles: ["randomBytes"],
  },
  {
    name: "the custody lane spelled outside app/lib/plane/",
    files: {
      "routes/rogue.ts":
        'export function loader() { requireSessionActor(0); return "/internal/v1/workspaces"; }\n',
    },
    needles: ["/internal/v1"],
  },
  {
    name: "a guardless data-reading route",
    files: {
      "routes/secret-page.tsx": "export async function loader() { return { leak: true }; }\n",
    },
    needles: ["without an auth guard"],
  },
  {
    name: "the retired acting-email header",
    files: { "lib/rogue.server.ts": 'headers.set("x-topos-acting-email", email);\n' },
    needles: ["x-topos-acting-email"],
  },
];

describe("check-boundary self-test", () => {
  it("passes a clean fixture", () => {
    const app = fixtureApp({
      "lib/example.server.ts": "export const fine = 1;\n",
    });
    expect(runGate(BOUNDARY, app).code).toBe(0);
  });

  it.each(titled(BOUNDARY_VIOLATIONS))("fires on %s", (_name, fc) => expectFires(BOUNDARY, fc));

  it("allows randomBytes in the identity mint", () => {
    const app = fixtureApp({
      "lib/db/identity.server.ts": 'import { randomBytes } from "node:crypto";\n',
    });
    expect(runGate(BOUNDARY, app).code).toBe(0);
  });

  it("fires on a TS-side sha256 while allowing the Postgres spelling", () => {
    const clean = fixtureApp({
      "lib/db/queries.example.server.ts":
        // biome-ignore lint/suspicious/noTemplateCurlyInString: the ${ IS the fixture under test.
        "export const probe = sql`SELECT sha256(convert_to(${x}, 'UTF8')) = credential_sha256`;\n",
    });
    expect(runGate(BOUNDARY, clean).code).toBe(0);
    const dirty = fixtureApp({
      "lib/rogue.server.ts": "const h = sha256(bytes);\n",
    });
    const run = runGate(BOUNDARY, dirty);
    expect(run.code).toBe(1);
    expect(run.output).toContain("sha256");
  });
});

describe("check-boundary agent-skills digest carve-out (one exact expression)", () => {
  // The sanctioned shape — the ONLY thing the named module may spell: one bare createHash
  // import plus one advertised-digest template, exactly as agent-skills.server.ts uses them.
  const SANCTIONED =
    'import { createHash } from "node:crypto";\n' +
    "const digestOf = (skillMd: Buffer) =>\n" +
    // biome-ignore lint/suspicious/noTemplateCurlyInString: the template IS the fixture under test.
    '  `sha256:${createHash("sha256").update(skillMd).digest("hex")}`;\n';
  const MODULE = join("lib", "agent-skills.server.ts");

  it("passes the exact sanctioned expression in the named module", () => {
    const app = fixtureApp({ [MODULE]: SANCTIONED });
    expect(runGate(BOUNDARY, app).code).toBe(0);
  });

  const CARVE_OUT_VIOLATIONS: FiringCase[] = [
    {
      name: 'createHash("sha1") — the algorithm is pinned',
      files: {
        [MODULE]:
          'import { createHash } from "node:crypto";\n' +
          // biome-ignore lint/suspicious/noTemplateCurlyInString: the template IS the fixture under test.
          'const d = `sha256:${createHash("sha1").update(skillMd).digest("hex")}`;\n',
      },
      needles: ["createHash"],
    },
    {
      name: "a SECOND createHash call site — the expression is allowed once",
      files: {
        [MODULE]:
          SANCTIONED +
          // biome-ignore lint/suspicious/noTemplateCurlyInString: the template IS the fixture under test.
          'const again = `sha256:${createHash("sha256").update(other).digest("hex")}`;\n',
      },
      needles: ["createHash"],
    },
    {
      name: "createHmac in that module",
      files: { [MODULE]: `${SANCTIONED}import { createHmac } from "node:crypto";\n` },
      needles: ["createHmac"],
    },
    {
      name: "sign( in that module",
      files: { [MODULE]: `${SANCTIONED}const s = sign(payload);\n` },
      needles: ["sign("],
    },
    {
      name: "a userland sha256 spelling in that module",
      files: { [MODULE]: 'import { sha256 } from "./vendored";\nconst h = sha256(bytes);\n' },
      needles: ["sha256"],
    },
  ];

  it.each(titled(CARVE_OUT_VIOLATIONS))("fires on %s", (_name, fc) => expectFires(BOUNDARY, fc));
});

describe("check-boundary MCP digest-spelling carve-out (reading, never computing)", () => {
  // The gate READS the two published digest spellings out of somebody else's server document so
  // it can tell an integrity digest from a credential. It hashes nothing.
  const MODULE = join("lib", "mcp", "validate.server.ts");

  it("passes the recognized spellings in the named module", () => {
    const app = fixtureApp({
      [MODULE]:
        "const OCI_DIGEST = /@sha256:[a-f0-9]{64}$/;\n" +
        'const isDigest = (before: string) => before.slice(-7).toLowerCase() === "sha256:";\n' +
        'const isKey = (key: string) => key.slice(-6).toLowerCase() === "sha256";\n',
    });
    expect(runGate(BOUNDARY, app).code).toBe(0);
  });

  const MCP_CARVE_OUT_VIOLATIONS: FiringCase[] = [
    {
      name: "a computation in that module",
      files: {
        [MODULE]: 'import { createHash } from "node:crypto";\nconst h = createHash("sha256");\n',
      },
      needles: ["createHash"],
    },
    {
      name: "the spellings in any OTHER module — the carve-out is one file's",
      files: { "lib/rogue.server.ts": 'const prefix = "sha256:";\n' },
      needles: ["sha256"],
    },
  ];

  it.each(titled(MCP_CARVE_OUT_VIOLATIONS))("fires on %s", (_name, fc) =>
    expectFires(BOUNDARY, fc));
});

const EMAIL_VIOLATIONS: FiringCase[] = [
  {
    name: "an email equality branch, with file:line",
    files: { "lib/rogue.server.ts": "const admit = user.email === invited.email;\n" },
    needles: ["rogue.server.ts:1"],
  },
  {
    name: "a Drizzle email predicate and a SQL template predicate",
    files: {
      "lib/rogue1.server.ts": "where(eq(user.email, presented))\n",
      // biome-ignore lint/suspicious/noTemplateCurlyInString: the ${ IS the violation under test.
      "lib/rogue2.server.ts": "sql`SELECT 1 FROM seats WHERE email = ${presented}`\n",
    },
    needles: ["rogue1.server.ts:1", "rogue2.server.ts:1"],
  },
  {
    name: "the retired canonicalization defenses",
    files: { "lib/rogue.server.ts": "export { normalizeEmail } from './old';\n" },
    needles: ["normalizeEmail"],
  },
];

describe("check-email-authz self-test", () => {
  it("passes a clean fixture (email as display data)", () => {
    const app = fixtureApp({
      "routes/members.tsx": "export const label = (r: { email: string }) => r.email;\n",
    });
    expect(runGate(EMAIL, app).code).toBe(0);
  });

  it.each(titled(EMAIL_VIOLATIONS))("fires on %s", (_name, fc) => expectFires(EMAIL, fc));

  it("allows the three sanctioned lookups", () => {
    const app = fixtureApp({
      // biome-ignore lint/suspicious/noTemplateCurlyInString: the ${ IS the sanctioned lookup shape.
      "lib/db/identity.server.ts": "sql`WHERE email = ${lowered} AND status = 'pending'`\n",
      // biome-ignore lint/suspicious/noTemplateCurlyInString: same.
      "lib/auth/registration.server.ts": "sql`WHERE email = ${lowered}`\n",
      // biome-ignore lint/suspicious/noTemplateCurlyInString: same.
      "lib/auth/recovery.server.ts": "sql`WHERE email = ${lowered}`\n",
    });
    expect(runGate(EMAIL, app).code).toBe(0);
  });
});
