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
 * The two DECISION arms share that bucket, but only when they MISS: deciding a request already
 * resolved on screen is not a guess and costs nothing, while a loop of guessed codes pays for
 * every miss and meets the same wall.
 *
 * Driven through the REAL action against a scratch Postgres, with its own module graph so the
 * page's per-user belt is this file's alone; each belt case runs as its own seeded person, which
 * is what the bucket is keyed on, so one case never spends another's tokens.
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

/**
 * THE TWO THINGS A PERSON DOES TO THIS FIELD BY ACCIDENT.
 *
 * Clicking "Look up" with nothing typed threw the framework's bare 400, which the root boundary
 * renders as the house fault screen — a page that says neither what went wrong nor what to do.
 * From the person's side the form simply stopped working. It now answers on the form, in the
 * sentence the page already opens with.
 *
 * And the field took any amount of text, so a stray paste — a whole command line, a URL, a log
 * excerpt — went to the lookup as though it might be a code. A code is `XXXX-XXXX` and nothing
 * longer can be one, so anything longer is the ordinary miss, told free: it is not a guess of the
 * code space, so it must not spend the belt that guards it.
 */
describe("the code field refuses what cannot be a code, in the page", () => {
  it("says what to do when the submit carried no code", async () => {
    const empty = await postVerify({ code: "" });
    expect(statusOf(empty)).toBe(400);
    expect(bodyOf(empty)).toEqual({
      kind: "refused",
      error: "Enter the code your terminal shows",
    });
    // Whitespace is nothing typed, whatever the field's own validation makes of it.
    expect(bodyOf(await postVerify({ code: "   " }))).toEqual({
      kind: "refused",
      error: "Enter the code your terminal shows",
    });
  });

  it("answers a paste far longer than a code with the ordinary miss", async () => {
    await seedUser(db, "u_paste", "Paster", "paster@example.com");
    await seatUser(db, wsId, "u_paste", "member");
    session = { user: { id: "u_paste", name: "Paster", email: "paster@example.com" } };

    const pasted = "topos login --workspace acme # AB29-CD34";
    expect(pasted.length).toBeGreaterThan(9);
    // Eleven of them — one more than the belt's whole burst. Every one is the same honest miss,
    // because none of them ever asked the belt for a token.
    for (let i = 0; i < 11; i++) {
      expect(await postVerify({ code: pasted })).toEqual({ kind: "miss" });
    }

    // The belt is untouched, so a real code still resolves on the very next submission.
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("pasted-box", null);
    expect(await postVerify({ code: flow.userCode })).toMatchObject({
      kind: "resolved",
      pending: { requestedName: "pasted-box" },
    });
  });

  it("still looks up a code of exactly the right length", async () => {
    session = { user: { id: "u_own", name: "Owner", email: "owner@example.com" } };
    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("edge-box", null);
    expect(flow.userCode).toHaveLength(9);
    expect(await postVerify({ code: flow.userCode })).toMatchObject({ kind: "resolved" });
    // One character past the shape is not a code, whatever it starts with.
    expect(await postVerify({ code: `${flow.userCode}X` })).toEqual({ kind: "miss" });
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

describe("a guessed DECISION pays per miss; a decision on a resolved code never does", () => {
  it("the eleventh missed approve renders the refusal, and a real approve after it still lands", async () => {
    await seedUser(db, "u_guess", "Guesser", "guesser@example.com");
    await seatUser(db, wsId, "u_guess", "member");
    session = { user: { id: "u_guess", name: "Guesser", email: "guesser@example.com" } };

    const identity = await import("@/lib/db/identity.server");
    const flow = await identity.startLoginFlow("guessed-box", null);

    // Ten approves of codes that resolve NOTHING. Each is the honest gone line — and each pays a
    // token, because a decision that resolves nothing is a guess whatever it calls itself.
    for (let i = 0; i < 10; i++) {
      const missed = await postVerify({
        intent: "approve",
        code: `ZZZZ-00${String(i).padStart(2, "0")}`,
        pick: "seat:",
      });
      expect(statusOf(missed)).toBe(400);
      expect(bodyOf(missed)).toMatchObject({ kind: "refused" });
    }

    // The eleventh meets the wall the lookup meets, in the same words and on the same page.
    const walled = await postVerify({ intent: "approve", code: "ZZZZ-0099", pick: "seat:" });
    expect(statusOf(walled)).toBe(429);
    expect(bodyOf(walled)).toEqual({
      kind: "refused",
      error: "Too many attempts — wait a few seconds and try again",
    });

    // A REAL approve still lands with the bucket empty: a code that resolves never asks the belt.
    expect(
      await postVerify({ intent: "approve", code: flow.userCode, pick: "seat:" }),
    ).toMatchObject({ kind: "approved", name: "guessed-box" });
    expect((await identity.pollLoginFlow(flow.flowCode)).status).toBe("granted");
  });
});
