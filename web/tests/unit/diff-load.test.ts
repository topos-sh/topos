import { describe, expect, it } from "vitest";
import type { DiffBlobSource } from "@/lib/diff/load.server";
import { loadDiffContents } from "@/lib/diff/load.server";
import { MAX_BLOB_BYTES } from "@/lib/diff/model";
import { computeDiffPlan, type PlanFile } from "@/lib/diff/plan";
import type { PlaneResult } from "@/lib/plane/errors";

const oid = (c: string) => c.repeat(64);
const f = (path: string, object: string, mode: "100644" | "100755" = "100644"): PlanFile => ({
  path,
  mode,
  object_id: oid(object),
});
function source(blobs: Record<string, Uint8Array | "missing" | "huge">): DiffBlobSource {
  return {
    getBundleCapped: (objectId, _max): Promise<PlaneResult<Uint8Array>> => {
      const blob = blobs[objectId];
      if (blob === undefined || blob === "missing") {
        return Promise.resolve({
          ok: false,
          kind: "not_found",
          retryable: false,
          message: "not found",
        });
      }
      if (blob === "huge") {
        return Promise.resolve({
          ok: false,
          kind: "too_large",
          retryable: false,
          message: "this object exceeds the size cap and wasn't fetched",
        });
      }
      return Promise.resolve({ ok: true, data: blob });
    },
  };
}

const text = (s: string) => new TextEncoder().encode(s);

describe("loadDiffContents", () => {
  it("fetches only what a card needs and classifies text/binary/moved/mode-only", async () => {
    const fetched: string[] = [];
    const io = source({
      [oid("1")]: text("old\n"),
      [oid("a")]: text("new\n"),
      [oid("b")]: new Uint8Array([0x00, 0x01, 0x02]),
    });
    const spied: DiffBlobSource = {
      getBundleCapped: (objectId, max) => {
        fetched.push(objectId);
        return io.getBundleCapped(objectId, max);
      },
    };
    const plan = computeDiffPlan(
      [f("SKILL.md", "1"), f("same.md", "2"), f("mode.sh", "3"), f("from.md", "4")],
      [
        f("SKILL.md", "a"),
        f("same.md", "2"),
        f("mode.sh", "3", "100755"),
        f("to.md", "4"),
        f("bin.dat", "b"),
      ],
    );
    const models = await loadDiffContents(plan, spied);
    // unchanged (same.md), mode-only, and moved never fetch
    expect(fetched.sort()).toEqual([oid("1"), oid("a"), oid("b")].sort());
    const byPath = new Map(models.map((m) => [m.entry.path, m]));
    expect(byPath.get("SKILL.md")?.presentation).toBe("text");
    expect(byPath.get("SKILL.md")?.oldText).toBe("old\n");
    expect(byPath.get("SKILL.md")?.newText).toBe("new\n");
    expect(byPath.get("bin.dat")?.presentation).toBe("binary");
    expect(byPath.get("bin.dat")?.oldText).toBeUndefined();
    expect(byPath.get("to.md")?.presentation).toBe("text");
    expect(byPath.get("to.md")?.entry.kind).toBe("moved");
    expect(byPath.get("mode.sh")?.entry.kind).toBe("mode-only");
    expect(byPath.has("same.md")).toBe(false);
  });

  it("a too_large blob becomes a too-large card with the blob-cap reason", async () => {
    const plan = computeDiffPlan([], [f("big.dat", "9")]);
    const models = await loadDiffContents(plan, source({ [oid("9")]: "huge" }));
    expect(models[0]?.presentation).toBe("too-large");
    expect(models[0]?.reason).toBe("blob-cap");
  });

  it("a failed blob fetch degrades that one card only", async () => {
    const plan = computeDiffPlan([], [f("ok.md", "1"), f("gone.md", "2")]);
    const models = await loadDiffContents(plan, source({ [oid("1")]: text("fine\n") }));
    const byPath = new Map(models.map((m) => [m.entry.path, m]));
    expect(byPath.get("ok.md")?.presentation).toBe("text");
    expect(byPath.get("gone.md")?.presentation).toBe("fetch-failed");
  });

  it("passes the per-blob cap through to the source", async () => {
    let sawMax = 0;
    const io: DiffBlobSource = {
      getBundleCapped: (_objectId, max) => {
        sawMax = max;
        return Promise.resolve({ ok: true, data: text("x") });
      },
    };
    await loadDiffContents(computeDiffPlan([], [f("a.md", "1")]), io);
    expect(sawMax).toBe(MAX_BLOB_BYTES);
  });

  it("entries past the file-count cap become honest overflow entries", async () => {
    const cand: PlanFile[] = [];
    for (let i = 0; i < 105; i++) {
      cand.push({
        path: `f${String(i).padStart(3, "0")}.md`,
        mode: "100644",
        object_id: `${String(i).padStart(4, "0")}${"0".repeat(60)}`,
      });
    }
    const io: DiffBlobSource = {
      getBundleCapped: () => Promise.resolve({ ok: true, data: text("hi\n") }),
    };
    const models = await loadDiffContents(computeDiffPlan([], cand), io);
    const overflowed = models.filter((m) => m.reason === "file-count");
    expect(overflowed).toHaveLength(5);
    expect(models.filter((m) => m.presentation === "text")).toHaveLength(100);
  });

  it("moved and mode-only entries do NOT consume the file-count budget", async () => {
    // 60 byte-identical moves (rename only) + 60 real modifications. The moves must not push any
    // of the 60 modifications past the 100-card budget: fewer than 100 diffs actually render.
    const base: PlanFile[] = [];
    const cand: PlanFile[] = [];
    for (let i = 0; i < 60; i++) {
      const oid60 = `${String(i).padStart(4, "0")}${"a".repeat(60)}`;
      base.push({ path: `moved-from-${i}.md`, mode: "100644", object_id: oid60 });
      cand.push({ path: `moved-to-${i}.md`, mode: "100644", object_id: oid60 });
    }
    for (let i = 0; i < 60; i++) {
      const oldOid = `${String(i).padStart(4, "0")}${"b".repeat(60)}`;
      const newOid = `${String(i).padStart(4, "0")}${"c".repeat(60)}`;
      base.push({
        path: `mod-${String(i).padStart(2, "0")}.md`,
        mode: "100644",
        object_id: oldOid,
      });
      cand.push({
        path: `mod-${String(i).padStart(2, "0")}.md`,
        mode: "100644",
        object_id: newOid,
      });
    }
    const io: DiffBlobSource = {
      getBundleCapped: () => Promise.resolve({ ok: true, data: text("x\n") }),
    };
    const models = await loadDiffContents(computeDiffPlan(base, cand), io);
    // All 60 modifications render (60 < the 100-card budget); none forced to file-count overflow.
    expect(models.filter((m) => m.reason === "file-count")).toHaveLength(0);
    expect(models.filter((m) => m.entry.kind === "moved")).toHaveLength(60);
  });

  it("enforces the whole-page byte budget once fetched blobs exceed it", async () => {
    // 20 modifications, each side ~1 MiB — well past the 8 MiB whole-page budget. Once the running
    // total crosses the cap, later entries must become page-cap cards.
    const big = new Uint8Array(1024 * 1024); // 1 MiB, all-zero → classifies binary, still counts
    const base: PlanFile[] = [];
    const cand: PlanFile[] = [];
    const blobs: Record<string, Uint8Array> = {};
    for (let i = 0; i < 20; i++) {
      const oldOid = `${String(i).padStart(4, "0")}${"d".repeat(60)}`;
      const newOid = `${String(i).padStart(4, "0")}${"e".repeat(60)}`;
      base.push({
        path: `big-${String(i).padStart(2, "0")}.dat`,
        mode: "100644",
        object_id: oldOid,
      });
      cand.push({
        path: `big-${String(i).padStart(2, "0")}.dat`,
        mode: "100644",
        object_id: newOid,
      });
      blobs[oldOid] = big;
      blobs[newOid] = big;
    }
    const models = await loadDiffContents(computeDiffPlan(base, cand), source(blobs));
    const capped = models.filter((m) => m.reason === "page-cap");
    expect(capped.length).toBeGreaterThan(0);
    // And the cap actually bounds work: far fewer than all 20 entries got fetched+rendered.
    expect(models.filter((m) => m.presentation === "binary").length).toBeLessThan(20);
  });
});
