import type { RouteConfigEntry } from "@react-router/dev/routes";
import { describe, expect, it } from "vitest";
import { ossRoutes } from "@/topos-web/routes";
import { OSS_TOP_LEVEL_SEGMENTS } from "@/topos-web/segments";

/**
 * The route table ↔ reserved-segment lockstep. A workspace NAME slug only occupies the ROOT
 * position in MULTI tenancy (where `/:ws` sits at the top level), so the segments a name must
 * never shadow are exactly the top-level STATIC segments MULTI mode registers — and
 * `OSS_TOP_LEVEL_SEGMENTS` must equal that set. A top-level route added without updating the
 * constant fails here; a constant entry with no route fails too.
 *
 * We derive from BOTH modes to make the intent explicit: SINGLE tenancy is origin-rooted, so its
 * member routes (members/skills/channels/settings/archive) ARE top-level too, but they shadow no
 * name slug (single mints no workspaces by name), so they are deliberately NOT reserved. The
 * secondary check keeps the list honest the other way — every reserved segment that is not
 * multi-only maps to a real single-mode route as well.
 */

/**
 * The top-level STATIC path segments a route tree registers: walk the tree, take the first
 * segment of every ABSOLUTE static path (index/layout nodes carry no segment, so their children
 * stay at the same level), and drop dynamic (`:`) and splat (`*`) segments.
 */
function topLevelStatics(entries: RouteConfigEntry[]): string[] {
  const out = new Set<string>();
  const walk = (list: RouteConfigEntry[], atRoot: boolean): void => {
    for (const entry of list) {
      const hasPath = typeof entry.path === "string" && entry.path.length > 0;
      if (hasPath) {
        if (atRoot) {
          const first = (entry.path as string).split("/")[0] ?? "";
          if (first.length > 0 && !first.startsWith(":") && !first.startsWith("*")) {
            out.add(first);
          }
        }
        if (entry.children) {
          walk(entry.children, false);
        }
      } else if (entry.children) {
        walk(entry.children, atRoot);
      }
    }
  };
  walk(entries, true);
  return [...out].sort();
}

describe("OSS_TOP_LEVEL_SEGMENTS ↔ the route table", () => {
  it("equals every top-level static segment MULTI tenancy registers (the reserved surface)", () => {
    const multi = topLevelStatics(ossRoutes({ tenancy: "multi" }));
    expect(multi).toEqual([...OSS_TOP_LEVEL_SEGMENTS].sort());
  });

  it("reserves no segment that no route uses: each non-multi-only entry is a real single-mode route", () => {
    const single = topLevelStatics(ossRoutes({ tenancy: "single" }));
    const multi = topLevelStatics(ossRoutes({ tenancy: "multi" }));
    // Segments that exist ONLY in multi (e.g. the landing index, `new`) have no single-mode route.
    const multiOnly = new Set(multi.filter((seg) => !single.includes(seg)));
    for (const seg of OSS_TOP_LEVEL_SEGMENTS) {
      if (multiOnly.has(seg)) {
        continue;
      }
      expect(single).toContain(seg);
    }
  });
});
