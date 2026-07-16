import { beforeAll, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The door resolver (`/app`). A seated visitor is sent to their workspace under the tenancy
 * grammar; a SEATLESS one diverges by mode: MULTI offers self-serve creation (`/new`), SINGLE has
 * no workspace to make and keeps the house 404. Composition, the auth entry, and the memberships
 * read are mocked so the loader's branching is exercised without a live session or database.
 */

let tenancy: "single" | "multi" = "multi";
vi.mock("@/composition.server", () => ({
  composition: {
    get tenancy() {
      return tenancy;
    },
  },
}));

vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({
    api: {
      getSession: async () => ({ user: { id: "u_1", name: "Ada", email: "ada@example.com" } }),
    },
  }),
}));

let memberships: { id: string; displayName: string; address: string; role: string }[] = [];
vi.mock("@/lib/db/queries.server", () => ({
  membershipsFor: async () => memberships,
}));

let loader: typeof import("@/routes/app-entry").loader;

beforeAll(async () => {
  installTestEnv();
  ({ loader } = await import("@/routes/app-entry"));
});

function call() {
  return loader({
    request: new Request("http://localhost/app"),
  } as Parameters<typeof loader>[0]);
}

describe("the /app door resolver", () => {
  it("multi + a seat: redirects to the workspace's name-slug path", async () => {
    tenancy = "multi";
    memberships = [{ id: "w_1", displayName: "Acme", address: "acme", role: "owner" }];
    const res = await call();
    expect(res.status).toBe(302);
    expect(res.headers.get("Location")).toBe("/acme");
  });

  it("multi + no seat: redirects to self-serve creation (/new)", async () => {
    tenancy = "multi";
    memberships = [];
    const res = await call();
    expect(res.status).toBe(302);
    expect(res.headers.get("Location")).toBe("/new");
  });

  it("single + no seat: the house 404 stands (single-tenant behavior unchanged)", async () => {
    tenancy = "single";
    memberships = [];
    await expect(call()).rejects.toMatchObject({ init: { status: 404 } });
  });
});
