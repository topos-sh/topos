import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { zipStream } from "@/lib/export/zip.server";
import {
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";
import { type UnzipEntry, unzipStore } from "./helpers/unzip";

/**
 * Bundle C — the owner-only workspace export.
 *
 * Two layers: (1) the STORE zip writer round-trips through an independent reader (structure +
 * CRC), and (2) the REAL export loader runs against a REAL scratch Postgres with an in-process
 * stub vault (the ONE transport re-pointed by PLANE_INTERNAL_URL — no mocking of the custody
 * layer, exactly the api-v1-routes.test.ts pattern), proving the authz fences and the archive
 * contents field-for-field. The session is faked at the auth entry (mutable per test); seats are
 * real rows, so requireWorkspaceOwner resolves exactly as production does.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

const ORIGIN = "http://x";
const V_ALPHA = "a1".repeat(32);
const V_BETA = "b2".repeat(32);
const OBJ_ALPHA_SKILL = "aa".repeat(32);
const OBJ_ALPHA_RUN = "ab".repeat(32);
const OBJ_BETA_SKILL = "ba".repeat(32);

const ALPHA_SKILL_BYTES = "# Alpha\nHello alpha\n";
const ALPHA_RUN_BYTES = "#!/bin/sh\necho run\n";
const BETA_SKILL_BYTES = "# Beta\n";

/** `${bundle}/${version_id}` → the version's file listing the stub vault serves. */
const fileListings = new Map<string, { path: string; mode: string; object_id: string }[]>([
  [
    `s_alpha/${V_ALPHA}`,
    [
      { path: "SKILL.md", mode: "100644", object_id: OBJ_ALPHA_SKILL },
      { path: "scripts/run.sh", mode: "100755", object_id: OBJ_ALPHA_RUN },
    ],
  ],
  [`s_beta/${V_BETA}`, [{ path: "SKILL.md", mode: "100644", object_id: OBJ_BETA_SKILL }]],
]);
/** object_id → raw bytes. */
const objects = new Map<string, Buffer>([
  [OBJ_ALPHA_SKILL, Buffer.from(ALPHA_SKILL_BYTES)],
  [OBJ_ALPHA_RUN, Buffer.from(ALPHA_RUN_BYTES)],
  [OBJ_BETA_SKILL, Buffer.from(BETA_SKILL_BYTES)],
]);

let db: ScratchDb;
let wsId = "";
let stub: Server;

async function readAll(stream: ReadableStream<Uint8Array>): Promise<Buffer> {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    chunks.push(value);
  }
  return Buffer.concat(chunks);
}

async function collect(
  entries: { path: string; bytes: Uint8Array; mode?: number }[],
): Promise<Map<string, UnzipEntry>> {
  async function* gen() {
    for (const e of entries) {
      yield e;
    }
  }
  return unzipStore(await readAll(zipStream(gen())));
}

// ── (1) the STORE zip writer, round-tripped ──────────────────────────────────────────────────

describe("zip writer (STORE round-trip)", () => {
  it("writes each entry verbatim under its path, with matching CRC and mode", async () => {
    const binary = new Uint8Array(256);
    for (let i = 0; i < 256; i++) {
      binary[i] = i;
    }
    const map = await collect([
      { path: "manifest.json", bytes: new TextEncoder().encode('{"a":1}') },
      { path: "alpha/SKILL.md", bytes: new TextEncoder().encode("# Alpha\n") },
      { path: "alpha/scripts/run.sh", bytes: binary, mode: 0o100755 },
    ]);
    expect([...map.keys()].sort()).toEqual([
      "alpha/SKILL.md",
      "alpha/scripts/run.sh",
      "manifest.json",
    ]);
    expect(map.get("manifest.json")?.bytes.toString()).toBe('{"a":1}');
    expect(map.get("alpha/SKILL.md")?.bytes.toString()).toBe("# Alpha\n");
    // Binary bytes survive byte-for-byte (STORE, no text mangling).
    expect(Uint8Array.from(map.get("alpha/scripts/run.sh")?.bytes as Buffer)).toEqual(binary);
    // The explicit executable mode is preserved; the default is a regular file.
    expect(map.get("alpha/scripts/run.sh")?.mode).toBe(0o100755);
    expect(map.get("manifest.json")?.mode).toBe(0o100644);
  });

  it("emits a valid empty archive when there are no entries", async () => {
    const map = await collect([]);
    expect(map.size).toBe(0);
  });

  it("normalizes a leading slash out of the archive path", async () => {
    const map = await collect([{ path: "/x/y.txt", bytes: new TextEncoder().encode("z") }]);
    expect([...map.keys()]).toEqual(["x/y.txt"]);
  });

  it("refuses a traversal path — a dot-dot component or a backslash — rather than rewrite it", async () => {
    // A backslash is a legal Unix filename char (one safe component) but a Windows separator on
    // extraction; a dot-dot component is classic zip-slip. Both are refused, not normalized.
    // (The traversal path is built from segments so no literal escapes into committed source.)
    const traversal = ["a", "..", "..", "etc", "passwd"].join("/");
    const backslashed = ["a", "..", "..", "evil"].join("\\");
    await expect(collect([{ path: traversal, bytes: new Uint8Array([1]) }])).rejects.toThrow(
      /traversal/,
    );
    await expect(collect([{ path: backslashed, bytes: new Uint8Array([1]) }])).rejects.toThrow(
      /backslash/,
    );
  });
});

// ── (2) the export route ─────────────────────────────────────────────────────────────────────

/**
 * A guard 404 throws React Router's `data(null, { status })` (a `DataWithResponseInit`), while a
 * signed-out bounce throws a real `redirect()` `Response` — normalize both to a `Response` so the
 * assertions read one way.
 */
function toResponse(e: unknown): Response {
  if (e instanceof Response) {
    return e;
  }
  if (typeof e === "object" && e !== null && "init" in e && "data" in e) {
    return new Response(null, (e as { init?: ResponseInit }).init);
  }
  throw e;
}

async function call(params: Record<string, string> = {}): Promise<Response> {
  const { loader } = await import("@/routes/workspace-export");
  try {
    return await loader({
      request: new Request(`${ORIGIN}/settings/export`),
      params,
      context: {},
    } as unknown as Parameters<typeof loader>[0]);
  } catch (e) {
    return toResponse(e);
  }
}

beforeAll(async () => {
  stub = createServer((request, response) => {
    const url = request.url ?? "";
    const version = url.match(
      /^\/internal\/v1\/workspaces\/[^/]+\/bundles\/([^/]+)\/versions\/([^/]+)$/,
    );
    if (request.method === "GET" && version?.[1] && version[2]) {
      const versionId = version[2];
      const files = fileListings.get(`${version[1]}/${versionId}`);
      if (files) {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(
          JSON.stringify({
            version_id: versionId,
            parents: [],
            author: "seed",
            message: "seed",
            bundle_digest: "d".repeat(64),
            created_at_ms: 1_700_000_000_000,
            files,
          }),
        );
        return;
      }
    }
    const object = url.match(
      /^\/internal\/v1\/workspaces\/[^/]+\/bundles\/[^/]+\/objects\/([^/]+)$/,
    );
    if (request.method === "GET" && object?.[1]) {
      const bytes = objects.get(object[1]);
      if (bytes) {
        response.writeHead(200, {
          "content-type": "application/octet-stream",
          "content-length": String(bytes.length),
        });
        response.end(bytes);
        return;
      }
    }
    response.writeHead(404, { "content-type": "application/json" });
    response.end(JSON.stringify({ code: "NOT_FOUND" }));
  });
  await new Promise<void>((resolve) => stub.listen(0, "127.0.0.1", resolve));
  const port = (stub.address() as AddressInfo).port;

  db = await createScratchDb("web_export", { PLANE_INTERNAL_URL: `http://127.0.0.1:${port}` });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup(ORIGIN);
  wsId = (await identity.theWorkspace())?.id ?? "";

  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_stranger", "Stranger", "stranger@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_mem", "member");

  await seedBundle(db, wsId, "s_alpha", "alpha", { versionId: V_ALPHA });
  await seedBundle(db, wsId, "s_beta", "beta", { versionId: V_BETA });
  // A name with nothing published yet — MUST be absent from the export.
  await seedBundle(db, wsId, "s_gamma", "gamma", { withPointer: false });
}, 60000);

afterAll(async () => {
  await new Promise<void>((resolve, reject) => stub.close((e) => (e ? reject(e) : resolve())));
  await db.drop();
});

describe("authorization (owner-only, 404-not-403)", () => {
  it("a signed-out visitor is bounced to /login (per the member-surface convention)", async () => {
    session = null;
    const res = await call();
    expect(res.status).toBe(302);
    expect(res.headers.get("location")).toBe("/login");
  });

  it("a plain MEMBER gets the uniform 404 — never a 403, never bytes", async () => {
    session = { user: { id: "u_mem", name: "Member", email: "mem@example.com" } };
    const res = await call();
    expect(res.status).toBe(404);
    // No zip headers leak — the miss is indistinguishable from a missing resource.
    expect(res.headers.get("content-type")).not.toBe("application/zip");
  });

  it("a signed-in NON-member gets the same uniform 404", async () => {
    session = { user: { id: "u_stranger", name: "Stranger", email: "stranger@example.com" } };
    const res = await call();
    expect(res.status).toBe(404);
  });
});

describe("the archive (owner)", () => {
  it("streams every published skill at its current version, plus a manifest", async () => {
    session = { user: { id: "u_owner", name: "Owner", email: "owner@example.com" } };
    const res = await call();
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("application/zip");
    expect(res.headers.get("content-disposition")).toBe('attachment; filename="team-skills.zip"');
    expect(res.headers.get("cache-control")).toBe("no-store");

    const map = unzipStore(new Uint8Array(await res.arrayBuffer()));
    expect([...map.keys()].sort()).toEqual([
      "alpha/SKILL.md",
      "alpha/scripts/run.sh",
      "beta/SKILL.md",
      "manifest.json",
    ]);
    // gamma (no current version) never appears.
    expect([...map.keys()].some((k) => k.startsWith("gamma/"))).toBe(false);

    // Bytes are the vault's, verbatim, under `<skill-name>/<path>`.
    expect(map.get("alpha/SKILL.md")?.bytes.toString()).toBe(ALPHA_SKILL_BYTES);
    expect(map.get("alpha/scripts/run.sh")?.bytes.toString()).toBe(ALPHA_RUN_BYTES);
    expect(map.get("beta/SKILL.md")?.bytes.toString()).toBe(BETA_SKILL_BYTES);

    // The git file mode survives: the `100755` script extracts executable, the rest regular.
    expect(map.get("alpha/scripts/run.sh")?.mode).toBe(0o100755);
    expect(map.get("alpha/SKILL.md")?.mode).toBe(0o100644);
    expect(map.get("beta/SKILL.md")?.mode).toBe(0o100644);
    expect(map.get("manifest.json")?.mode).toBe(0o100644);

    const manifest = JSON.parse((map.get("manifest.json") as UnzipEntry).bytes.toString());
    expect(manifest.workspace).toBe("team");
    expect(typeof manifest.generated_at).toBe("string");
    expect(Number.isNaN(Date.parse(manifest.generated_at))).toBe(false);
    expect(manifest.skills).toEqual([
      { name: "alpha", version_id: V_ALPHA },
      { name: "beta", version_id: V_BETA },
    ]);
  });
});

/**
 * Multi tenancy: the SAME route module, mounted under `/:ws`, resolves the workspace by its NAME
 * slug through `workspaceInScope` — an unknown slug (and a missing one) is the uniform 404, never
 * an existence oracle. The composition's tenancy is flipped for the block and restored after.
 */
describe("multi tenancy (the :ws slug grammar)", () => {
  it("resolves by slug for the owner, and 404s an unknown/absent slug", async () => {
    const { composition } = await import("@/composition.server");
    const original = composition.tenancy;
    composition.tenancy = "multi";
    try {
      session = { user: { id: "u_owner", name: "Owner", email: "owner@example.com" } };

      const ok = await call({ ws: "team" });
      expect(ok.status).toBe(200);
      expect(ok.headers.get("content-type")).toBe("application/zip");
      const map = unzipStore(new Uint8Array(await ok.arrayBuffer()));
      expect(map.has("manifest.json")).toBe(true);
      expect(map.has("alpha/SKILL.md")).toBe(true);

      // An unknown slug and an absent one both throw the house 404 before any read.
      expect((await call({ ws: "nope" })).status).toBe(404);
      expect((await call({})).status).toBe(404);
    } finally {
      composition.tenancy = original;
    }
  });
});
