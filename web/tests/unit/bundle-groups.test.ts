import { describe, expect, it } from "vitest";
import { groupByBase } from "@/lib/bundle-base";

/**
 * The kind SPLIT a chooser over the whole catalog renders (app/lib/bundle-base.ts) — the picker on
 * the channel face is the one surface that offers both kinds at once, and a flat list there says
 * nothing about what a name is. Three rules, each a way the split could rot back into a mixed list:
 *
 *  · both kinds present → TWO headed sections, skills first, matching the rail and the dashboard;
 *  · one kind present → ONE section, and NO heading standing over an empty one;
 *  · within a section, the catalog's own order (name-sorted) is untouched.
 */

type Row = { name: string; kind: string };

const SKILL: Row = { name: "pr-describe", kind: "skill" };
const SKILL2: Row = { name: "release-notes", kind: "skill" };
const SERVER: Row = { name: "weather", kind: "mcp" };

describe("groupByBase", () => {
  it("splits a mixed catalog into Skills then MCP servers", () => {
    const groups = groupByBase([SERVER, SKILL, SKILL2]);
    expect(groups.map((g) => g.label)).toEqual(["Skills", "MCP servers"]);
    expect(groups.map((g) => g.base)).toEqual(["skills", "mcp"]);
    expect(groups[0]?.rows).toEqual([SKILL, SKILL2]);
    expect(groups[1]?.rows).toEqual([SERVER]);
  });

  it("drops a kind with no rows — a skills-only catalog gets no MCP heading", () => {
    const groups = groupByBase([SKILL, SKILL2]);
    expect(groups.map((g) => g.label)).toEqual(["Skills"]);
    expect(groups[0]?.rows).toEqual([SKILL, SKILL2]);
  });

  it("drops the other way too — an MCP-only catalog gets no Skills heading", () => {
    const groups = groupByBase([SERVER]);
    expect(groups.map((g) => g.label)).toEqual(["MCP servers"]);
  });

  it("has nothing to head when there is nothing to offer", () => {
    expect(groupByBase([])).toEqual([]);
  });

  it("keeps the incoming order inside a section, and reads anything not 'mcp' as a skill", () => {
    const odd: Row = { name: "zzz", kind: "knowledge" };
    const groups = groupByBase([SKILL2, odd, SKILL]);
    expect(groups.map((g) => g.label)).toEqual(["Skills"]);
    expect(groups[0]?.rows).toEqual([SKILL2, odd, SKILL]);
  });
});
