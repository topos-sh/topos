import { randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * AUTH BEFORE THE BODY on the session lane's write routes — the memory-amplification fix: an
 * unauthenticated caller must be refused BEFORE this tier reads (buffers) any of the request
 * body, publish-sized caps most of all. Proven two ways: the response is the uniform 404, and
 * the request's own body stream was NEVER PULLED. The workspace bind that replaces the old
 * body-keyed resolve is pinned too: a valid credential presenting another workspace's id in
 * the body meets the same uniform 404. The storage quota (`storage-bytes`) rides the same
 * suite — it refuses AFTER auth and before any vault call, so no vault runs here at all.
 */

const seam = vi.hoisted(() => ({
  limits: {} as Record<string, number>,
  /** What the mocked vault stat answers; null = the stat read failed (fail-open). */
  storedBytes: 0 as number | null,
}));
vi.mock("@/lib/plane/storage.server", () => ({
  workspaceStoredBytes: async () => seam.storedBytes,
  storageStats: async () => new Map<string, number>(),
}));
vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "single",
    registration: "gated",
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: async () => ({
        allows: () => true,
        limit: (key: string) => seam.limits[key] ?? null,
      }),
    },
  },
}));

let db: ScratchDb;
let wsId = "";

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response> | Response;

async function drive(
  h: unknown,
  request: Request,
  params: Record<string, string | undefined> = {},
): Promise<Response> {
  try {
    return await (h as RouteHandler)({ request, params });
  } catch (e) {
    if (e instanceof Response) {
      return e;
    }
    throw e;
  }
}

/**
 * A streaming-body request whose consumption is observable: `request.bodyUsed` flips true the
 * moment ANY reader touches `request.body` (which is exactly what `readCappedBody` does), and
 * stays false when the route refused before reading. (The raw source stream cannot be the
 * probe — undici pre-pumps a half-duplex source at CONSTRUCTION time, before any route runs.)
 */
function bodyProbeRequest(path: string, opts: { method?: string; cred?: string } = {}): Request {
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      controller.enqueue(new TextEncoder().encode("{}"));
      controller.close();
    },
  });
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (opts.cred !== undefined) {
    headers.authorization = `Bearer ${opts.cred}`;
  }
  return new Request(`http://x${path}`, {
    method: opts.method ?? "POST",
    headers,
    body: stream,
    // Node's fetch primitives require this for a streaming body.
    duplex: "half",
  } as RequestInit);
}

function publishBody(workspaceId: string, extra: Record<string, unknown> = {}): string {
  return JSON.stringify({
    workspace_id: workspaceId,
    skill_id: "b_probe",
    op_id: randomUUID(),
    expected: 0,
    candidate: {
      files: [{ path: "SKILL.md", mode: "100644", content_base64: "aGVsbG8=" }],
      parents: [],
      author: "Owner",
      message: "publish",
    },
    ...extra,
  });
}

beforeAll(async () => {
  db = await createScratchDb("web_authorder", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seedSession(db, "cred_owner", wsId, "u_owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("unauthenticated writes never buffer a body", () => {
  it("publish: no credential → the uniform 404, the body stream untouched", async () => {
    const { action } = await import("@/routes/api.v1.publish");
    const request = bodyProbeRequest("/api/v1/publish");
    const res = await drive(action, request);
    expect(res.status).toBe(404);
    expect(request.bodyUsed).toBe(false);
  });

  it("propose, reverts, reviews, report, invitations, protection: same order, same silence", async () => {
    const routes: [string, unknown, Record<string, string>, string][] = [
      ["/api/v1/proposals", (await import("@/routes/api.v1.propose")).action, {}, "POST"],
      ["/api/v1/reverts", (await import("@/routes/api.v1.reverts")).action, {}, "POST"],
      ["/api/v1/reviews", (await import("@/routes/api.v1.reviews")).action, {}, "POST"],
      [
        `/api/v1/workspaces/${wsId}/report`,
        (await import("@/routes/api.v1.report")).action,
        { ws: wsId },
        "PUT",
      ],
      [
        `/api/v1/workspaces/${wsId}/invitations`,
        (await import("@/routes/api.v1.invitations")).action,
        { ws: wsId },
        "POST",
      ],
      [
        `/api/v1/workspaces/${wsId}/skills/b_x/protection`,
        (await import("@/routes/api.v1.skill-protection")).action,
        { ws: wsId, skill: "b_x" },
        "PUT",
      ],
      [
        `/api/v1/workspaces/${wsId}/channels/everyone/protection`,
        (await import("@/routes/api.v1.channel-protection")).action,
        { ws: wsId, channel: "everyone" },
        "PUT",
      ],
      [
        `/api/v1/workspaces/${wsId}/notices/ack`,
        (await import("@/routes/api.v1.notices-ack")).action,
        { ws: wsId },
        "POST",
      ],
    ];
    for (const [path, action, params, method] of routes) {
      const request = bodyProbeRequest(path, { method });
      const res = await drive(action, request, params);
      expect(res.status, path).toBe(404);
      expect(request.bodyUsed, path).toBe(false);
    }
  });

  it("an oversized declared Content-Length from an AUTHENTICATED caller still refuses up front", async () => {
    const { action } = await import("@/routes/api.v1.publish");
    const request = new Request("http://x/api/v1/publish", {
      method: "POST",
      headers: {
        authorization: "Bearer cred_owner",
        "content-type": "application/json",
        "content-length": String(200 * 1024 * 1024),
      },
      body: new ReadableStream<Uint8Array>({
        start(controller) {
          controller.close();
        },
      }),
      duplex: "half",
    } as RequestInit);
    const res = await drive(action, request);
    expect(res.status).toBe(400);
  });
});

describe("the workspace bind behind the credential-first resolve", () => {
  it("a live credential naming ANOTHER workspace in the body is the uniform 404", async () => {
    const { action } = await import("@/routes/api.v1.publish");
    const res = await drive(
      action,
      new Request("http://x/api/v1/publish", {
        method: "POST",
        headers: { authorization: "Bearer cred_owner", "content-type": "application/json" },
        body: publishBody("w_someone_elses"),
      }),
    );
    expect(res.status).toBe(404);
  });
});

describe("the storage quota (`storage-bytes`)", () => {
  it("refuses typed after auth, before any vault call (no vault runs in this suite)", async () => {
    const { action } = await import("@/routes/api.v1.publish");
    seam.storedBytes = 90;
    seam.limits["storage-bytes"] = 100;
    try {
      const res = await drive(
        action,
        new Request("http://x/api/v1/publish", {
          method: "POST",
          headers: { authorization: "Bearer cred_owner", "content-type": "application/json" },
          body: publishBody(wsId),
        }),
      );
      expect(res.status).toBe(200);
      const envelope = (await res.json()) as {
        ok: boolean;
        error?: { code: string; context: { message?: string } };
      };
      expect(envelope.ok).toBe(false);
      expect(envelope.error?.code).toBe("STORAGE_LIMIT_REACHED");
      expect(envelope.error?.context.message).toBe("Storage limit reached for this workspace.");
    } finally {
      delete seam.limits["storage-bytes"];
      seam.storedBytes = 0;
    }
  });

  it("a failed stat read ALLOWS (fail-open); an absent limit never reads the stat at all", async () => {
    const { storageQuotaRefusal } = await import("@/lib/api/publish-flow.server");
    const { asSession } = await import("./helpers/scratch-db");
    const actor = asSession(wsId, "u_owner", "cred_owner", "owner");
    // Stat failure under a tight limit: allow (the ingest shares the backend and fails there).
    seam.storedBytes = null;
    seam.limits["storage-bytes"] = 1;
    try {
      expect(await storageQuotaRefusal(actor, randomUUID(), "publish", 10_000)).toBeNull();
      // Over the limit with a healthy stat: the typed refusal.
      seam.storedBytes = 90;
      const refused = await storageQuotaRefusal(actor, randomUUID(), "publish", 11);
      expect(refused).not.toBeNull();
      // Absent limit (the OSS default): a no-op whatever the stat would say.
      delete seam.limits["storage-bytes"];
      expect(await storageQuotaRefusal(actor, randomUUID(), "publish", 1e12)).toBeNull();
    } finally {
      delete seam.limits["storage-bytes"];
      seam.storedBytes = 0;
    }
  });
});
