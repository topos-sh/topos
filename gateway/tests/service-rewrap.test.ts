import { randomBytes } from "node:crypto";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { EnvelopeCrypto, FileMasterKey, type MasterKeyBackend } from "../service/crypto";
import { KmsMasterKey } from "../service/kms";
import { GcpKms } from "../service/kms-gcp";
import { planRewrap, rewrapWorkspaceKeys } from "../service/rewrap";
import { PgStore } from "../service/store";
import { createServiceDb, seedConnectedServer, seedIdentity, type ServiceDb } from "./helpers/service-db";
import { fakeGcpKms } from "./helpers/fake-kms";

/**
 * The file → KMS migration against a real database. The credential written under the old backend
 * must still decrypt after the sweep, because the DATA key never changes — only its wrap does.
 */

const KEY_NAME = "projects/topos-test/locations/us-east1/keyRings/gateway/cryptoKeys/master";
const TOPOS_PUBLIC_URL = "https://team.example.com";
const quiet = () => {};

let db: ServiceDb;
let pool: pg.Pool;
let fileBackend: MasterKeyBackend;
let kmsBackend: MasterKeyBackend;

beforeAll(async () => {
  db = await createServiceDb();
  pool = new pg.Pool({ connectionString: db.gatewayUrl, max: 5 });
  fileBackend = new FileMasterKey(randomBytes(32));
  kmsBackend = new KmsMasterKey(
    new GcpKms({
      keyName: KEY_NAME,
      tokens: { describe: "a test token", token: async () => "test-token" },
      fetch: fakeGcpKms({ keyName: KEY_NAME }).fetch,
      sleep: async () => {},
    }),
  );
}, 120_000);

afterAll(async () => {
  await pool?.end();
  await db?.drop();
});

/** One workspace with a stored credential, sealed under whichever backend is given. */
async function seedCredential(
  workspaceId: string,
  backend: MasterKeyBackend,
  secret: string,
): Promise<string> {
  await seedIdentity(db, workspaceId, `u-${workspaceId}`);
  const server = await seedConnectedServer(db, workspaceId, workspaceId.replaceAll("-", ""));
  const store = new PgStore(pool, new EnvelopeCrypto(backend), TOPOS_PUBLIC_URL, quiet);
  await store.storeCredential({
    workspaceId,
    serverId: server.serverId,
    userId: `u-${workspaceId}`,
    authKind: "manual",
    payload: { secret },
    createdByDisplay: "test",
  });
  return server.serverId;
}

async function wrappedKey(workspaceId: string): Promise<Buffer> {
  const rows = await db.q<{ wrapped_key: Buffer }>(
    "SELECT wrapped_key FROM gateway.workspace_key WHERE workspace_id = $1",
    [workspaceId],
  );
  const key = rows[0]?.wrapped_key;
  if (key === undefined) {
    throw new Error("no workspace key");
  }
  return key;
}

describe("re-wrapping workspace keys from the file backend to a KMS", () => {
  it("converts every row, and the credentials still open afterwards", async () => {
    const serverA = await seedCredential("rw-a", fileBackend, "secret-a");
    const serverB = await seedCredential("rw-b", fileBackend, "secret-b");
    expect((await wrappedKey("rw-a"))[0]).toBe(0x01);

    const summary = await rewrapWorkspaceKeys(pool, {
      from: fileBackend,
      to: kmsBackend,
      dryRun: false,
      log: quiet,
    });
    expect(summary).toEqual({ scanned: 2, converted: 2, alreadyThere: 0, failed: 0 });
    expect((await wrappedKey("rw-a"))[0]).toBe(0x02);
    expect((await wrappedKey("rw-b"))[0]).toBe(0x02);

    // The proof that matters: a store on the NEW backend reads the credential written under the
    // old one, byte for byte, because only the wrap moved.
    const after = new PgStore(pool, new EnvelopeCrypto(kmsBackend), TOPOS_PUBLIC_URL, quiet);
    expect((await after.credentialFor("rw-a", serverA, "u-rw-a"))?.secret).toBe("secret-a");
    expect((await after.credentialFor("rw-b", serverB, "u-rw-b"))?.secret).toBe("secret-b");

    // And the old backend can no longer open them — the move is real, not additive.
    const before = new PgStore(pool, new EnvelopeCrypto(fileBackend), TOPOS_PUBLIC_URL, quiet);
    await expect(before.credentialFor("rw-a", serverA, "u-rw-a")).rejects.toThrow(
      /written by a KMS key backend/,
    );
  });

  it("is idempotent — a second run converts nothing and breaks nothing", async () => {
    const server = await seedCredential("rw-idem", fileBackend, "secret-idem");
    const first = await rewrapWorkspaceKeys(pool, {
      from: fileBackend,
      to: kmsBackend,
      dryRun: false,
      log: quiet,
    });
    expect(first.converted).toBeGreaterThanOrEqual(1);
    const wrappedAfterFirst = await wrappedKey("rw-idem");

    const second = await rewrapWorkspaceKeys(pool, {
      from: fileBackend,
      to: kmsBackend,
      dryRun: false,
      log: quiet,
    });
    expect(second.converted).toBe(0);
    expect(second.failed).toBe(0);
    expect(second.alreadyThere).toBe(second.scanned);
    // Untouched, not merely re-converted to something equivalent.
    expect(await wrappedKey("rw-idem")).toEqual(wrappedAfterFirst);
    const after = new PgStore(pool, new EnvelopeCrypto(kmsBackend), TOPOS_PUBLIC_URL, quiet);
    expect((await after.credentialFor("rw-idem", server, "u-rw-idem"))?.secret).toBe("secret-idem");
  });

  it("a dry run proves the whole path and writes nothing", async () => {
    const server = await seedCredential("rw-dry", fileBackend, "secret-dry");
    const before = await wrappedKey("rw-dry");
    const summary = await rewrapWorkspaceKeys(pool, {
      from: fileBackend,
      to: kmsBackend,
      dryRun: true,
      log: quiet,
    });
    expect(summary.converted).toBeGreaterThanOrEqual(1);
    expect(summary.failed).toBe(0);
    expect(await wrappedKey("rw-dry")).toEqual(before);
    // Still readable by the backend that is actually configured.
    const store = new PgStore(pool, new EnvelopeCrypto(fileBackend), TOPOS_PUBLIC_URL, quiet);
    expect((await store.credentialFor("rw-dry", server, "u-rw-dry"))?.secret).toBe("secret-dry");
  });

  it("leaves a row neither backend can open untouched, and reports it", async () => {
    await seedIdentity(db, "rw-alien", "u-rw-alien");
    // A key wrapped under a THIRD master key — a restored-from-elsewhere row, or a lost key file.
    const alien = await new FileMasterKey(randomBytes(32)).wrapDataKey(
      randomBytes(32),
      "topos:gateway:workspace-key:rw-alien",
    );
    await db.q("INSERT INTO gateway.workspace_key (workspace_id, wrapped_key) VALUES ($1, $2)", [
      "rw-alien",
      alien,
    ]);
    const lines: string[] = [];
    const summary = await rewrapWorkspaceKeys(pool, {
      from: fileBackend,
      to: kmsBackend,
      dryRun: false,
      log: (_level, message) => lines.push(message),
    });
    expect(summary.failed).toBe(1);
    expect(lines).toContain("workspace key opens under neither backend; left untouched");
    expect(await wrappedKey("rw-alien")).toEqual(alien);
    // The row has made its point; the next case asserts an exact, clean summary.
    await db.q("DELETE FROM gateway.workspace_key WHERE workspace_id = $1", ["rw-alien"]);
  });

  it("moves back to the file backend the same way", async () => {
    const server = await seedCredential("rw-back", kmsBackend, "secret-back");
    expect((await wrappedKey("rw-back"))[0]).toBe(0x02);
    const summary = await rewrapWorkspaceKeys(pool, {
      from: kmsBackend,
      to: fileBackend,
      dryRun: false,
      log: quiet,
    });
    expect(summary.failed).toBe(0);
    expect((await wrappedKey("rw-back"))[0]).toBe(0x01);
    const store = new PgStore(pool, new EnvelopeCrypto(fileBackend), TOPOS_PUBLIC_URL, quiet);
    expect((await store.credentialFor("rw-back", server, "u-rw-back"))?.secret).toBe("secret-back");
  });
});

describe("what a re-wrap run was asked to do", () => {
  it("reads --from and --dry-run", () => {
    expect(planRewrap(["--from", "file"], "gcp-kms")).toEqual({ from: "file", dryRun: false });
    expect(planRewrap(["--from", "file", "--dry-run"], "gcp-kms")).toEqual({
      from: "file",
      dryRun: true,
    });
    expect(planRewrap(["--dry-run", "--from", "gcp-kms"], "file").from).toBe("gcp-kms");
  });

  it("refuses a run with nothing to move or nothing named", () => {
    expect(() => planRewrap([], "gcp-kms")).toThrow(/--from is required/);
    expect(() => planRewrap(["--from", "azure"], "gcp-kms")).toThrow(/--from must be one of/);
    expect(() => planRewrap(["--from", "file", "--force"], "gcp-kms")).toThrow(
      /unrecognized argument: --force/,
    );
    expect(() => planRewrap(["--from", "file"], "file")).toThrow(/nothing to move/);
  });
});
