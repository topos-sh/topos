import { mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { afterAll, describe, expect, it } from "vitest";
import { EnvelopeCrypto, loadMasterKey } from "../service/crypto";

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
  const crypto = new EnvelopeCrypto(randomBytes(32));

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
    const other = new EnvelopeCrypto(randomBytes(32));
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
