import { describe, expect, it } from "vitest";
import { computeDiffPlan, type PlanFile } from "@/lib/diff/plan";

const oid = (c: string) => c.repeat(64);
const f = (path: string, object: string, mode: "100644" | "100755" = "100644"): PlanFile => ({
  path,
  mode,
  object_id: oid(object),
});

describe("computeDiffPlan", () => {
  it("classifies the full matrix in one plan, sorted by path", () => {
    const base = [
      f("SKILL.md", "1"),
      f("keep.md", "2"),
      f("mode.sh", "3", "100644"),
      f("old-name.md", "4"),
      f("removed.md", "5"),
    ];
    const cand = [
      f("SKILL.md", "a"),
      f("added.md", "b"),
      f("keep.md", "2"),
      f("mode.sh", "3", "100755"),
      f("new-name.md", "4"),
    ];
    const plan = computeDiffPlan(base, cand);
    expect(plan.map((e) => [e.kind, e.path])).toEqual([
      ["modified", "SKILL.md"],
      ["added", "added.md"],
      ["unchanged", "keep.md"],
      ["mode-only", "mode.sh"],
      ["moved", "new-name.md"],
      ["deleted", "removed.md"],
    ]);
    const moved = plan.find((e) => e.kind === "moved");
    expect(moved?.prevPath).toBe("old-name.md");
    expect(moved?.objectIds).toEqual({ old: oid("4"), new: oid("4") });
  });

  it("never fetches unchanged: same path+object+mode is unchanged", () => {
    const plan = computeDiffPlan([f("a", "1")], [f("a", "1")]);
    expect(plan).toEqual([
      {
        kind: "unchanged",
        path: "a",
        modes: { old: "100644", new: "100644" },
        objectIds: { old: oid("1"), new: oid("1") },
      },
    ]);
  });

  it("same path, same object, different mode is mode-only", () => {
    const plan = computeDiffPlan([f("a", "1", "100644")], [f("a", "1", "100755")]);
    expect(plan[0]?.kind).toBe("mode-only");
  });

  it("ambiguous move (two deletions share the object id) falls back to add+delete", () => {
    const plan = computeDiffPlan([f("x.md", "9"), f("y.md", "9")], [f("z.md", "9")]);
    expect(plan.map((e) => [e.kind, e.path])).toEqual([
      ["deleted", "x.md"],
      ["deleted", "y.md"],
      ["added", "z.md"],
    ]);
  });

  it("ambiguous move (two additions share the object id) falls back to add+delete", () => {
    const plan = computeDiffPlan([f("x.md", "9")], [f("p.md", "9"), f("q.md", "9")]);
    expect(plan.map((e) => [e.kind, e.path])).toEqual([
      ["added", "p.md"],
      ["added", "q.md"],
      ["deleted", "x.md"],
    ]);
  });

  it("a byte-identical rename with a mode change is NOT a move", () => {
    const plan = computeDiffPlan([f("x.sh", "9", "100644")], [f("y.sh", "9", "100755")]);
    expect(plan.map((e) => [e.kind, e.path])).toEqual([
      ["deleted", "x.sh"],
      ["added", "y.sh"],
    ]);
  });

  it("pairs multiple distinct one-to-one moves independently", () => {
    const plan = computeDiffPlan(
      [f("a1.md", "1"), f("b1.md", "2")],
      [f("a2.md", "1"), f("b2.md", "2")],
    );
    expect(plan.map((e) => [e.kind, e.path, e.prevPath])).toEqual([
      ["moved", "a2.md", "a1.md"],
      ["moved", "b2.md", "b1.md"],
    ]);
  });

  it("empty to empty is an empty plan", () => {
    expect(computeDiffPlan([], [])).toEqual([]);
  });
});
