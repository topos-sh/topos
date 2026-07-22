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
  deviceApproval: "off" | "on";
}

const pendingDeviceAuthByChallenge = vi.fn<(hex: string) => Promise<PendingView | null>>();
const theWorkspace = vi.fn<() => Promise<WorkspaceRow | null>>();
const workspaceByName = vi.fn<(name: string) => Promise<WorkspaceRow | null>>();
const seatOf = vi.fn<() => Promise<{ role: string } | undefined>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingDeviceAuth: vi.fn(),
  pendingDeviceAuthByChallenge: (hex: string) => pendingDeviceAuthByChallenge(hex),
  approveDeviceAuth: vi.fn(),
  denyDeviceAuth: vi.fn(),
  // The REAL born-status rule, restated (a pure function — mocking it away would unpin the
  // awaits-approval copy from the rule the ceremonies run).
  linkBornStatus: (role: string, knob: string) =>
    role === "owner" ? "active" : knob === "on" ? "pending" : "active",
  // guards.server's imports (unused by the loader under test, present so the mock resolves).
  deviceActor: vi.fn(),
  devicePerson: vi.fn(),
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
  pendingDeviceAuthByChallenge.mockReset().mockResolvedValue(null);
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

describe("the challenge resolution (zero typing)", () => {
  it("a valid device challenge resolves the card with THE ONE workspace being linked", async () => {
    theWorkspace.mockResolvedValue({
      id: "w_1",
      name: "acme",
      displayName: "Acme",
      deviceApproval: "off",
    });
    seatOf.mockResolvedValue({ role: "member" });
    pendingDeviceAuthByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: {
        userCode: string;
        linked: { displayName: string; joining: boolean; awaitsApproval: boolean };
      } | null;
    };
    expect(pendingDeviceAuthByChallenge).toHaveBeenCalledWith(CHALLENGE);
    expect(result.resolved?.userCode).toBe("AB12-CD34");
    expect(result.resolved?.linked).toEqual({
      name: "acme",
      displayName: "Acme",
      joining: false,
      awaitsApproval: false,
    });
  });

  it("the device-approval knob + a non-owner approver forewarn the pending link", async () => {
    theWorkspace.mockResolvedValue({
      id: "w_1",
      name: "acme",
      displayName: "Acme",
      deviceApproval: "on",
    });
    seatOf.mockResolvedValue({ role: "member" });
    pendingDeviceAuthByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { linked: { awaitsApproval: boolean } } | null;
    };
    expect(result.resolved?.linked.awaitsApproval).toBe(true);
    // An OWNER approver's link never waits — the actor is the approval.
    seatOf.mockResolvedValue({ role: "owner" });
    const asOwner = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { linked: { awaitsApproval: boolean } } | null;
    };
    expect(asOwner.resolved?.linked.awaitsApproval).toBe(false);
  });

  it("an invite-carrying flow links the INVITATION's workspace, joining on approve", async () => {
    membershipsFor.mockResolvedValue([{ displayName: "Home", address: "home" }]);
    workspaceByName.mockResolvedValue({
      id: "w_acme",
      name: "acme",
      displayName: "Acme Platform",
      deviceApproval: "off",
    });
    pendingDeviceAuthByChallenge.mockResolvedValue({
      ...FLOW,
      inviteWorkspace: { name: "acme", displayName: "Acme Platform", role: "member" },
    });
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { linked: { displayName: string; joining: boolean } } | null;
    };
    expect(workspaceByName).toHaveBeenCalledWith("acme");
    expect(result.resolved?.linked).toEqual({
      name: "acme",
      displayName: "Acme Platform",
      joining: true,
      awaitsApproval: false,
    });
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
    pendingDeviceAuthByChallenge.mockResolvedValue(FLOW);
    const result = await call(`http://x/verify?device=${CHALLENGE}`);
    expect(result).toMatchObject({ multi: true, device: CHALLENGE });
  });

  it("zero seats, no flow → /new carrying this page as next", async () => {
    expectRedirect(await call("http://x/verify"), "/new?next=%2Fverify");
  });

  it("zero seats + a RESOLVED flow → /new with next AND the slug as a name prefill", async () => {
    pendingDeviceAuthByChallenge.mockResolvedValue(FLOW);
    expectRedirect(
      await call(`http://x/verify?device=${CHALLENGE}`),
      `/new?next=${encodeURIComponent(`/verify?device=${CHALLENGE}`)}&name=acme`,
    );
  });

  it("zero seats + an INVITE-carrying flow stays here — the accept will seat them", async () => {
    pendingDeviceAuthByChallenge.mockResolvedValue({
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
