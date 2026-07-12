import { describe, expect, it } from "vitest";
import { type HistoryFetcher, type HistoryMetaLike, walkHistory } from "@/lib/plane/history.server";

function id(n: number): string {
  return String(n).padStart(64, "0");
}

function meta(overrides: Partial<HistoryMetaLike> & { version_id: string }): HistoryMetaLike {
  return {
    parents: [],
    author: "dev-1",
    message: `msg ${overrides.version_id.slice(-4)}`,
    files: [{}, {}],
    ...overrides,
  };
}

function fetcherFor(metas: HistoryMetaLike[]): HistoryFetcher {
  const byId = new Map(metas.map((m) => [m.version_id, m]));
  return async (versionId) => {
    const found = byId.get(versionId);
    return found ? { ok: true, data: found } : { ok: false };
  };
}

describe("walkHistory", () => {
  it("walks a linear chain to genesis", async () => {
    const fetcher = fetcherFor([
      meta({ version_id: id(3), parents: [id(2)] }),
      meta({ version_id: id(2), parents: [id(1)] }),
      meta({ version_id: id(1), parents: [] }),
    ]);
    const page = await walkHistory(fetcher, id(3), { depth: 10 });
    expect(page.steps.map((s) => s.versionId)).toEqual([id(3), id(2), id(1)]);
    expect(page.cursor).toBeNull();
    expect(page.truncated).toBe(false);
    expect(page.steps[0]).toMatchObject({ author: "dev-1", fileCount: 2 });
  });

  it("follows the FIRST parent of a merge and keeps the full parent set on the step", async () => {
    const fetcher = fetcherFor([
      meta({ version_id: id(9), parents: [id(2), id(8)] }),
      meta({ version_id: id(2), parents: [id(1)] }),
      meta({ version_id: id(1), parents: [] }),
    ]);
    const page = await walkHistory(fetcher, id(9), { depth: 10 });
    expect(page.steps.map((s) => s.versionId)).toEqual([id(9), id(2), id(1)]);
    expect(page.steps[0]?.parents).toEqual([id(2), id(8)]);
  });

  it("stops at the depth cap and returns the next id as the cursor", async () => {
    const fetcher = fetcherFor([
      meta({ version_id: id(4), parents: [id(3)] }),
      meta({ version_id: id(3), parents: [id(2)] }),
      meta({ version_id: id(2), parents: [id(1)] }),
      meta({ version_id: id(1), parents: [] }),
    ]);
    const page = await walkHistory(fetcher, id(4), { depth: 2 });
    expect(page.steps.map((s) => s.versionId)).toEqual([id(4), id(3)]);
    expect(page.cursor).toBe(id(2));
    expect(page.truncated).toBe(false);
  });

  it("resumes from a cursor", async () => {
    const fetcher = fetcherFor([
      meta({ version_id: id(2), parents: [id(1)] }),
      meta({ version_id: id(1), parents: [] }),
    ]);
    const page = await walkHistory(fetcher, id(4), { depth: 5, from: id(2) });
    expect(page.steps.map((s) => s.versionId)).toEqual([id(2), id(1)]);
    expect(page.cursor).toBeNull();
  });

  it("guards against cycles: stops and reports truncation instead of looping", async () => {
    const fetcher = fetcherFor([
      meta({ version_id: id(2), parents: [id(1)] }),
      meta({ version_id: id(1), parents: [id(2)] }), // corrupt: 1 → 2 → 1
    ]);
    const page = await walkHistory(fetcher, id(2), { depth: 100 });
    expect(page.steps.map((s) => s.versionId)).toEqual([id(2), id(1)]);
    expect(page.truncated).toBe(true);
    expect(page.cursor).toBeNull();
  });

  it("returns steps-so-far with truncated:true on a mid-walk fetch failure", async () => {
    const fetcher = fetcherFor([
      meta({ version_id: id(3), parents: [id(2)] }),
      meta({ version_id: id(2), parents: [id(1)] }),
      // id(1) missing: the fetch fails there
    ]);
    const page = await walkHistory(fetcher, id(3), { depth: 10 });
    expect(page.steps.map((s) => s.versionId)).toEqual([id(3), id(2)]);
    expect(page.truncated).toBe(true);
    expect(page.cursor).toBeNull();
  });

  it("a failure on the very first fetch yields an empty, truncated page", async () => {
    const page = await walkHistory(async () => ({ ok: false }), id(1), { depth: 10 });
    expect(page).toEqual({ steps: [], cursor: null, truncated: true });
  });

  it("clamps a non-positive depth to 1", async () => {
    const fetcher = fetcherFor([meta({ version_id: id(2), parents: [id(1)] })]);
    const page = await walkHistory(fetcher, id(2), { depth: 0 });
    expect(page.steps).toHaveLength(1);
    expect(page.cursor).toBe(id(1));
  });
});
