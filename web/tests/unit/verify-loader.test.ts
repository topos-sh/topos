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

describe("the URL challenge never pre-arms the approve card", () => {
  it("a valid device challenge still yields NO resolved card — the code must be typed", async () => {
    theWorkspace.mockResolvedValue({
      id: "w_1",
      name: "acme",
      displayName: "Acme",
      sessionApproval: "off",
    });
    seatOf.mockResolvedValue({ role: "member" });
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      device: string | null;
      resolved: unknown;
    };
    // `device` is the hex of the flow's device-code hash, so ANYONE who started a flow can
    // compute it — and starting one takes no credential. Handing back a resolved card would let
    // a stranger mail a signed-in member a one-click link and collect a credential acting as
    // that member. The challenge still rides through (the loopback outcome needs it); the card
    // does not.
    expect(result.resolved).toBeNull();
    expect(result.device).toBe(CHALLENGE);
  });

  it("an unknown challenge is indistinguishable from a known one", async () => {
    theWorkspace.mockResolvedValue({
      id: "w_1",
      name: "acme",
      displayName: "Acme",
      sessionApproval: "off",
    });
    seatOf.mockResolvedValue({ role: "member" });
    pendingLoginFlowByChallenge.mockResolvedValue(null);
    const miss = (await call(`http://x/verify?device=${CHALLENGE}`)) as { resolved: unknown };
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    const hit = (await call(`http://x/verify?device=${CHALLENGE}`)) as { resolved: unknown };
    expect(miss.resolved).toBeNull();
    expect(hit.resolved).toBeNull();
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
