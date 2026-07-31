import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The /verify loader's ROUTING + CHOOSER decisions, with the composition + data layer mocked:
 * the signed-out bounce carries the page (validated pass-through params only — the CODE never
 * rides a URL, so there is nothing secret to preserve) as the login `next`; a `device`
 * challenge in the URL resolves the card with zero typing (loopback flows only — the SQL
 * fence itself is pinned against a live database in identity-core.test.ts); and the loader
 * ALWAYS serves the chooser data — seats, pending invitations, whether creation is open —
 * because the workspace is chosen HERE now, not at the flow's start. There is no /new weave
 * anymore: a zero-seat person gets the in-page create form or honest guidance, never a
 * redirect. The `?check=` arm answers the create form's availability probe, /new-shaped.
 */

let tenancy: "single" | "multi" = "single";
let session: { user: { id: string; name: string; email: string } } | null = null;
let createSwitch = true;

vi.mock("@/composition.server", () => ({
  composition: {
    get tenancy() {
      return tenancy;
    },
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: () => Promise.resolve({ allows: () => createSwitch, limit: () => null }),
    },
  },
}));

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

interface PendingView {
  requestedName: string;
  userCode: string;
  binding: "device" | "loopback";
  preselect: string | null;
  invite: null;
}

interface SeatChoice {
  workspaceId: string;
  name: string;
  displayName: string;
  role: string;
  awaitsApproval: boolean;
}

const pendingLoginFlowByChallenge =
  vi.fn<(hex: string, viewerId: string) => Promise<PendingView | null>>();
const seatChoicesFor = vi.fn<() => Promise<SeatChoice[]>>();
const pendingInvitationsFor =
  vi.fn<() => Promise<{ invitations: unknown[]; heldUnverified: boolean }>>();
const theWorkspace = vi.fn<() => Promise<{ claimedAt: Date | null } | null>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingLoginFlow: vi.fn(),
  pendingLoginFlowByChallenge: (hex: string, viewerId: string) =>
    pendingLoginFlowByChallenge(hex, viewerId),
  approveLoginFlow: vi.fn(),
  denyLoginFlow: vi.fn(),
  seatChoicesFor: () => seatChoicesFor(),
  pendingInvitationsFor: () => pendingInvitationsFor(),
  // guards.server's imports (unused by the loader under test, present so the mock resolves).
  sessionActor: vi.fn(),
  seatOf: vi.fn(),
  theWorkspace: () => theWorkspace(),
  workspaceByName: vi.fn(),
}));

const workspaceNameAvailable = vi.fn<(name: string) => Promise<boolean>>();
vi.mock("@/lib/db/workspace-create.server", () => ({
  workspaceNameAvailable: (name: string) => workspaceNameAvailable(name),
  createWorkspacePrecheck: vi.fn(),
}));

let loader: typeof import("@/routes/verify").loader;

const CHALLENGE = "a".repeat(64);
const FLOW: PendingView = {
  requestedName: "box",
  userCode: "AB12-CD34",
  binding: "loopback",
  preselect: "acme",
  invite: null,
};
const SEAT: SeatChoice = {
  workspaceId: "w_1",
  name: "acme",
  displayName: "Acme",
  role: "member",
  awaitsApproval: false,
};

beforeAll(async () => {
  installTestEnv();
  ({ loader } = await import("@/routes/verify"));
});

beforeEach(() => {
  tenancy = "single";
  createSwitch = true;
  session = { user: { id: "u_1", name: "Person", email: "person@example.com" } };
  pendingLoginFlowByChallenge.mockReset().mockResolvedValue(null);
  seatChoicesFor.mockReset().mockResolvedValue([]);
  pendingInvitationsFor.mockReset().mockResolvedValue({ invitations: [], heldUnverified: false });
  theWorkspace.mockReset().mockResolvedValue({ claimedAt: new Date() });
  workspaceNameAvailable.mockReset().mockResolvedValue(true);
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

describe("the URL challenge pre-arms the card, viewer-resolved", () => {
  // The real binding gate is SQL (`pendingLoginFlowByChallenge` filters on `binding='loopback'`)
  // and is covered against a live database in identity-core.test.ts — this suite pins that the
  // loader passes the challenge AND the viewer through, and serves the card beside the chooser.
  it("a resolved flow rides through with the chooser and the preselect intact", async () => {
    seatChoicesFor.mockResolvedValue([SEAT]);
    pendingLoginFlowByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: PendingView | null;
      chooser: { seats: SeatChoice[] };
    };
    expect(pendingLoginFlowByChallenge).toHaveBeenCalledWith(CHALLENGE, "u_1");
    expect(result.resolved?.userCode).toBe("AB12-CD34");
    expect(result.resolved?.preselect).toBe("acme");
    expect(result.chooser.seats).toEqual([SEAT]);
  });

  it("an unresolved challenge still serves the page (the typed-code form)", async () => {
    const result = await call(`http://x/verify?device=${CHALLENGE}`);
    expect(result).toMatchObject({ resolved: null });
    expect(result).not.toBeInstanceOf(Response);
  });
});

describe("the chooser data", () => {
  it("single tenancy, one seat: the chooser carries it; create never renders", async () => {
    seatChoicesFor.mockResolvedValue([SEAT]);
    const result = (await call("http://x/verify")) as {
      multi: boolean;
      chooser: { seats: SeatChoice[]; createAllowed: boolean; bootUnclaimed: boolean };
    };
    expect(result.multi).toBe(false);
    expect(result.chooser.seats).toEqual([SEAT]);
    // Creation is a MULTI surface — single tenancy IS its one workspace.
    expect(result.chooser.createAllowed).toBe(false);
    expect(result.chooser.bootUnclaimed).toBe(false);
  });

  it("single tenancy, zero seats: the page still serves — guidance, never a redirect", async () => {
    theWorkspace.mockResolvedValue({ claimedAt: null });
    const result = (await call("http://x/verify")) as {
      chooser: { seats: SeatChoice[]; createAllowed: boolean; bootUnclaimed: boolean };
    };
    expect(result).not.toBeInstanceOf(Response);
    expect(result.chooser.seats).toEqual([]);
    // The boot workspace is unclaimed — the guidance mentions the setup link.
    expect(result.chooser.bootUnclaimed).toBe(true);
  });

  it("multi tenancy: seats + invitations + the create switch ride the chooser", async () => {
    tenancy = "multi";
    seatChoicesFor.mockResolvedValue([SEAT]);
    pendingInvitationsFor.mockResolvedValue({
      invitations: [{ id: "inv_1", workspaceName: "beta", workspaceDisplay: "Beta" }],
      heldUnverified: false,
    });
    const result = (await call("http://x/verify")) as {
      multi: boolean;
      chooser: { seats: SeatChoice[]; invitations: unknown[]; createAllowed: boolean };
    };
    expect(result.multi).toBe(true);
    expect(result.chooser.seats).toEqual([SEAT]);
    expect(result.chooser.invitations).toHaveLength(1);
    expect(result.chooser.createAllowed).toBe(true);
  });

  it("multi tenancy, zero seats: NO /new redirect — the create form is this page's own", async () => {
    tenancy = "multi";
    const result = await call(`http://x/verify?device=${CHALLENGE}`);
    expect(result).not.toBeInstanceOf(Response);
    expect(result).toMatchObject({ multi: true, chooser: { createAllowed: true } });
  });

  it("a composition with creation OFF carries createAllowed=false (guidance instead)", async () => {
    tenancy = "multi";
    createSwitch = false;
    const result = (await call("http://x/verify")) as { chooser: { createAllowed: boolean } };
    expect(result.chooser.createAllowed).toBe(false);
  });

  it("held-unverified invitations surface as the honest flag, never as options", async () => {
    tenancy = "multi";
    pendingInvitationsFor.mockResolvedValue({ invitations: [], heldUnverified: true });
    const result = (await call("http://x/verify")) as {
      chooser: { invitations: unknown[]; heldUnverified: boolean };
    };
    expect(result.chooser.invitations).toEqual([]);
    expect(result.chooser.heldUnverified).toBe(true);
  });
});

describe("the ?check= availability arm", () => {
  it("answers the probe shape — /new's contract, served here for the inline create form", async () => {
    workspaceNameAvailable.mockResolvedValue(false);
    const result = await call("http://x/verify?check=taken-name");
    expect(result).toEqual({ name: "taken-name", available: false });
    expect(workspaceNameAvailable).toHaveBeenCalledWith("taken-name");
  });

  it("still bounces signed-out — the probe is a signed-in surface", async () => {
    session = null;
    const result = await call("http://x/verify?check=any");
    expect(result).toBeInstanceOf(Response);
    expect((result as Response).status).toBe(302);
  });
});
