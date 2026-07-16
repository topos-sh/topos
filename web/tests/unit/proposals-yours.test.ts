import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { loader as wsProposalsLoader } from "@/routes/api.v1.ws-proposals";
import {
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedDevice,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The review inbox's author-owned split is SERVER-computed by user-id equality — never the
 * display string, whose email-vs-name skew would mislabel the author's own proposal. Driven
 * through the REAL served loader with two device credentials. The custody lane points at an
 * unreachable port on purpose: the index decorates base/message from the vault, but the `yours`
 * flag is pure app-tier, so the vault reads degrade gracefully and the split still answers.
 */

const ORIGIN = "http://x";

let db: ScratchDb;
let wsId = "";

beforeAll(async () => {
  db = await createScratchDb("web_yours", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: "http://127.0.0.1:1",
  });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup(ORIGIN);
  wsId = (await identity.theWorkspace())?.id ?? "";

  await seedUser(db, "u_ana", "Ana", "ana@example.com");
  await seedUser(db, "u_bo", "Bo", "bo@example.com");
  await seatUser(db, wsId, "u_ana", "reviewer");
  await seatUser(db, wsId, "u_bo", "member");
  await seedDevice(db, "dk_ana", "u_ana", "ana-laptop"); // Bearer plaintext = "dk_ana"
  await seedDevice(db, "dk_bo", "u_bo", "bo-laptop");

  await seedBundle(db, wsId, "s_p", "planner");
  // ONE open proposal, authored by Ana.
  await db.q(
    `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status)
     VALUES ('p_ana', $1, 's_p', $2, 'u_ana', 'open')`,
    [wsId, "ab".repeat(32)],
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

async function proposalsFor(cred: string): Promise<{ yours: boolean; proposer: string }[]> {
  const request = new Request(`${ORIGIN}/api/v1/workspaces/${wsId}/proposals`, {
    headers: { authorization: `Bearer ${cred}` },
  });
  const res = await wsProposalsLoader({
    request,
    params: { ws: wsId },
    context: {},
  } as unknown as Parameters<typeof wsProposalsLoader>[0]);
  expect(res.status).toBe(200);
  const body = (await res.json()) as { proposals: { yours: boolean; proposer: string }[] };
  return body.proposals;
}

describe("proposals index: server-computed `yours` (user-id equality, never the display)", () => {
  it("is true for the author's own device and false for another member's", async () => {
    const forAuthor = await proposalsFor("dk_ana");
    expect(forAuthor).toHaveLength(1);
    expect(forAuthor[0]?.yours).toBe(true);

    const forOther = await proposalsFor("dk_bo");
    expect(forOther).toHaveLength(1);
    expect(forOther[0]?.yours).toBe(false);
    // Same proposal, same display for both readers — only `yours` differs.
    expect(forOther[0]?.proposer).toBe(forAuthor[0]?.proposer);
  });
});
