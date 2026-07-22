import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { personDisplay } from "@/lib/person-display";
import {
  asMember,
  bootWorkspace,
  createScratchDb,
  linkDevice,
  type ScratchDb,
  seatUser,
  seedDevice,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The display-name fallback: magic-link sign-ups are born with `name = ''`, and a blank name
 * must never surface as an empty human label — it falls back to the email. Covered at BOTH
 * layers: the pure TS rule (`personDisplay`) and its SQL twin through the real queries (the
 * device-lane actor's raw SQL and a DAL site riding `personDisplaySql`).
 */

describe("personDisplay (the pure rule)", () => {
  it("keeps a real name", () => {
    expect(personDisplay("Ada", "ada@example.com")).toBe("Ada");
  });

  it("falls back to the email on empty, whitespace, null, and undefined names", () => {
    expect(personDisplay("", "ada@example.com")).toBe("ada@example.com");
    expect(personDisplay("   ", "ada@example.com")).toBe("ada@example.com");
    expect(personDisplay(null, "ada@example.com")).toBe("ada@example.com");
    expect(personDisplay(undefined, "ada@example.com")).toBe("ada@example.com");
  });
});

describe("the SQL twin (scratch DB)", () => {
  let db: ScratchDb;
  let ws = "";

  beforeAll(async () => {
    db = await createScratchDb("display_fallback");
    ws = await bootWorkspace();
    // A magic-link-born member: empty name. A named member rides along as the control.
    await seedUser(db, "u_blank", "", "blank@example.com");
    await seedUser(db, "u_named", "Named Person", "named@example.com");
    await seatUser(db, ws, "u_blank", "member");
    await seatUser(db, ws, "u_named", "member");
    await seedDevice(db, "dev_blank", "u_blank");
    await linkDevice(db, "dev_blank", ws);
  }, 60_000);

  afterAll(async () => {
    await db.drop();
  });

  it("the device-lane actor resolve coalesces a blank name to the email", async () => {
    const { deviceActor } = await import("@/lib/db/identity.server");
    // seedDevice derives the credential hash from the device id.
    const row = await deviceActor(ws, "dev_blank");
    expect(row?.userId).toBe("u_blank");
    expect(row?.userDisplay).toBe("blank@example.com");
  });

  it("the roster read coalesces a blank name to the email (personDisplaySql)", async () => {
    const { rosterOf } = await import("@/lib/db/queries.roster.server");
    const roster = await rosterOf(asMember(ws, "u_named"));
    const byId = new Map(roster.map((r) => [r.userId, r.display]));
    expect(byId.get("u_blank")).toBe("blank@example.com");
    expect(byId.get("u_named")).toBe("Named Person");
  });
});
