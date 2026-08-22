import { randomBytes } from "node:crypto";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { EnvelopeCrypto, FileMasterKey } from "../service/crypto";
import { createInternalHandler } from "../service/internal";
import { PgStore } from "../service/store";
import {
  createServiceDb,
  seedConnectedServer,
  seedIdentity,
  type ServiceDb,
} from "./helpers/service-db";

const TOKEN = "internal-test-bearer";
const CONFIG = {
  gatewayPublicUrl: "https://gateway.example.com",
  toposPublicUrl: "https://team.example.com",
};

let db: ServiceDb;
let pool: pg.Pool;
let store: PgStore;
let armed: (request: Request) => Promise<Response>;
let unarmed: (request: Request) => Promise<Response>;

function lane(
  method: string,
  path: string,
  options: { bearer?: string | null; body?: unknown } = {},
): Request {
  const headers = new Headers();
  if (options.bearer !== null) {
    headers.set("authorization", `Bearer ${options.bearer ?? TOKEN}`);
  }
  return new Request(`http://gateway.internal${path}`, {
    method,
    headers,
    ...(options.body === undefined ? {} : { body: JSON.stringify(options.body) }),
  });
}

beforeAll(async () => {
  db = await createServiceDb();
  pool = new pg.Pool({ connectionString: db.gatewayUrl, max: 5 });
  store = new PgStore(pool, new EnvelopeCrypto(new FileMasterKey(randomBytes(32))), CONFIG.toposPublicUrl, () => {});
  await seedIdentity(db, "ws1", "u1");
  const deps = {
    store,
    config: CONFIG,
    guardedFetch: fetch,
    log: () => {},
  };
  armed = createInternalHandler({ ...deps, internalToken: TOKEN });
  unarmed = createInternalHandler({ ...deps, internalToken: undefined });
}, 120_000);

afterAll(async () => {
  await pool?.end();
  await db?.drop();
});

describe("the lane gate", () => {
  it("unarmed: the whole lane is a uniform 404, valid bearer or not", async () => {
    for (const request of [
      lane("POST", "/internal/v1/authorize/begin", { body: {} }),
      lane("POST", "/internal/v1/credentials/manual", { body: {} }),
      lane("DELETE", "/internal/v1/credentials/cred_x"),
      lane("GET", "/internal/v1/anything", { bearer: null }),
    ]) {
      const response = await unarmed(request);
      expect(response.status).toBe(404);
      expect(await response.json()).toEqual({ code: "NOT_FOUND" });
    }
  });

  it("armed: a wrong or missing bearer answers an honest 401", async () => {
    for (const request of [
      lane("DELETE", "/internal/v1/credentials/cred_x", { bearer: "wrong" }),
      lane("DELETE", "/internal/v1/credentials/cred_x", { bearer: null }),
    ]) {
      const response = await armed(request);
      expect(response.status).toBe(401);
      expect(await response.json()).toEqual({ code: "UNAUTHORIZED" });
    }
  });

  it("armed: an unmatched lane path is the uniform 404", async () => {
    const response = await armed(lane("GET", "/internal/v1/no-such-route"));
    expect(response.status).toBe(404);
    expect(await response.json()).toEqual({ code: "NOT_FOUND" });
  });
});

describe("credentials/manual", () => {
  it("stores the secret encrypted and answers the credential id", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "manual", { authMode: "manual" });
    const response = await armed(
      lane("POST", "/internal/v1/credentials/manual", {
        body: {
          workspaceId: "ws1",
          serverId: seeded.serverId,
          userId: "u1",
          secret: "sk-manual-secret",
          createdByDisplay: "Person One",
        },
      }),
    );
    expect(response.status).toBe(200);
    const { credentialId } = (await response.json()) as { credentialId: string };
    expect(credentialId).toMatch(/^cred_[0-9a-f]{32}$/);
    // The ciphertext never carries the secret in the clear…
    const rows = await db.q(
      "SELECT ciphertext FROM gateway.credential_secret WHERE credential_id = $1",
      [credentialId],
    );
    expect((rows[0]?.ciphertext as Buffer).includes(Buffer.from("sk-manual-secret"))).toBe(false);
    // …and the store's decrypt path hands it back whole.
    expect((await store.credentialFor("ws1", seeded.serverId, "u1"))?.secret).toBe(
      "sk-manual-secret",
    );
  });

  it("refuses an unconnected server with the uniform 404", async () => {
    const response = await armed(
      lane("POST", "/internal/v1/credentials/manual", {
        body: {
          workspaceId: "ws1",
          serverId: "msrv_nope",
          userId: null,
          secret: "s",
          createdByDisplay: "x",
        },
      }),
    );
    expect(response.status).toBe(404);
  });

  it("refuses a malformed body typed", async () => {
    const response = await armed(
      lane("POST", "/internal/v1/credentials/manual", { body: { workspaceId: "ws1" } }),
    );
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ code: "BAD_REQUEST" });
  });
});

describe("DELETE credentials/{id}", () => {
  it("deletes and answers 204; a second delete is the 404", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "del");
    const id = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: null,
      authKind: "manual",
      payload: { secret: "bye" },
      createdByDisplay: "owner",
    });
    const first = await armed(lane("DELETE", `/internal/v1/credentials/${id}`));
    expect(first.status).toBe(204);
    const second = await armed(lane("DELETE", `/internal/v1/credentials/${id}`));
    expect(second.status).toBe(404);
  });
});

describe("authorize/begin refusals that need no discovery", () => {
  it("a manual-tier server answers 409 MANUAL_ONLY", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "manonly", { authMode: "manual" });
    const response = await armed(
      lane("POST", "/internal/v1/authorize/begin", {
        body: {
          workspaceId: "ws1",
          serverId: seeded.serverId,
          userId: "u1",
          returnTo: `${CONFIG.toposPublicUrl}/mcp/${seeded.bundleName}`,
        },
      }),
    );
    expect(response.status).toBe(409);
    expect(await response.json()).toEqual({ code: "MANUAL_ONLY" });
  });

  it("an auth-free server answers 409 NO_AUTH_NEEDED", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "noauth", { authMode: "none" });
    const response = await armed(
      lane("POST", "/internal/v1/authorize/begin", {
        body: {
          workspaceId: "ws1",
          serverId: seeded.serverId,
          userId: null,
          returnTo: `${CONFIG.toposPublicUrl}/mcp/${seeded.bundleName}`,
        },
      }),
    );
    expect(response.status).toBe(409);
    expect(await response.json()).toEqual({ code: "NO_AUTH_NEEDED" });
  });

  it("an unconnected server is the uniform 404", async () => {
    const response = await armed(
      lane("POST", "/internal/v1/authorize/begin", {
        body: {
          workspaceId: "ws1",
          serverId: "msrv_ghost",
          userId: null,
          returnTo: `${CONFIG.toposPublicUrl}/mcp/x`,
        },
      }),
    );
    expect(response.status).toBe(404);
  });
});
