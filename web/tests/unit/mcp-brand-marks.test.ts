import { describe, expect, it } from "vitest";
import { MCP_BRAND_MARKS, mcpBrandMark } from "@/lib/mcp/brand-marks";
import { CURATED_MCP_SERVERS, curatedServerRows } from "@/lib/mcp/curated.server";

/**
 * THE MARKS, HELD TO THE PICKER'S ONE PROMISE: every built-in row draws SOMETHING deliberate — the
 * brand's own mark where the vendored set carries it, the app's MCP glyph where it does not — and
 * neither arm can turn into a broken image.
 *
 * The failure this suite exists to make impossible is a row that names a mark nobody vendored:
 * `logo: "canva"` against a set with no Canva in it renders an empty box that reads as a bug. That
 * is a data mistake a future entry makes silently, so it is checked here rather than looked at.
 */

describe("every curated row either resolves a mark or falls back cleanly", () => {
  it.each(
    CURATED_MCP_SERVERS.map((entry) => [entry.title, entry] as const),
  )("%s", (_title, entry) => {
    if (entry.logo === undefined) {
      // The fallback arm, stated: no key, so `McpMark` draws the Plug. Nothing to resolve.
      expect(mcpBrandMark(entry.logo)).toBeUndefined();
      return;
    }
    const mark = mcpBrandMark(entry.logo);
    // The whole point: a named key that resolves to nothing would render an empty 20px box.
    expect(mark, `no vendored mark for logo key "${entry.logo}"`).toBeDefined();
    expect(mark?.brand.length).toBeGreaterThan(0);
  });

  it("carries the same answer onto the rows the loader ships", () => {
    for (const row of curatedServerRows()) {
      // A row with no mark has no `logo` KEY at all — not a key holding undefined — so nothing
      // downstream has to tell the two apart.
      if (!("logo" in row)) {
        continue;
      }
      expect(mcpBrandMark(row.logo)).toBeDefined();
    }
  });

  it("projects each entry's logo, present or absent, onto its row", () => {
    const rows = curatedServerRows();
    for (const [index, entry] of CURATED_MCP_SERVERS.entries()) {
      expect(rows[index]?.logo).toBe(entry.logo);
    }
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
    const flown = new Set(
      CURATED_MCP_SERVERS.flatMap((entry) => (entry.logo === undefined ? [] : [entry.logo])),
    );
    // Dead path data is dead weight in the client bundle — and a mark for a brand this list no
    // longer offers is a trademark carried for no reason at all.
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
