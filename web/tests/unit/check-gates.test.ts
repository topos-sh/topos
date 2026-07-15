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

describe("check-boundary self-test", () => {
  it("passes a clean fixture", () => {
    const app = fixtureApp({
      "lib/example.server.ts": "export const fine = 1;\n",
    });
    expect(runGate(BOUNDARY, app).code).toBe(0);
  });

  it("fires on randomBytes outside the two mints", () => {
    const app = fixtureApp({
      "lib/rogue.server.ts": 'import { randomBytes } from "node:crypto";\n',
    });
    const run = runGate(BOUNDARY, app);
    expect(run.code).toBe(1);
    expect(run.output).toContain("randomBytes");
  });

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

  it("fires on the custody lane spelled outside app/lib/plane/", () => {
    const app = fixtureApp({
      "routes/rogue.ts":
        'export function loader() { requireDeviceActor(0); return "/internal/v1/workspaces"; }\n',
    });
    const run = runGate(BOUNDARY, app);
    expect(run.code).toBe(1);
    expect(run.output).toContain("/internal/v1");
  });

  it("fires on a guardless data-reading route", () => {
    const app = fixtureApp({
      "routes/secret-page.tsx": "export async function loader() { return { leak: true }; }\n",
    });
    const run = runGate(BOUNDARY, app);
    expect(run.code).toBe(1);
    expect(run.output).toContain("without an auth guard");
  });

  it("fires on the retired acting-email header", () => {
    const app = fixtureApp({
      "lib/rogue.server.ts": 'headers.set("x-topos-acting-email", email);\n',
    });
    const run = runGate(BOUNDARY, app);
    expect(run.code).toBe(1);
    expect(run.output).toContain("x-topos-acting-email");
  });
});

describe("check-email-authz self-test", () => {
  it("passes a clean fixture (email as display data)", () => {
    const app = fixtureApp({
      "routes/members.tsx": "export const label = (r: { email: string }) => r.email;\n",
    });
    expect(runGate(EMAIL, app).code).toBe(0);
  });

  it("fires on an email equality branch, with file:line", () => {
    const app = fixtureApp({
      "lib/rogue.server.ts": "const admit = user.email === invited.email;\n",
    });
    const run = runGate(EMAIL, app);
    expect(run.code).toBe(1);
    expect(run.output).toMatch(/rogue\.server\.ts:1/);
  });

  it("fires on a Drizzle email predicate and a SQL template predicate", () => {
    const app = fixtureApp({
      "lib/rogue1.server.ts": "where(eq(user.email, presented))\n",
      // biome-ignore lint/suspicious/noTemplateCurlyInString: the ${ IS the violation under test.
      "lib/rogue2.server.ts": "sql`SELECT 1 FROM seats WHERE email = ${presented}`\n",
    });
    const run = runGate(EMAIL, app);
    expect(run.code).toBe(1);
    expect(run.output).toContain("rogue1.server.ts:1");
    expect(run.output).toContain("rogue2.server.ts:1");
  });

  it("fires on the retired canonicalization defenses", () => {
    const app = fixtureApp({
      "lib/rogue.server.ts": "export { normalizeEmail } from './old';\n",
    });
    const run = runGate(EMAIL, app);
    expect(run.code).toBe(1);
    expect(run.output).toContain("normalizeEmail");
  });

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
