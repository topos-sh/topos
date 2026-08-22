import { readFileSync, statSync } from "node:fs";
import { randomBytes, timingSafeEqual as nodeTimingSafeEqual, createHash } from "node:crypto";

/**
 * Envelope crypto for credential custody — AES-256-GCM throughout, via WebCrypto.
 *
 * Two layers: a per-workspace DATA KEY encrypts credential payloads; the MASTER KEY (increment 1
 * backend: a 32-byte file at GATEWAY_MASTER_KEY_FILE) wraps each data key. Every ciphertext's AAD
 * binds it to its row identity — a credential's to (credential id, workspace id), a wrapped key's
 * to its workspace id — so copying bytes between rows yields nothing but a decrypt failure.
 *
 * The envelope byte layout is versioned for a later KMS backend:
 *   [0] format version (0x01) · [1..13] 96-bit IV · [13..] GCM ciphertext+tag.
 *
 * Nothing here logs, and nothing here holds plaintext longer than its caller does.
 */

const FORMAT_VERSION = 0x01;
const IV_BYTES = 12;
const KEY_BYTES = 32;

/** The decrypted payload one credential row protects. */
export interface CredentialPayload {
  secret: string;
  refreshToken?: string;
  tokenEndpoint?: string;
  clientId?: string;
}

export function sha256Hex(input: string | Buffer): string {
  return createHash("sha256").update(input).digest("hex");
}

/** Constant-time equality over equal-length digests; unequal lengths answer false, not a throw. */
export function digestsEqual(a: Buffer, b: Buffer): boolean {
  if (a.length !== b.length) {
    return false;
  }
  return nodeTimingSafeEqual(a, b);
}

/**
 * Read the master key file: exactly 32 raw bytes or refuse to boot. A group/world-readable key
 * file is warned about, not refused — self-host bind mounts land with surprising modes and a
 * refusal here would brick a deployment the operator can still fix live.
 */
export function loadMasterKey(
  path: string,
  warn: (message: string, fields?: Record<string, string | number>) => void,
): Buffer {
  const stat = statSync(path);
  if ((stat.mode & 0o077) !== 0) {
    warn("master key file is group/world-readable; chmod it 0600", {
      mode: (stat.mode & 0o777).toString(8),
    });
  }
  const bytes = readFileSync(path);
  if (bytes.length !== KEY_BYTES) {
    throw new Error(
      `GATEWAY_MASTER_KEY_FILE must hold exactly ${KEY_BYTES} raw bytes (found ${bytes.length})`,
    );
  }
  return bytes;
}

async function importAesKey(raw: Buffer): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", new Uint8Array(raw), { name: "AES-GCM" }, false, [
    "encrypt",
    "decrypt",
  ]);
}

async function seal(key: CryptoKey, plaintext: Buffer, aad: string): Promise<Buffer> {
  const iv = randomBytes(IV_BYTES);
  const sealed = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: new Uint8Array(iv), additionalData: new TextEncoder().encode(aad) },
    key,
    new Uint8Array(plaintext),
  );
  return Buffer.concat([Buffer.from([FORMAT_VERSION]), iv, Buffer.from(sealed)]);
}

async function open(key: CryptoKey, envelope: Buffer, aad: string): Promise<Buffer> {
  if (envelope.length < 1 + IV_BYTES || envelope[0] !== FORMAT_VERSION) {
    throw new Error("unrecognized envelope format");
  }
  const iv = envelope.subarray(1, 1 + IV_BYTES);
  const body = envelope.subarray(1 + IV_BYTES);
  const opened = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: new Uint8Array(iv), additionalData: new TextEncoder().encode(aad) },
    key,
    new Uint8Array(body),
  );
  return Buffer.from(opened);
}

/** The AAD strings — one spelling each, here and nowhere else. */
function credentialAad(credentialId: string, workspaceId: string): string {
  return `topos:gateway:credential:${credentialId}:${workspaceId}`;
}

function workspaceKeyAad(workspaceId: string): string {
  return `topos:gateway:workspace-key:${workspaceId}`;
}

export class EnvelopeCrypto {
  private masterKey: Promise<CryptoKey>;

  constructor(masterKeyBytes: Buffer) {
    this.masterKey = importAesKey(masterKeyBytes);
  }

  /** Mint a fresh workspace data key; returns it wrapped for storage. */
  async mintWrappedWorkspaceKey(workspaceId: string): Promise<Buffer> {
    return seal(await this.masterKey, randomBytes(KEY_BYTES), workspaceKeyAad(workspaceId));
  }

  private async unwrapWorkspaceKey(workspaceId: string, wrapped: Buffer): Promise<CryptoKey> {
    const raw = await open(await this.masterKey, wrapped, workspaceKeyAad(workspaceId));
    if (raw.length !== KEY_BYTES) {
      throw new Error("unwrapped workspace key has the wrong size");
    }
    return importAesKey(raw);
  }

  async encryptCredential(
    payload: CredentialPayload,
    credentialId: string,
    workspaceId: string,
    wrappedWorkspaceKey: Buffer,
  ): Promise<Buffer> {
    const key = await this.unwrapWorkspaceKey(workspaceId, wrappedWorkspaceKey);
    const plaintext = Buffer.from(JSON.stringify(payload), "utf8");
    return seal(key, plaintext, credentialAad(credentialId, workspaceId));
  }

  async decryptCredential(
    ciphertext: Buffer,
    credentialId: string,
    workspaceId: string,
    wrappedWorkspaceKey: Buffer,
  ): Promise<CredentialPayload> {
    const key = await this.unwrapWorkspaceKey(workspaceId, wrappedWorkspaceKey);
    const plaintext = await open(key, ciphertext, credentialAad(credentialId, workspaceId));
    const parsed: unknown = JSON.parse(plaintext.toString("utf8"));
    if (
      typeof parsed !== "object" ||
      parsed === null ||
      typeof (parsed as { secret?: unknown }).secret !== "string"
    ) {
      throw new Error("credential payload has an unrecognized shape");
    }
    return parsed as unknown as CredentialPayload;
  }
}
