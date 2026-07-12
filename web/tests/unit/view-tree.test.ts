import { describe, expect, it } from "vitest";
import { buildListing, docFileOf, type VersionFileRef } from "@/lib/view/tree";

const file = (path: string, mode = "100644", object_id = `oid-${path}`): VersionFileRef => ({
  path,
  mode,
  object_id,
});

describe("buildListing", () => {
  it("orders directories before files at each level, each sorted lexicographically", () => {
    const listing = buildListing([
      file("z.txt"),
      file("a.txt"),
      file("scripts/run.sh"),
      file("assets/logo.svg"),
    ]);
    expect(listing.map((e) => `${e.kind}:${e.path}`)).toEqual([
      "dir:assets",
      "file:assets/logo.svg",
      "dir:scripts",
      "file:scripts/run.sh",
      "file:a.txt",
      "file:z.txt",
    ]);
  });

  it("emits a pre-order walk with correct depths and synthesized dir rows", () => {
    const listing = buildListing([
      file("a/x.txt"),
      file("a/b/y.txt"),
      file("c.txt"),
      file("z/w.txt"),
    ]);
    expect(listing).toEqual([
      { kind: "dir", name: "a", path: "a", depth: 0 },
      { kind: "dir", name: "b", path: "a/b", depth: 1 },
      {
        kind: "file",
        name: "y.txt",
        path: "a/b/y.txt",
        depth: 2,
        mode: "100644",
        objectId: "oid-a/b/y.txt",
      },
      {
        kind: "file",
        name: "x.txt",
        path: "a/x.txt",
        depth: 1,
        mode: "100644",
        objectId: "oid-a/x.txt",
      },
      { kind: "dir", name: "z", path: "z", depth: 0 },
      {
        kind: "file",
        name: "w.txt",
        path: "z/w.txt",
        depth: 1,
        mode: "100644",
        objectId: "oid-z/w.txt",
      },
      {
        kind: "file",
        name: "c.txt",
        path: "c.txt",
        depth: 0,
        mode: "100644",
        objectId: "oid-c.txt",
      },
    ]);
  });

  it("carries the file mode and object id through", () => {
    const listing = buildListing([file("run.sh", "100755", "abc123")]);
    expect(listing).toEqual([
      {
        kind: "file",
        name: "run.sh",
        path: "run.sh",
        depth: 0,
        mode: "100755",
        objectId: "abc123",
      },
    ]);
  });

  it("returns an empty listing for no files", () => {
    expect(buildListing([])).toEqual([]);
  });
});

describe("docFileOf", () => {
  it("prefers root-level SKILL.md over README.md", () => {
    const doc = docFileOf([file("README.md"), file("SKILL.md")]);
    expect(doc?.path).toBe("SKILL.md");
  });

  it("falls back to root-level README.md when there is no SKILL.md", () => {
    const doc = docFileOf([file("README.md"), file("scripts/run.sh")]);
    expect(doc?.path).toBe("README.md");
  });

  it("matches the basename case-insensitively", () => {
    expect(docFileOf([file("Skill.md")])?.path).toBe("Skill.md");
    expect(docFileOf([file("readme.MD")])?.path).toBe("readme.MD");
  });

  it("only considers root-level files (nested SKILL.md does not count)", () => {
    expect(docFileOf([file("docs/SKILL.md")])).toBeUndefined();
    expect(docFileOf([file("docs/SKILL.md"), file("README.md")])?.path).toBe("README.md");
  });

  it("returns undefined when neither doc exists", () => {
    expect(docFileOf([file("main.ts"), file("run.sh")])).toBeUndefined();
  });
});
