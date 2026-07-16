import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The /verify loader's ROUTING decisions, with the composition + data layer mocked: the
 * signed-out bounce keeps the code in `next`, and — MULTI tenancy only — a signed-in person
 * with ZERO seats anywhere is woven through workspace creation (`/new`) carrying this page as
 * `next` and, when a pending flow resolved, its requested slug as a `name` prefill hint. The
 * `/new` target is a sibling route's contract; this suite pins the decision, not the page.
 * Single tenancy never consults memberships — the zero-seat behavior is unchanged there.
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

const pendingDeviceAuth =
  vi.fn<(code: string) => Promise<{ requestedName: string; requestedWorkspace: string } | null>>();
vi.mock("@/lib/db/identity.server", () => ({
  pendingDeviceAuth: (code: string) => pendingDeviceAuth(code),
  approveDeviceAuth: vi.fn(),
  denyDeviceAuth: vi.fn(),
  // guards.server's imports (unused by the loader under test, present so the mock resolves).
  deviceActor: vi.fn(),
  seatOf: vi.fn(),
  theWorkspace: vi.fn(),
  workspaceByName: vi.fn(),
}));

const membershipsFor = vi.fn<() => Promise<unknown[]>>();
vi.mock("@/lib/db/queries.server", () => ({
  membershipsFor: () => membershipsFor(),
}));

let loader: typeof import("@/routes/verify").loader;

beforeAll(async () => {
  installTestEnv();
  ({ loader } = await import("@/routes/verify"));
});

beforeEach(() => {
  tenancy = "single";
  session = { user: { id: "u_1", name: "Person", email: "person@example.com" } };
  pendingDeviceAuth.mockReset().mockResolvedValue(null);
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
  it("carries the page — code included — as the login next path", async () => {
    session = null;
    expectRedirect(
      await call("http://x/verify?code=AB12-CD34"),
      "/login?next=%2Fverify%3Fcode%3DAB12-CD34",
    );
  });
});

describe("single tenancy (unchanged)", () => {
  it("a zero-seat person still gets the page — memberships are never consulted", async () => {
    const result = await call("http://x/verify");
    expect(result).toEqual({ code: "", pending: null, multi: false });
    expect(membershipsFor).not.toHaveBeenCalled();
  });
});

describe("multi tenancy: the workspace-creation weave", () => {
  beforeEach(() => {
    tenancy = "multi";
  });

  it("a seated person gets the page", async () => {
    membershipsFor.mockResolvedValue([{ id: "w_1" }]);
    pendingDeviceAuth.mockResolvedValue({ requestedName: "box", requestedWorkspace: "acme" });
    const result = await call("http://x/verify?code=AB12-CD34");
    expect(result).toEqual({
      code: "AB12-CD34",
      pending: { requestedName: "box", requestedWorkspace: "acme" },
      multi: true,
    });
  });

  it("zero seats, no code → /new carrying this page as next", async () => {
    expectRedirect(await call("http://x/verify"), "/new?next=%2Fverify");
  });

  it("zero seats + a RESOLVED pending flow → /new with next AND the slug as a name prefill", async () => {
    pendingDeviceAuth.mockResolvedValue({ requestedName: "box", requestedWorkspace: "acme" });
    expectRedirect(
      await call("http://x/verify?code=AB12-CD34"),
      "/new?next=%2Fverify%3Fcode%3DAB12-CD34&name=acme",
    );
  });

  it("zero seats + an unresolved code → /new with next only (no prefill from a dead code)", async () => {
    expectRedirect(
      await call("http://x/verify?code=ZZZZ-9999"),
      "/new?next=%2Fverify%3Fcode%3DZZZZ-9999",
    );
  });
});
