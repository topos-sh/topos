import { describe, expect, it } from "vitest";
import { harnessTone } from "@/routes/sessions";

/**
 * The fleet page's per-harness chips. What a row RENDERS is covered end to end by the
 * Playwright suite; what is worth pinning here is the one decision with logic in it — which
 * state word earns which tone.
 *
 * The state vocabulary belongs to the CLIENT and is deliberately open (the report door
 * shape-checks it, it never closes it), so the mapping has to answer for a word this build has
 * never heard: it renders, quietly, rather than being dropped. A fleet page that hides a state
 * it does not recognize is worse than one that shows it plainly.
 */

describe("harness state → chip tone", () => {
  it("names a machine that holds the entry as good", () => {
    expect(harnessTone("current")).toBe("verified");
  });

  it.each(["drifted", "unprovable", "conflicting"])("marks %s as wanting attention", (state) => {
    expect(harnessTone(state)).toBe("pending");
  });

  it("keeps 'this agent does not do MCP' at the quietest step", () => {
    expect(harnessTone("not-supported")).toBe("faint");
  });

  it.each([
    "something-new",
    "",
    "CURRENT",
  ])("still renders an unknown word (%s) rather than dropping it", (state) => {
    expect(harnessTone(state)).toBe("neutral");
  });
});
