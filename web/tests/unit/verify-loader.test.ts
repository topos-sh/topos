import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The /verify loader's ROUTING decisions, with the composition + data layer mocked: the
 * signed-out bounce carries the page (validated pass-through params only — the CODE never
 * rides a URL, so there is nothing secret to preserve) as the login `next`; a `device`
 * challenge in the URL resolves the card with zero typing; and — MULTI tenancy only — a
 * signed-in person with ZERO seats anywhere is woven through workspace creation (`/new`)
 * carrying this page as `next` and, when a flow resolved, its requested slug as a `name`
 * prefill hint — UNLESS the flow carries an invitation, whose accept will seat them right
 * here. The `/new` target is a sibling route's contract; this suite pins the decision, not
 * the page.
 */

let tenancy: "single" | "multi" = "single";
let session: { user: { id: string; name: string; email: string } } | null = null;

vi.mock("@/composition.server", () => ({
  composition: {
    get tenancy() {
      return tenancy;
    },
  },
}));

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

interface PendingView {
  requestedName: string;
  requestedWorkspace: string;
  userCode: string;
  inviteWorkspace: { name: string; displayName: string; role: string } | null;
}

interface WorkspaceRow {
  id: string;
  name: string;
  displayName: string;
  sessionApproval: "off" | "on";
}

const pendingLoginFlowByChallenge = vi.fn<(hex: string) => Promise<PendingView | null>>();
const theWorkspace = vi.fn<() => Promise<WorkspaceRow | null>>();
const workspaceByName = vi.fn<(name: string) => Promise<WorkspaceRow | null>>();
const seatOf = vi.fn<() => Promise<{ role: string } | undefined>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingLoginFlow: vi.fn(),
  pendingLoginFlowByChallenge: (hex: string) => pendingLoginFlowByChallenge(hex),
  approveLoginFlow: vi.fn(),
  denyLoginFlow: vi.fn(),
  // The REAL born-status rule, restated (a pure function — mocking it away would unpin the
  // awaits-approval copy from the rule the ceremonies run).
  sessionBornStatus: (role: string, knob: string) =>
    role === "owner" ? "active" : knob === "on" ? "pending" : "active",
  // guards.server's imports (unused by the loader under test, present so the mock resolves).
  sessionActor: vi.fn(),
  seatOf: () => seatOf(),
  theWorkspace: () => theWorkspace(),
  workspaceByName: (name: string) => workspaceByName(name),
}));

const membershipsFor = vi.fn<() => Promise<unknown[]>>();
vi.mock("@/lib/db/queries.server", () => ({
  membershipsFor: () => membershipsFor(),
}));

let loader: typeof import("@/routes/verify").loader;

const CHALLENGE = "a".repeat(64);
const FLOW: PendingView = {
  requestedName: "box",
  requestedWorkspace: "acme",
  userCode: "AB12-CD34",
  inviteWorkspace: null,
};

beforeAll(async () => {
  installTestEnv();
  ({ loader } = await import("@/routes/verify"));
});

beforeEach(() => {
  tenancy = "single";
  session = { user: { id: "u_1", name: "Person", email: "person@example.com" } };
  pendingLoginFlowByChallenge.mockReset().mockResolvedValue(null);
  membershipsFor.mockReset().mockResolvedValue([]);
  theWorkspace.mockReset().mockResolvedValue(null);
  workspaceByName.mockReset().mockResolvedValue(null);
  seatOf.mockReset().mockResolvedValue(undefined);
});

/** Drive the loader; a thrown redirect Response comes back as the result. */
async function call(url: string): Promise<Response | Awaited<ReturnType<typeof loader>>> {
  try {
    return await loader({ request: new Request(url), params: {}, context: {} } as Parameters<
      typeof loader
    >[0]);
  } catch (e) {
    if (e instanceof Response) {
      return e;
    }
    throw e;
  }
}

function expectRedirect(result: unknown, location: string): void {
  expect(result).toBeInstanceOf(Response);
  expect((result as Response).status).toBe(302);
  expect((result as Response).headers.get("location")).toBe(location);
}

describe("the signed-out bounce", () => {
  it("carries the page — validated loopback params included — as the login next path", async () => {
    session = null;
    expectRedirect(
      await call(`http://x/verify?device=${CHALLENGE}&port=4321&state=abcdefgh`),
      `/login?next=${encodeURIComponent(`/verify?device=${CHALLENGE}&port=4321&state=abcdefgh`)}`,
    );
  });

  it("drops malformed pass-through params from the next path", async () => {
    session = null;
    expectRedirect(
      await call("http://x/verify?device=nonsense&port=80&state=!!"),
      "/login?next=%2Fverify",
    );
  });
});

describe("the URL challenge pre-arms the card ONLY for a loopback flow", () => {
  it("a LOOPBACK flow resolves with zero typing, naming THE ONE workspace", async () => {
    theWorkspace.mockResolvedValue({
      id: "w_1",
      name: "acme",
      displayName: "Acme",
      sessionApproval: "off",
    });
    seatOf.mockResolvedValue({ role: "member" });
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: {
        userCode: string;
        linked: { displayName: string; joining: boolean; awaitsApproval: boolean };
      } | null;
    };
    expect(pendingLoginFlowByChallenge).toHaveBeenCalledWith(CHALLENGE);
    expect(result.resolved?.userCode).toBe("AB12-CD34");
    expect(result.resolved?.linked).toEqual({
      name: "acme",
      displayName: "Acme",
      joining: false,
      awaitsApproval: false,
    });
  });

  // The real binding gate is SQL (`pendingLoginFlowByChallenge` filters on `binding='loopback'`)
  // and is covered against a live database in identity-core.test.ts. A version of that assertion
  // lived here and was deleted: it mocked the very function it claimed to test, so it could not
  // fail for the reason it named while making the suite look like it covered the gate.
  //
  // Recorded loss: the challenge USED to disclose nothing about any flow. It is now deliberately
  // an existence oracle for LOOPBACK flows — a signed-in person who guesses one learns a login is
  // pending. That is the price of the pre-armed card, and it is bounded: a device-bound flow
  // still discloses nothing (identity-core.test.ts pins that), and approving a loopback flow
  // hands the credential to the asking machine, not to whoever resolved the card.

  it("the device-approval knob + a non-owner approver forewarn the pending session", async () => {
    theWorkspace.mockResolvedValue({
      id: "w_1",
      name: "acme",
      displayName: "Acme",
      sessionApproval: "on",
    });
    seatOf.mockResolvedValue({ role: "member" });
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { linked: { awaitsApproval: boolean } } | null;
    };
    expect(result.resolved?.linked.awaitsApproval).toBe(true);
    seatOf.mockResolvedValue({ role: "owner" });
    const asOwner = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { linked: { awaitsApproval: boolean } } | null;
    };
    expect(asOwner.resolved?.linked.awaitsApproval).toBe(false);
  });
});

describe("single tenancy", () => {
  it("a zero-seat person still gets the page (the entry form)", async () => {
    const result = await call("http://x/verify");
    expect(result).toMatchObject({ multi: false, device: null, loopback: null, resolved: null });
  });
});

describe("multi tenancy: the workspace-creation weave", () => {
  beforeEach(() => {
    tenancy = "multi";
  });

  it("a seated person gets the page", async () => {
    membershipsFor.mockResolvedValue([{ displayName: "W One", address: "w-one" }]);
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    const result = await call(`http://x/verify?device=${CHALLENGE}`);
    expect(result).toMatchObject({ multi: true, device: CHALLENGE });
  });

  it("zero seats, no flow → /new carrying this page as next", async () => {
    expectRedirect(await call("http://x/verify"), "/new?next=%2Fverify");
  });

  it("zero seats + a RESOLVED flow → /new with next AND the slug as a name prefill", async () => {
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    expectRedirect(
      await call(`http://x/verify?device=${CHALLENGE}`),
      `/new?next=${encodeURIComponent(`/verify?device=${CHALLENGE}`)}&name=acme`,
    );
  });

  it("zero seats + an INVITE-carrying flow stays here — the accept will seat them", async () => {
    pendingLoginFlowByChallenge.mockResolvedValue({
      ...FLOW,
      inviteWorkspace: { name: "acme", displayName: "Acme", role: "member" },
    });
    const result = await call(`http://x/verify?device=${CHALLENGE}`);
    expect(result).toMatchObject({ multi: true });
    expect(result).not.toBeInstanceOf(Response);
  });

  it("zero seats + an unresolved challenge → /new with next only (no prefill)", async () => {
    expectRedirect(
      await call(`http://x/verify?device=${CHALLENGE}`),
      `/new?next=${encodeURIComponent(`/verify?device=${CHALLENGE}`)}`,
    );
  });
});
