import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { MCP_BRAND_MARKS, mcpBrandMark } from "@/lib/mcp/brand-marks";
import { createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * THE MARKS, HELD TO THE PICKER'S ONE PROMISE: every catalog row draws SOMETHING deliberate — the
 * brand's own mark where the vendored set carries it, the app's MCP glyph where it does not — and
 * neither arm can turn into a broken image.
 *
 * The failure this suite exists to make impossible is a row that names a mark nobody vendored:
 * `icon = 'canva'` against a set with no Canva in it renders an empty box that reads as a bug.
 * The rows are the catalog's own now, so the check runs against a migrated database rather than
 * against a list in code — which also catches a later seed adding a key nobody drew.
 */

let db: ScratchDb;
let icons: string[] = [];

beforeAll(async () => {
  db = await createScratchDb("web_mcp_marks");
  const rows = await db.q<{ icon: string }>(
    "SELECT DISTINCT icon FROM web.mcp_server WHERE icon IS NOT NULL ORDER BY icon",
  );
  icons = rows.map((row) => row.icon);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("every catalog row either resolves a mark or falls back cleanly", () => {
  it("resolves every icon key the catalog names", () => {
    expect(icons.length).toBeGreaterThan(0);
    for (const icon of icons) {
      const mark = mcpBrandMark(icon);
      // The whole point: a named key that resolves to nothing would render an empty box.
      expect(mark, `no vendored mark for icon key "${icon}"`).toBeDefined();
      expect(mark?.brand.length).toBeGreaterThan(0);
    }
  });

  it("draws the app's own glyph for a row that names no mark", () => {
    // The fallback arm, stated: no key, so `McpMark` draws the Plug. Nothing to resolve.
    expect(mcpBrandMark(undefined)).toBeUndefined();
  });
});

describe("the vendored set itself", () => {
  it("draws every mark as one path on the upstream 24×24 grid", () => {
    for (const [key, mark] of Object.entries(MCP_BRAND_MARKS)) {
      expect(key, "keys are the lowercase slug the icon set files the brand under").toMatch(
        /^[a-z0-9][a-z0-9-]*$/,
      );
      // A `d` starts with a moveto; anything else is a truncated or mangled paste.
      expect(mark.path, `${key} is not a path`).toMatch(/^[Mm]/);
      expect(mark.path.length).toBeGreaterThan(10);
      // No second path and no baked-in colour: the mark has to inherit `currentColor`, which is
      // what lets the picker draw it in the neutral ramp instead of a brand hex.
      expect(mark.path).not.toContain("<");
      expect(mark.path).not.toContain("fill");
    }
  });

  it("keeps no mark nobody flies", () => {
    const flown = new Set(icons);
    // Dead path data is dead weight in the client bundle — and a mark for a brand this catalog
    // does not carry is a trademark held for no reason at all.
    expect([...Object.keys(MCP_BRAND_MARKS)].filter((key) => !flown.has(key))).toEqual([]);
  });

  it("resolves nothing for a key it does not hold", () => {
    expect(mcpBrandMark(undefined)).toBeUndefined();
    expect(mcpBrandMark("")).toBeUndefined();
    expect(mcpBrandMark("not-a-brand")).toBeUndefined();
    // Prototype keys are keys like any other here, and must not resolve to a function.
    expect(mcpBrandMark("toString")).toBeUndefined();
  });
});
