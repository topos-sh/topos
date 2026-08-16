import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { asUser, createScratchDb, type ScratchDb, seedUser } from "./helpers/scratch-db";

/**
 * The workspace-creation floors, counted INSIDE `createWorkspaceTx` under the per-person
 * advisory lock (the TOCTOU fix — both doors share the tx body): the OWNED cap
 * (`workspaces-owned`, floored at 3 current owner seats; a present row wins even when lower)
 * and the rolling-day cap (`workspace-create-per-day`, floored at 10). Both refuse TYPED and
 * honest — never `taken` — and leaving a workspace (the owner seat gone) really frees an
 * owned slot.
 */

const seam = vi.hoisted(() => ({
  limits: {} as Record<string, number>,
}));
vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "multi",
    registration: "open",
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: async () => ({
        allows: () => true,
        limit: (key: string) => seam.limits[key] ?? null,
      }),
    },
  },
}));

let db: ScratchDb;

async function wc() {
  return import("@/lib/db/workspace-create.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_wscaps");
  await seedUser(db, "u_maker", "Maker", "maker@example.com");
  await seedUser(db, "u_low", "Low", "low@example.com");
  await seedUser(db, "u_daily", "Daily", "daily@example.com");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the owned-workspaces floor (3)", () => {
  it("creates up to the floor, then refuses `owned-limit`; a freed owner seat frees the slot", async () => {
    const create = (await wc()).createWorkspace;
    for (const name of ["maker-one", "maker-two", "maker-three"]) {
      const born = await create(asUser("u_maker", "Maker"), { name, displayName: name });
      expect(born.outcome).toBe("created");
    }
    const fourth = await create(asUser("u_maker", "Maker"), {
      name: "maker-four",
      displayName: "maker-four",
    });
    expect(fourth.outcome).toBe("owned-limit");
    // The count is CURRENT owner seats: giving one up frees the slot.
    await db.q(
      `DELETE FROM web.seat WHERE user_id = 'u_maker'
       AND workspace_id = (SELECT id FROM web.workspace WHERE name = 'maker-one')`,
    );
    const afterLeave = await create(asUser("u_maker", "Maker"), {
      name: "maker-four",
      displayName: "maker-four",
    });
    expect(afterLeave.outcome).toBe("created");
  });

  it("a present `workspaces-owned` row wins even when LOWER than the floor", async () => {
    seam.limits["workspaces-owned"] = 1;
    try {
      const create = (await wc()).createWorkspace;
      const first = await create(asUser("u_low", "Low"), { name: "low-one", displayName: "one" });
      expect(first.outcome).toBe("created");
      const second = await create(asUser("u_low", "Low"), { name: "low-two", displayName: "two" });
      expect(second.outcome).toBe("owned-limit");
    } finally {
      delete seam.limits["workspaces-owned"];
    }
  });
});

describe("the rolling-day floor, now inside the transaction", () => {
  it("refuses `rate-limited` past the daily cap (audit-counted, so deletes don't reset it)", async () => {
    seam.limits["workspace-create-per-day"] = 2;
    try {
      const create = (await wc()).createWorkspace;
      const one = await create(asUser("u_daily", "Daily"), { name: "daily-a", displayName: "a" });
      const two = await create(asUser("u_daily", "Daily"), { name: "daily-b", displayName: "b" });
      expect([one.outcome, two.outcome]).toEqual(["created", "created"]);
      // Deleting a workspace does NOT reset the day count — the audit trail is append-only.
      await db.q(`DELETE FROM web.workspace WHERE name = 'daily-a'`);
      const third = await create(asUser("u_daily", "Daily"), { name: "daily-c", displayName: "c" });
      expect(third.outcome).toBe("rate-limited");
    } finally {
      delete seam.limits["workspace-create-per-day"];
    }
  });
});
