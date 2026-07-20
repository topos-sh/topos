import { createHash } from "node:crypto";
import { beforeAll, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The machine-discovery lane: /llms.txt + the agent-skills discovery index and the built-in
 * skill's files under /.well-known/agent-skills/. The invariants under test:
 *  - the index digest equals the sha256 of the exact bytes the file route serves (computed
 *    from the SAME read, so this locks the by-construction no-drift claim);
 *  - the legacy /.well-known/skills/index.json is byte-identical to the canonical path;
 *  - llms.txt is a constant plain-text document under its size budget.
 * Vitest runs with cwd = web/, so the BUILTIN_SKILL_DIR default resolves to the repo's own
 * skills/topos — the routes read the real committed files here.
 */

let indexLoader: () => Promise<Response>;
let legacyLoader: () => Promise<Response>;
let fileLoader: (args: { request: Request; params: Record<string, string> }) => Promise<Response>;
let llmsLoader: () => Promise<Response>;

beforeAll(async () => {
  installTestEnv();
  ({ loader: indexLoader } = await import("@/routes/agent-skills-index"));
  ({ loader: legacyLoader } = await import("@/routes/agent-skills-index-legacy"));
  ({ loader: fileLoader } = (await import("@/routes/agent-skills-file")) as never);
  ({ loader: llmsLoader } = await import("@/routes/llms-txt"));
});

function fetchFile(name: string): Promise<Response> {
  return fileLoader({
    request: new Request(`http://localhost/.well-known/agent-skills/topos/${name}`),
    params: { file: name },
  });
}

describe("the agent-skills discovery index", () => {
  it("serves the 0.2.0 shape: one skill-md entry named topos, path-absolute URL, sha256 digest", async () => {
    const res = await indexLoader();
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("application/json; charset=utf-8");
    const index = await res.json();
    expect(index.$schema).toBe("https://schemas.agentskills.io/discovery/0.2.0/schema.json");
    expect(index.skills).toHaveLength(1);
    const [skill] = index.skills;
    expect(skill.name).toBe("topos");
    expect(skill.type).toBe("skill-md");
    expect(skill.url).toBe("/.well-known/agent-skills/topos/SKILL.md");
    expect(skill.digest).toMatch(/^sha256:[0-9a-f]{64}$/);
    // The description is the skill's own frontmatter scalar, inside the RFC's cap.
    expect(skill.description.length).toBeGreaterThan(0);
    expect(skill.description.length).toBeLessThanOrEqual(1024);
  });

  it("advertises the digest of the exact bytes the file route serves", async () => {
    const index = await (await indexLoader()).json();
    const [skill] = index.skills;
    const served = await fetchFile("SKILL.md");
    expect(served.status).toBe(200);
    const bytes = Buffer.from(await served.arrayBuffer());
    const digest = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
    expect(skill.digest).toBe(digest);
  });

  it("serves all three skill files as markdown, and a constant 404 for any other name", async () => {
    for (const name of ["SKILL.md", "INSTALL.md", "reference.md"]) {
      const res = await fetchFile(name);
      expect(res.status).toBe(200);
      expect(res.headers.get("content-type")).toBe("text/markdown; charset=utf-8");
      expect((await res.text()).length).toBeGreaterThan(0);
    }
    for (const name of ["nope.md", "SKILL.md.bak", ".."]) {
      const miss = await fetchFile(name).catch((thrown: unknown) => thrown as Response);
      expect(miss).toBeInstanceOf(Response);
      expect((miss as Response).status).toBe(404);
    }
  });

  it("the legacy /.well-known/skills/index.json is byte-identical to the canonical path", async () => {
    const canonical = await indexLoader();
    const legacy = await legacyLoader();
    expect(await legacy.text()).toBe(await canonical.text());
    expect(legacy.headers.get("content-type")).toBe(canonical.headers.get("content-type"));
    expect(legacy.headers.get("cache-control")).toBe(canonical.headers.get("cache-control"));
  });
});

describe("llms.txt", () => {
  it("serves plain text, starts at the H1, and stays under its 40-line budget", async () => {
    const res = await llmsLoader();
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/plain; charset=utf-8");
    const text = await res.text();
    expect(text.startsWith("# Topos\n")).toBe(true);
    // The convention's shape: a blockquote summary follows the H1.
    expect(text).toContain("\n> ");
    expect(text.trimEnd().split("\n").length).toBeLessThan(40);
  });
});
