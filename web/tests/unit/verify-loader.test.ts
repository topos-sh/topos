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
  inviteWorkspace: { name: string; displayName: string } | null;
}

const pendingDeviceAuthByChallenge = vi.fn<(hex: string) => Promise<PendingView | null>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingDeviceAuth: vi.fn(),
  pendingDeviceAuthByChallenge: (hex: string) => pendingDeviceAuthByChallenge(hex),
  approveDeviceAuth: vi.fn(),
  denyDeviceAuth: vi.fn(),
  // guards.server's imports (unused by the loader under test, present so the mock resolves).
  deviceActor: vi.fn(),
  devicePerson: vi.fn(),
  seatOf: vi.fn(),
  theWorkspace: vi.fn(),
  workspaceByName: vi.fn(),
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
  it("a valid device challenge resolves the card, reach included", async () => {
    membershipsFor.mockResolvedValue([{ displayName: "Acme", address: "acme" }]);
    pendingDeviceAuthByChallenge.mockResolvedValue(FLOW);
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { userCode: string; reach: { label: string; joining: boolean }[] } | null;
    };
    expect(pendingDeviceAuthByChallenge).toHaveBeenCalledWith(CHALLENGE);
    expect(result.resolved?.userCode).toBe("AB12-CD34");
    expect(result.resolved?.reach).toEqual([{ label: "Acme", joining: false }]);
  });

  it("an invite-carrying flow adds the joining workspace to the reach", async () => {
    membershipsFor.mockResolvedValue([{ displayName: "Home", address: "home" }]);
    pendingDeviceAuthByChallenge.mockResolvedValue({
      ...FLOW,
      inviteWorkspace: { name: "acme", displayName: "Acme Platform" },
    });
    const result = (await call(`http://x/verify?device=${CHALLENGE}`)) as {
      resolved: { reach: { label: string; joining: boolean }[] } | null;
    };
    expect(result.resolved?.reach).toEqual([
      { label: "Home", joining: false },
      { label: "Acme Platform", joining: true },
    ]);
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
      inviteWorkspace: { name: "acme", displayName: "Acme" },
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
