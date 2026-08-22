import { readFileSync, statSync } from "node:fs";
import { randomBytes, timingSafeEqual as nodeTimingSafeEqual, createHash } from "node:crypto";

/**
 * Envelope crypto for credential custody — AES-256-GCM throughout, via WebCrypto.
 *
 * Two layers: a per-workspace DATA KEY encrypts credential payloads; a MASTER KEY BACKEND wraps
 * each data key. Every ciphertext's AAD binds it to its row identity — a credential's to
 * (credential id, workspace id), a wrapped key's to its workspace id — so copying bytes between
 * rows yields nothing but a decrypt failure.
 *
 * The leading byte of every envelope says which kind of key opens it:
 *   0x01 — AES-256-GCM under a key THIS PROCESS HOLDS: [0] 0x01 · [1..13] 96-bit IV · [13..]
 *          ciphertext+tag. Every credential ciphertext is this, always (the data key is local by
 *          construction); a data key wrapped by `FileMasterKey` is this too.
 *   0x02 — a data key wrapped by an EXTERNAL KMS (`service/kms.ts` owns that layout).
 * A backend unwraps its own version and refuses the other by name, so a row's bytes — not the
 * deployment's configuration — decide which key must open it.
 *
 * Nothing here logs, and nothing here holds plaintext longer than its caller does.
 */

/** AES-256-GCM under a key this process holds. Its meaning is frozen: rows in the field carry it. */
export const LOCAL_FORMAT_VERSION = 0x01;
const IV_BYTES = 12;
export const KEY_BYTES = 32;

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
  return Buffer.concat([Buffer.from([LOCAL_FORMAT_VERSION]), iv, Buffer.from(sealed)]);
}

async function open(key: CryptoKey, envelope: Buffer, aad: string): Promise<Buffer> {
  if (envelope.length < 1 + IV_BYTES || envelope[0] !== LOCAL_FORMAT_VERSION) {
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

/** The wrap's AAD. Exported because the re-wrap routine seals under the SAME binding. */
export function workspaceKeyAad(workspaceId: string): string {
  return `topos:gateway:workspace-key:${workspaceId}`;
}

/**
 * Where the master key lives. `wrapDataKey`/`unwrapDataKey` are the WHOLE contract: a backend may
 * hold the key in this process (`FileMasterKey`) or never see it at all (`KmsMasterKey`, whose
 * wrap and unwrap are RPCs), and nothing above this interface can tell which.
 *
 * `aad` binds a wrap to its row and MUST reach the underlying primitive as authenticated data —
 * dropping it would let a wrapped key be moved between workspaces, which is the property the two
 * layers exist for. Every backend stamps `formatVersion` as the envelope's first byte and refuses
 * any other version by name; that refusal is what keeps a deployment from silently reading rows
 * its configured key cannot actually open.
 */
export interface MasterKeyBackend {
  /** The envelope version this backend produces AND the only one it will open. */
  readonly formatVersion: number;
  /** Log-safe identity for boot and the re-wrap ledger — never key material. */
  readonly describe: string;
  wrapDataKey(plaintextKey: Buffer, aad: string): Promise<Buffer>;
  unwrapDataKey(envelope: Buffer, aad: string): Promise<Buffer>;
}

const VERSION_BLAME = new Map<number, string>([
  [LOCAL_FORMAT_VERSION, "the file key backend"],
  [0x02, "a KMS key backend"],
]);

/**
 * Fail closed on a wrapped key this backend did not write. The version byte is the row's own
 * statement of what wrote it, so the refusal names both sides: an operator who moved a deployment
 * to a KMS without running the re-wrap gets told exactly that, not a decrypt failure.
 */
export function assertWrapVersion(envelope: Buffer, expected: number, backend: string): void {
  const found = envelope[0];
  if (found === expected) {
    return;
  }
  const wrote =
    found === undefined
      ? "an empty value"
      : (VERSION_BLAME.get(found) ?? `an unrecognized backend (0x${found.toString(16).padStart(2, "0")})`);
  throw new Error(
    `wrapped workspace key was written by ${wrote}; this gateway runs the ${backend} backend ` +
      `(envelope format 0x${expected.toString(16).padStart(2, "0")})`,
  );
}

/**
 * The master key as 32 bytes in this process's memory — today's deployment, unchanged. The wrap
 * is local AES-256-GCM, so a data key wrapped here is a plain 0x01 envelope and every wrapped key
 * already in the field keeps opening.
 */
export class FileMasterKey implements MasterKeyBackend {
  readonly formatVersion = LOCAL_FORMAT_VERSION;
  readonly describe = "file";
  private key: Promise<CryptoKey>;

  constructor(masterKeyBytes: Buffer) {
    this.key = importAesKey(masterKeyBytes);
  }

  async wrapDataKey(plaintextKey: Buffer, aad: string): Promise<Buffer> {
    return seal(await this.key, plaintextKey, aad);
  }

  async unwrapDataKey(envelope: Buffer, aad: string): Promise<Buffer> {
    assertWrapVersion(envelope, this.formatVersion, this.describe);
    return open(await this.key, envelope, aad);
  }
}

/**
 * How long an unwrapped data key stays usable in memory. Fifteen minutes is the trade: an unwrap
 * is a network round trip under a KMS backend and EVERY proxied tool call needs one, so caching is
 * what keeps the gateway's latency the upstream's rather than the key service's — while a window
 * this short still bounds how long a workspace keeps working after its KMS key is disabled or its
 * IAM binding is pulled. Nothing here is a substitute for revocation: revoking a CREDENTIAL is a
 * row delete, which the store sees before any key is consulted.
 */
const DATA_KEY_TTL_MS = 15 * 60 * 1000;
/** A ceiling on resident data keys — the busiest workspaces stay, the rest re-unwrap on demand. */
const DATA_KEY_CACHE_MAX = 256;

export class EnvelopeCrypto {
  /**
   * Unwrapped data keys, keyed by workspace AND by the wrapped bytes they came from. The digest in
   * the key is what makes a re-wrapped row (a migration to another backend) impossible to serve
   * from a stale entry: different bytes, different cache key, a real unwrap through whichever
   * backend this process is configured with.
   *
   * The value is the in-flight PROMISE, so a burst of concurrent calls for one workspace makes one
   * unwrap, not one per call. The CryptoKey it resolves to is non-extractable — the raw key bytes
   * are dropped as soon as WebCrypto has imported them, and nothing can read them back out.
   */
  private dataKeys = new Map<string, { key: Promise<CryptoKey>; expiresAt: number }>();

  constructor(
    private masterKey: MasterKeyBackend,
    private now: () => number = Date.now,
  ) {}

  /** Mint a fresh workspace data key; returns it wrapped for storage. */
  async mintWrappedWorkspaceKey(workspaceId: string): Promise<Buffer> {
    return this.masterKey.wrapDataKey(randomBytes(KEY_BYTES), workspaceKeyAad(workspaceId));
  }

  /** Drop every cached data key — the next call unwraps again. */
  clearDataKeyCache(): void {
    this.dataKeys.clear();
  }

  private async unwrapWorkspaceKey(workspaceId: string, wrapped: Buffer): Promise<CryptoKey> {
    const cacheKey = `${workspaceId}:${sha256Hex(wrapped)}`;
    const standing = this.dataKeys.get(cacheKey);
    if (standing !== undefined) {
      if (this.now() < standing.expiresAt) {
        // Re-insert to move this entry to the end: Map iterates in insertion order, so the
        // eviction below drops the least recently USED rather than the oldest. The expiry is
        // carried over unchanged — the TTL is absolute, so traffic cannot hold a key forever.
        this.dataKeys.delete(cacheKey);
        this.dataKeys.set(cacheKey, standing);
        return standing.key;
      }
      this.dataKeys.delete(cacheKey);
    }
    const key = (async (): Promise<CryptoKey> => {
      const raw = await this.masterKey.unwrapDataKey(wrapped, workspaceKeyAad(workspaceId));
      if (raw.length !== KEY_BYTES) {
        throw new Error("unwrapped workspace key has the wrong size");
      }
      return importAesKey(raw);
    })();
    // A failed unwrap must never be remembered — the next call has to ask the backend again.
    key.catch(() => this.dataKeys.delete(cacheKey));
    // Sweep what has already expired. The TTL is checked on ACCESS, so without this an entry that
    // is never asked for again sits in the map holding a live key long past its window — residency
    // outliving usefulness, which is the opposite of what the TTL is for.
    for (const [k, v] of this.dataKeys) {
      if (this.now() >= v.expiresAt) {
        this.dataKeys.delete(k);
      }
    }
    this.dataKeys.set(cacheKey, { key, expiresAt: this.now() + DATA_KEY_TTL_MS });
    while (this.dataKeys.size > DATA_KEY_CACHE_MAX) {
      const oldest = this.dataKeys.keys().next();
      if (oldest.done === true) {
        break;
      }
      this.dataKeys.delete(oldest.value);
    }
    return key;
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
