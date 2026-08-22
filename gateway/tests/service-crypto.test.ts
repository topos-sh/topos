import { mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { afterAll, describe, expect, it } from "vitest";
import {
  EnvelopeCrypto,
  FileMasterKey,
  loadMasterKey,
  type MasterKeyBackend,
} from "../service/crypto";

const dir = mkdtempSync(join(tmpdir(), "gw-crypto-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

function keyFile(name: string, bytes: Buffer, mode = 0o600): string {
  const path = join(dir, name);
  writeFileSync(path, bytes);
  chmodSync(path, mode);
  return path;
}

describe("master key file", () => {
  it("loads exactly 32 raw bytes", () => {
    const path = keyFile("good", randomBytes(32));
    expect(loadMasterKey(path, () => {}).length).toBe(32);
  });

  it("refuses any other size", () => {
    for (const size of [0, 16, 31, 33, 64]) {
      const path = keyFile(`bad-${size}`, randomBytes(size));
      expect(() => loadMasterKey(path, () => {})).toThrow(/32 raw bytes/);
    }
  });

  it("warns on a group/world-readable key file", () => {
    const warnings: string[] = [];
    loadMasterKey(keyFile("loose", randomBytes(32), 0o644), (message) => warnings.push(message));
    expect(warnings.some((m) => m.includes("group/world-readable"))).toBe(true);
    const quiet: string[] = [];
    loadMasterKey(keyFile("tight", randomBytes(32), 0o600), (message) => quiet.push(message));
    expect(quiet).toEqual([]);
  });
});

describe("credential envelope", () => {
  const crypto = new EnvelopeCrypto(new FileMasterKey(randomBytes(32)));

  it("round-trips a payload under its own identity", async () => {
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    const payload = { secret: "tok-abc", refreshToken: "ref-xyz", clientId: "c1" };
    const sealed = await crypto.encryptCredential(payload, "cred_a", "ws1", wrapped);
    const opened = await crypto.decryptCredential(sealed, "cred_a", "ws1", wrapped);
    expect(opened).toEqual(payload);
  });

  it("a ciphertext copied onto another credential id will not decrypt", async () => {
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    const sealed = await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    await expect(crypto.decryptCredential(sealed, "cred_b", "ws1", wrapped)).rejects.toThrow();
  });

  it("a ciphertext copied into another workspace will not decrypt", async () => {
    const wrappedA = await crypto.mintWrappedWorkspaceKey("ws1");
    const sealed = await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrappedA);
    // Same wrapped key bytes presented under another workspace id: the WRAP's AAD refuses
    // before the credential layer is even reached.
    await expect(crypto.decryptCredential(sealed, "cred_a", "ws2", wrappedA)).rejects.toThrow();
  });

  it("a wrapped key does not unwrap under a different master key", async () => {
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    const other = new EnvelopeCrypto(new FileMasterKey(randomBytes(32)));
    await expect(
      other.decryptCredential(
        await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped),
        "cred_a",
        "ws1",
        wrapped,
      ),
    ).rejects.toThrow();
  });

  it("refuses an unrecognized envelope format", async () => {
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    await expect(
      crypto.decryptCredential(Buffer.from([0x02, 1, 2, 3]), "cred_a", "ws1", wrapped),
    ).rejects.toThrow(/format/);
  });
});

/**
 * The 0x01 envelope is FROZEN. These bytes were produced by the file-backed envelope as it stood
 * before the key-backend seam existed — a real witness, generated from that revision of
 * `service/crypto.ts`, not re-derived from the code under test. Every deployment in the field
 * holds rows exactly like them, so a change that stops opening these has broken custody.
 */
describe("the 0x01 envelope stays byte-compatible", () => {
  const FROZEN = {
    master: "4f8b2c1d9e3a7f6058b4c2d1e0f9a8b7c6d5e4f3021a3b4c5d6e7f8091a2b3c4",
    wrapped:
      "01f8319267a7ed4e2fd899a882eff46f5a9650e7854905a718a07b3bb4786710b1c479a5b7add1bf8ba9025ed6ce2749c90de108def1879d865138cf62",
    sealed:
      "0123a50f10d05b819e50e40410ea954093aad95ea98ba44021af5ac099586bc031791a87040fb33ebc2de0f075a06f2c8c9c518c5e5f7a99bd32fb201821c2b5f5a50e35d39b049fe810ac32548a94ef",
    workspaceId: "ws_frozen",
    credentialId: "cred_frozen",
  };
  const frozen = new EnvelopeCrypto(new FileMasterKey(Buffer.from(FROZEN.master, "hex")));

  it("opens a wrapped key and credential written before the seam existed", async () => {
    const opened = await frozen.decryptCredential(
      Buffer.from(FROZEN.sealed, "hex"),
      FROZEN.credentialId,
      FROZEN.workspaceId,
      Buffer.from(FROZEN.wrapped, "hex"),
    );
    expect(opened).toEqual({ secret: "tok-frozen", refreshToken: "ref-frozen" });
  });

  it("still writes that same shape: 0x01, a 12-byte IV, a 32-byte key and its tag", async () => {
    const wrapped = await frozen.mintWrappedWorkspaceKey("ws_frozen");
    expect(wrapped[0]).toBe(0x01);
    expect(wrapped.length).toBe(Buffer.from(FROZEN.wrapped, "hex").length);
    // Freshly written bytes and the frozen ones open under the same key, for the same workspace.
    const sealed = await frozen.encryptCredential({ secret: "s" }, "cred_frozen", "ws_frozen", wrapped);
    expect((await frozen.decryptCredential(sealed, "cred_frozen", "ws_frozen", wrapped)).secret).toBe("s");
  });
});

describe("a backend refuses another backend's wrapped key", () => {
  it("names the KMS backend when a 0x02 row meets a file key", async () => {
    const file = new EnvelopeCrypto(new FileMasterKey(randomBytes(32)));
    const kmsShaped = Buffer.concat([Buffer.from([0x02, 0x01]), randomBytes(48)]);
    await expect(
      file.encryptCredential({ secret: "s" }, "cred_a", "ws1", kmsShaped),
    ).rejects.toThrow(/KMS key backend.*file backend/s);
  });
});

/**
 * The data-key cache. Under a KMS backend an unwrap is a network round trip and every proxied tool
 * call needs one, so these are the properties that keep that from being one RPC per request —
 * without letting a key outlive its window or a migrated row be served from a stale entry.
 */
describe("the data-key cache", () => {
  function counting(inner: MasterKeyBackend): MasterKeyBackend & { unwraps: number } {
    const spy = {
      formatVersion: inner.formatVersion,
      describe: inner.describe,
      unwraps: 0,
      wrapDataKey: (key: Buffer, aad: string) => inner.wrapDataKey(key, aad),
      unwrapDataKey: (envelope: Buffer, aad: string) => {
        spy.unwraps += 1;
        return inner.unwrapDataKey(envelope, aad);
      },
    };
    return spy;
  }

  it("unwraps once for a burst on one workspace, and once more per workspace", async () => {
    const backend = counting(new FileMasterKey(randomBytes(32)));
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    const other = await crypto.mintWrappedWorkspaceKey("ws2");
    await Promise.all(
      Array.from({ length: 8 }, async () =>
        crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped),
      ),
    );
    expect(backend.unwraps).toBe(1);
    await crypto.encryptCredential({ secret: "s" }, "cred_b", "ws2", other);
    expect(backend.unwraps).toBe(2);
  });

  it("re-unwraps once the entry has aged out", async () => {
    const backend = counting(new FileMasterKey(randomBytes(32)));
    let clock = 1_000_000;
    const crypto = new EnvelopeCrypto(backend, () => clock);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    clock += 14 * 60 * 1000;
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    expect(backend.unwraps).toBe(1);
    clock += 2 * 60 * 1000;
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    expect(backend.unwraps).toBe(2);
  });

  it("never serves a re-wrapped row from the key cached for the old bytes", async () => {
    const backend = counting(new FileMasterKey(randomBytes(32)));
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    expect(backend.unwraps).toBe(1);
    // The same workspace, a wrap this backend cannot open — a row migrated to another backend.
    const migrated = Buffer.concat([Buffer.from([0x02, 0x01]), randomBytes(48)]);
    await expect(
      crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", migrated),
    ).rejects.toThrow(/KMS key backend/);
    expect(backend.unwraps).toBe(2);
  });

  it("does not remember a failed unwrap", async () => {
    const inner = new FileMasterKey(randomBytes(32));
    let failNext = true;
    const backend: MasterKeyBackend & { unwraps: number } = {
      formatVersion: inner.formatVersion,
      describe: inner.describe,
      unwraps: 0,
      wrapDataKey: (key: Buffer, aad: string) => inner.wrapDataKey(key, aad),
      unwrapDataKey: async (envelope: Buffer, aad: string) => {
        backend.unwraps += 1;
        if (failNext) {
          failNext = false;
          throw new Error("the key service is unreachable");
        }
        return inner.unwrapDataKey(envelope, aad);
      },
    };
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await inner.wrapDataKey(randomBytes(32), "topos:gateway:workspace-key:ws1");
    await expect(
      crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped),
    ).rejects.toThrow(/unreachable/);
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    expect(backend.unwraps).toBe(2);
  });

  it("keeps the cache bounded, evicting the least recently used", async () => {
    const backend = counting(new FileMasterKey(randomBytes(32)));
    const crypto = new EnvelopeCrypto(backend);
    const first = await crypto.mintWrappedWorkspaceKey("ws-first");
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws-first", first);
    const before = backend.unwraps;
    for (let i = 0; i < 300; i += 1) {
      const wrapped = await crypto.mintWrappedWorkspaceKey(`ws-${i}`);
      await crypto.encryptCredential({ secret: "s" }, "cred_a", `ws-${i}`, wrapped);
    }
    // The first workspace fell out of a bounded cache and has to be unwrapped again.
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws-first", first);
    expect(backend.unwraps).toBe(before + 301);
  });

  it("clears on demand", async () => {
    const backend = counting(new FileMasterKey(randomBytes(32)));
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    crypto.clearDataKeyCache();
    await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    expect(backend.unwraps).toBe(2);
  });
});
