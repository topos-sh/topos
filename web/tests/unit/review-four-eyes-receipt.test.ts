import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { action as reviewsAction } from "@/routes/api.v1.reviews";
import {
  createScratchDb,
  linkDevice,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedDevice,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The device-lane write contract: a write 200 carries a receipt, WHATEVER the outcome. A
 * reviewer self-approving their own proposal on a `reviewed` bundle earns the typed
 * FOUR_EYES_REQUIRED denial (before any vault call), and that denial must now ride a DENIED
 * receipt echoing the op_id — otherwise the CLI maps a receipt-less denial to CORRUPT_STATE and
 * its op-WAL wedges (the stored envelope then replays receipt-less forever). Driven through the
 * REAL served action against a scratch Postgres; the seedDevice credential plaintext IS the
 * device id, so `requireDeviceActor` resolves exactly as production does.
 */

const ORIGIN = "http://x";
const OP_ID = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
const CANDIDATE = "ab".repeat(32);

let db: ScratchDb;
let wsId = "";

beforeAll(async () => {
  db = await createScratchDb("web_four_eyes", { TOPOS_WEB_RATELIMIT: "off" });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup(ORIGIN);
  wsId = (await identity.theWorkspace())?.id ?? "";

  await seedUser(db, "u_rev", "Reviewer", "rev@example.com");
  await seatUser(db, wsId, "u_rev", "reviewer");
  await seedDevice(db, "dk_rev", "u_rev", "rev-laptop"); // Bearer plaintext = "dk_rev"
  await linkDevice(db, "dk_rev", wsId);

  // A `reviewed` bundle + an OPEN proposal the reviewer themselves authored: the self-approve.
  await seedBundle(db, wsId, "s_fe", "four-eyes", {
    protection: "reviewed",
    versionId: "cd".repeat(32),
  });
  await db.q(
    `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status)
     VALUES ('p_fe', $1, 's_fe', $2, 'u_rev', 'open')`,
    [wsId, CANDIDATE],
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("device-lane review: an author's self-approve on a reviewed bundle", () => {
  it("answers 200 ok:false FOUR_EYES_REQUIRED, carrying a DENIED receipt that echoes the op_id", async () => {
    const request = new Request(`${ORIGIN}/api/v1/reviews`, {
      method: "POST",
      headers: { authorization: "Bearer dk_rev", "content-type": "application/json" },
      body: JSON.stringify({
        workspace_id: wsId,
        skill_id: "s_fe",
        op_id: OP_ID,
        expected: 1,
        proposal: CANDIDATE,
        decision: "approve",
      }),
    });
    const res = await reviewsAction({
      request,
      params: {},
      context: {},
    } as unknown as Parameters<typeof reviewsAction>[0]);
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      ok: boolean;
      error: { code: string };
      receipt?: { outcome: string; op_id: string };
    };
    expect(body.ok).toBe(false);
    expect(body.error.code).toBe("FOUR_EYES_REQUIRED");
    expect(body.receipt).toBeDefined();
    expect(body.receipt?.outcome).toBe("DENIED");
    expect(body.receipt?.op_id).toBe(OP_ID);

    // The STORED replay envelope carries the receipt too — a same-op_id retry re-serves it
    // verbatim (the wedge is closed at its source).
    const stored = await db.q<{ outcome: { receipt?: { op_id?: string; outcome?: string } } }>(
      `SELECT outcome FROM web.op_receipt WHERE op_id = $1::uuid`,
      [OP_ID],
    );
    expect(stored[0]?.outcome.receipt?.op_id).toBe(OP_ID);
    expect(stored[0]?.outcome.receipt?.outcome).toBe("DENIED");
  });
});
