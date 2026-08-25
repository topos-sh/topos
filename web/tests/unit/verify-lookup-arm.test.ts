import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * /verify's CODE LOOKUP, reached the way a keyboard reaches it.
 *
 * Pressing Enter in the code field IS clicking "Look up" — the same act, and the page has to
 * answer it the same way. The action used to run the lookup only for a submission carrying the
 * form's hidden `intent=lookup` field, so anything that arrived without it (a form re-submitted
 * by something else on the page carries no submitter; a script or extension between the two can
 * drop a hidden input) fell through to a bare 400: no card, no message, nothing to do next.
 *
 * The lookup is the DEFAULT arm now — `approve` and `deny` are the two acts that decide a
 * request and each names itself; everything else is the lookup. It is also where the guessing
 * belt sits, and where the belt's refusal is SAID: a person who mistypes a code a few times used
 * to be thrown out to the house fault screen ("Something went wrong"), which names neither the
 * cause nor the wait.
 *
 * Driven through the REAL action against a scratch Postgres, with its own module graph so the
 * page's per-user lookup belt is this file's alone.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let wsId = "";

const ORIGIN = "http://x";

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<unknown>;

/** POST the given fields to the real action; a thrown answer comes back as the value. */
async function postVerify(form: Record<string, string>): Promise<unknown> {
  const { action } = await import("@/routes/verify");
  try {
    return await (action as RouteHandler)({
      request: new Request(`${ORIGIN}/verify`, {
        method: "POST",
        headers: { origin: ORIGIN, "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams(form).toString(),
      }),
      params: {},
    });
  } catch (thrown) {
    return thrown;
  }
}

/** The payload of a `data(...)` answer — the actionData the page would render. */
function bodyOf(result: unknown): unknown {
  return typeof result === "object" && result !== null && "data" in result
    ? (result as { data: unknown }).data
    : result;
}

/** The HTTP status a thrown `data(...)`/Response answer carries, wherever it rides. */
function statusOf(result: unknown): number | undefined {
  if (result instanceof Response) {
    return result.status;
  }
  return (result as { init?: { status?: number } }).init?.status;
}

beforeAll(async () => {
  db = await createScratchDb("web_verifylookup", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_own", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_own", "owner");
  session = { user: { id: "u_own", name: "Owner", email: "owner@example.com" } };
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the code lookup is the DEFAULT arm, not a named one", () => {
  it("a submission with no intent field resolves the card, exactly as the button does", async () => {
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("keyboard-box", null);

    expect(await postVerify({ code: flow.userCode })).toMatchObject({
      kind: "resolved",
      pending: { requestedName: "keyboard-box" },
    });
    // The named intent is the same answer — one arm, two spellings.
    expect(await postVerify({ intent: "lookup", code: flow.userCode })).toMatchObject({
      kind: "resolved",
      pending: { requestedName: "keyboard-box" },
    });
    // Resolving decides nothing: the request is still waiting for its approve or deny.
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("pending");
  });

  it("a code nothing pends is the honest in-page miss either way, never a fault", async () => {
    expect(await postVerify({ code: "ZZZZ-9999" })).toEqual({ kind: "miss" });
    expect(await postVerify({ intent: "lookup", code: "ZZZZ-9999" })).toEqual({ kind: "miss" });
  });

  it("still refuses a submission carrying no code at all", async () => {
    expect(statusOf(await postVerify({}))).toBe(400);
    expect(statusOf(await postVerify({ intent: "approve", pick: "seat:" }))).toBe(400);
  });
});

describe("the guessing belt is spent by lookups alone, and says so on the form", () => {
  it("the eleventh lookup answers on the page; an approval after ten still lands", async () => {
    // The belt keys on the acting PERSON, so this case runs as its own — a fresh burst of ten.
    await seedUser(db, "u_belt", "Belted", "belted@example.com");
    await seatUser(db, wsId, "u_belt", "member");
    session = { user: { id: "u_belt", name: "Belted", email: "belted@example.com" } };

    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("belted-box", null);

    // Ten lookups: the burst, all answered.
    for (let i = 0; i < 10; i++) {
      expect(await postVerify({ code: flow.userCode })).toMatchObject({ kind: "resolved" });
    }

    // The eleventh is refused IN THE PAGE — the status is still 429, but what comes back is the
    // form's own line, with the wait in it, instead of the house fault screen.
    const refused = await postVerify({ code: flow.userCode });
    expect(statusOf(refused)).toBe(429);
    expect(bodyOf(refused)).toEqual({
      kind: "refused",
      error: "Too many attempts — wait a few seconds and try again",
    });

    // …and the card the person already has in front of them still decides: an approve of a code
    // already looked up is not a guess, so it never had to queue behind the belt.
    expect(
      await postVerify({ intent: "approve", code: flow.userCode, pick: "seat:" }),
    ).toMatchObject({ kind: "approved", name: "belted-box" });
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
  });
});
