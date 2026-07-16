import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { asUser, createScratchDb, type ScratchDb, seedUser } from "./helpers/scratch-db";

/**
 * The self-serve create DAL (`workspace-create.server.ts`) against a REAL scratch Postgres, plus
 * the live-availability read and the create form's own loader `?check=` branch. A workspace is
 * born CLAIMED (creator = owner, no claim code) with its default `everyone` channel + an owner
 * seat + an audit row, all in ONE transaction. A RESERVED name and a name-race unique violation
 * return the SAME typed `taken` refusal — indistinguishable, so the reserved list never leaks.
 *
 * The composition is mocked so a reserved-extra can be supplied, and the auth entry is mocked so
 * the create form's loader resolves a signed-in actor without a live Better Auth session.
 */

// The composition's reserved-extra + tenancy — read lazily by the getters (hoist-safe).
const reservedExtra: readonly string[] = ["acme-reserved"];
vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "multi",
    get reservedWorkspaceNames() {
      return reservedExtra;
    },
    // The OSS allow-all shape by default; tests flip the two knobs below.
    entitlements: {
      forWorkspace: () =>
        Promise.resolve({
          allows: () => entitlementAllows,
          limit: () => entitlementPerDay,
        }),
    },
  },
}));
let entitlementAllows = true;
let entitlementPerDay: number | null = null;

// The create form's loader resolves the session through the auth entry — stub a signed-in owner.
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({
    api: {
      getSession: async () => ({
        user: { id: "u_owner", name: "Owner", email: "owner@example.com" },
      }),
    },
  }),
}));

let db: ScratchDb;

async function dal() {
  return import("@/lib/db/workspace-create.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_create");
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_other", "Other", "other@example.com");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("createWorkspace", () => {
  it("mints a claimed workspace with its everyone channel, an owner seat, and an audit row", async () => {
    const { createWorkspace } = await dal();
    const result = await createWorkspace(asUser("u_owner", "Owner"), {
      name: "acme",
      displayName: "Acme Engineering",
    });
    expect(result).toMatchObject({ outcome: "created", name: "acme" });

    const ws = await db.q<{
      id: string;
      display_name: string;
      claimed_at: string | null;
      claim_code_sha256: Buffer | null;
    }>(
      `SELECT id, display_name, claimed_at, claim_code_sha256 FROM web.workspace WHERE name = 'acme'`,
    );
    expect(ws).toHaveLength(1);
    expect(ws[0]?.display_name).toBe("Acme Engineering");
    // Born CLAIMED: claimed_at set, no claim code (the claim-state CHECK ties the two).
    expect(ws[0]?.claimed_at).not.toBeNull();
    expect(ws[0]?.claim_code_sha256).toBeNull();
    const wsId = ws[0]?.id as string;

    const channels = await db.q<{ name: string; is_default: boolean }>(
      `SELECT name, is_default FROM web.channel WHERE workspace_id = $1`,
      [wsId],
    );
    expect(channels).toEqual([{ name: "everyone", is_default: true }]);

    const seats = await db.q<{ user_id: string; role: string }>(
      `SELECT user_id, role FROM web.seat WHERE workspace_id = $1`,
      [wsId],
    );
    expect(seats).toEqual([{ user_id: "u_owner", role: "owner" }]);

    const audit = await db.q<{ kind: string; outcome: string; subject: string }>(
      `SELECT kind, outcome, subject FROM web.audit_event WHERE workspace_id = $1`,
      [wsId],
    );
    expect(audit).toEqual([{ kind: "workspace_created", outcome: "ok", subject: "acme" }]);
  });

  it("refuses a taken name AND a reserved name with the SAME strictly-equal typed refusal", async () => {
    const { createWorkspace } = await dal();
    const owner = asUser("u_owner", "Owner");
    const duplicate = await createWorkspace(owner, { name: "acme", displayName: "Acme Two" });
    // "api" (OSS route segment), "docs" (future-reserve list), "acme-reserved" (composition extra).
    const routeReserved = await createWorkspace(owner, { name: "api", displayName: "Api" });
    const futureReserved = await createWorkspace(owner, { name: "docs", displayName: "Docs" });
    const extraReserved = await createWorkspace(owner, {
      name: "acme-reserved",
      displayName: "Extra",
    });

    expect(duplicate).toEqual({ outcome: "taken" });
    // Every refusal is BYTE-IDENTICAL to the unique-violation one — the caller cannot tell a
    // reserved name from a taken one (the route maps this one outcome to one message).
    expect(routeReserved).toStrictEqual(duplicate);
    expect(futureReserved).toStrictEqual(duplicate);
    expect(extraReserved).toStrictEqual(duplicate);

    // A reserved name never reaches the insert — no workspace row, no channel, no seat. (It
    // still pays the same one indexed name-read a taken name pays, so timing classifies
    // nothing.)
    const leaked = await db.q(
      `SELECT 1 FROM web.workspace WHERE name IN ('api', 'docs', 'acme-reserved')`,
    );
    expect(leaked).toHaveLength(0);
  });

  it("trips the per-person rolling-day floor, counted from the audit trail", async () => {
    const { createWorkspace } = await dal();
    const owner = asUser("u_owner", "Owner");
    // The composition caps at 2/day for the test; u_owner already created one above ("acme").
    entitlementPerDay = 2;
    try {
      const second = await createWorkspace(owner, { name: "floor-two", displayName: "Two" });
      expect(second.outcome).toBe("created");
      const third = await createWorkspace(owner, { name: "floor-three", displayName: "Three" });
      expect(third).toEqual({ outcome: "rate-limited" });
      // The floor is per PERSON — another account still creates.
      const other = await createWorkspace(asUser("u_other", "Other"), {
        name: "floor-other",
        displayName: "Other",
      });
      expect(other.outcome).toBe("created");
    } finally {
      entitlementPerDay = null;
    }
  });

  it("answers `off` when the composition switches self-serve creation off", async () => {
    const { createWorkspace } = await dal();
    entitlementAllows = false;
    try {
      const result = await createWorkspace(asUser("u_owner", "Owner"), {
        name: "switched-off",
        displayName: "Off",
      });
      expect(result).toEqual({ outcome: "off" });
    } finally {
      entitlementAllows = true;
    }
  });

  it("throws (typed, not a CHECK 500) when a caller bypasses shape validation", async () => {
    const { createWorkspace } = await dal();
    await expect(
      createWorkspace(asUser("u_owner", "Owner"), { name: "Bad_Name", displayName: "Bad" }),
    ).rejects.toThrow(/not a valid workspace address slug/);
  });
});

describe("workspaceNameAvailable", () => {
  it("is true for a free valid slug, false for taken, reserved, and malformed", async () => {
    const { workspaceNameAvailable } = await dal();
    // Free + valid.
    expect(await workspaceNameAvailable("totally-free-team")).toBe(true);
    // Taken (created above).
    expect(await workspaceNameAvailable("acme")).toBe(false);
    // Reserved: a route segment, a future-reserve word, and the composition extra all read false.
    expect(await workspaceNameAvailable("api")).toBe(false);
    expect(await workspaceNameAvailable("docs")).toBe(false);
    expect(await workspaceNameAvailable("acme-reserved")).toBe(false);
    // Malformed: bad charset, leading hyphen, over-length.
    expect(await workspaceNameAvailable("Bad_Name")).toBe(false);
    expect(await workspaceNameAvailable("-lead")).toBe(false);
    expect(await workspaceNameAvailable("a".repeat(101))).toBe(false);
  });
});

describe("the create form's availability loader (?check=)", () => {
  it("returns {name, available} where taken and reserved are BOTH unavailable, a free slug is not", async () => {
    const { loader } = await import("@/routes/workspace-new");
    const check = (slug: string) =>
      loader({
        request: new Request(`http://localhost/new?check=${slug}`),
      } as Parameters<typeof loader>[0]);

    expect(await check("acme")).toEqual({ name: "acme", available: false });
    expect(await check("api")).toEqual({ name: "api", available: false });
    expect(await check("brand-new-space")).toEqual({ name: "brand-new-space", available: true });
    expect(await check("Bad_Name")).toEqual({ name: "Bad_Name", available: false });
  });
});
