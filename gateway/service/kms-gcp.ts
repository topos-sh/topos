import type { AccessTokenSource } from "./gcp-auth";
import {
  assertAad,
  PROVIDER_TAG,
  RetryableKmsError,
  withKmsRetry,
  type KmsProvider,
} from "./kms";

/**
 * Google Cloud KMS over its REST API with plain `fetch` — no vendor SDK.
 *
 * WHY NO SDK: two calls (`cryptoKeys.encrypt`, `cryptoKeys.decrypt`) against one host, with a
 * bearer token this service already knows how to mint. `@google-cloud/kms` brings gRPC, protobuf
 * loading, and a dependency tree measured in hundreds of packages into an image whose entire
 * runtime dependency list is `pg` and `zod` — and into a service whose OTHER transport is a
 * hand-written SSRF-guarded fetch precisely so nothing dials on its own. The REST surface here is
 * ~100 lines and every byte on the wire is visible in this file.
 *
 * INTEGRITY: Cloud KMS accepts a CRC32C of each field and reports whether it verified it. Both are
 * sent and both are checked, and the ciphertext's own checksum is recomputed on the way back — a
 * corrupted request that the service did not verify is retried, never accepted. The response's
 * `name` is compared to the configured key, so a misrouted answer is refused rather than stored.
 *
 * AAD: `additionalAuthenticatedData` is the row binding. Cloud KMS authenticates it exactly as the
 * local AES-GCM layer does, which is why moving to this backend loses no property.
 *
 * Both calls name the CRYPTO KEY, never a version: encrypt uses the key's primary version, decrypt
 * reads the version out of the ciphertext — which is what lets a rotated key still open old rows.
 */

/**
 * The global endpoint serves keys in every location — a key's own region is part of its resource
 * name, not of the host. (Regional endpoints exist for data-residency rules; adding one would be a
 * second variable, and nothing here needs it yet.)
 */
const ENDPOINT = "https://cloudkms.googleapis.com/v1";
/**
 * The HSM protection level caps plaintext+AAD at 8 KiB (software keys allow 64 KiB). A data key
 * plus its AAD is under a hundred bytes, so hold everything to the smaller ceiling rather than
 * discover the difference from a production 400.
 */
const MAX_PAYLOAD_BYTES = 8 * 1024;

export interface GcpKmsOptions {
  /** `projects/{p}/locations/{l}/keyRings/{r}/cryptoKeys/{k}` — shape-checked at env parse. */
  keyName: string;
  tokens: AccessTokenSource;
  fetch?: typeof fetch;
  sleep?: (ms: number) => Promise<void>;
}

/**
 * CRC32C (Castagnoli, reflected polynomial 0x82F63B78) — the checksum Cloud KMS speaks. Computed
 * a bit at a time with no lookup table: the payloads here are a 32-byte key and a short AAD, so
 * the table would cost more to build than the loop costs to run.
 */
export function crc32c(bytes: Buffer): number {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc = (crc ^ byte) >>> 0;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = ((crc & 1) !== 0 ? (crc >>> 1) ^ 0x82f63b78 : crc >>> 1) >>> 0;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/** Cloud KMS carries checksums as int64, which ProtoJSON spells as a decimal STRING. */
function checksum(bytes: Buffer): string {
  return crc32c(bytes).toString();
}

function checksumMatches(bytes: Buffer, reported: unknown): boolean {
  if (typeof reported !== "string" && typeof reported !== "number") {
    // A response with no checksum at all is not proof of corruption; the field is optional.
    return true;
  }
  return String(reported) === checksum(bytes);
}

/** 429 and 5xx are the transport having a bad minute; 4xx is an answer. */
function classify(status: number, detail: string): Error {
  if (status === 429 || status >= 500) {
    return new RetryableKmsError(`Cloud KMS is unavailable (${status}): ${detail}`);
  }
  return new Error(`Cloud KMS refused the request (${status}): ${detail}`);
}

function errorDetail(text: string): string {
  try {
    const parsed = JSON.parse(text) as { error?: { message?: unknown; status?: unknown } };
    const message = parsed.error?.message;
    const status = parsed.error?.status;
    if (typeof message === "string") {
      return typeof status === "string" ? `${status}: ${message}` : message;
    }
  } catch {
    // Not the documented envelope — fall through to the raw text.
  }
  return text.slice(0, 300);
}

export class GcpKms implements KmsProvider {
  readonly name = "gcp-kms";
  readonly tag = PROVIDER_TAG.gcp;
  private fetchImpl: typeof fetch;
  private sleep: (ms: number) => Promise<void>;

  constructor(private options: GcpKmsOptions) {
    this.fetchImpl = options.fetch ?? fetch;
    this.sleep = options.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
  }

  get describe(): string {
    return `${this.options.keyName} (${this.options.tokens.describe})`;
  }

  async encrypt(plaintext: Buffer, aad: string): Promise<Buffer> {
    const aadBytes = Buffer.from(assertAad(aad), "utf8");
    this.assertSize(plaintext, aadBytes);
    return withKmsRetry(async () => {
      const body = await this.call("encrypt", {
        plaintext: plaintext.toString("base64"),
        plaintextCrc32c: checksum(plaintext),
        additionalAuthenticatedData: aadBytes.toString("base64"),
        additionalAuthenticatedDataCrc32c: checksum(aadBytes),
      });
      // A false flag means the checksum never reached the service, so the request went
      // unverified — the documented answer is to discard the response and try again.
      if (body.verifiedPlaintextCrc32c !== true) {
        throw new RetryableKmsError("Cloud KMS did not verify the plaintext checksum");
      }
      if (body.verifiedAdditionalAuthenticatedDataCrc32c !== true) {
        throw new RetryableKmsError("Cloud KMS did not verify the AAD checksum");
      }
      const version = body.name;
      if (typeof version !== "string" || !version.startsWith(`${this.options.keyName}/`)) {
        throw new Error(
          `Cloud KMS answered for key ${String(version)}, not the configured ${this.options.keyName}`,
        );
      }
      if (typeof body.ciphertext !== "string") {
        throw new Error("Cloud KMS returned no ciphertext");
      }
      const ciphertext = Buffer.from(body.ciphertext, "base64");
      if (!checksumMatches(ciphertext, body.ciphertextCrc32c)) {
        throw new RetryableKmsError("Cloud KMS ciphertext failed its checksum in transit");
      }
      return ciphertext;
    }, this.sleep);
  }

  async decrypt(ciphertext: Buffer, aad: string): Promise<Buffer> {
    const aadBytes = Buffer.from(assertAad(aad), "utf8");
    return withKmsRetry(async () => {
      const body = await this.call("decrypt", {
        ciphertext: ciphertext.toString("base64"),
        ciphertextCrc32c: checksum(ciphertext),
        additionalAuthenticatedData: aadBytes.toString("base64"),
        additionalAuthenticatedDataCrc32c: checksum(aadBytes),
      });
      if (typeof body.plaintext !== "string") {
        throw new Error("Cloud KMS returned no plaintext");
      }
      const plaintext = Buffer.from(body.plaintext, "base64");
      if (!checksumMatches(plaintext, body.plaintextCrc32c)) {
        throw new RetryableKmsError("Cloud KMS plaintext failed its checksum in transit");
      }
      return plaintext;
    }, this.sleep);
  }

  private assertSize(plaintext: Buffer, aad: Buffer): void {
    if (plaintext.length + aad.length > MAX_PAYLOAD_BYTES) {
      throw new Error("payload is too large for a single Cloud KMS call");
    }
  }

  private async call(
    method: "encrypt" | "decrypt",
    request: Record<string, string>,
  ): Promise<Record<string, unknown>> {
    const token = await this.options.tokens.token();
    const url = `${ENDPOINT}/${this.options.keyName}:${method}`;
    let response: Response;
    try {
      response = await this.fetchImpl(url, {
        method: "POST",
        headers: {
          authorization: `Bearer ${token}`,
          "content-type": "application/json; charset=utf-8",
        },
        body: JSON.stringify(request),
      });
    } catch (error) {
      // A connection that never completed says nothing about whether the key works.
      throw new RetryableKmsError(`Cloud KMS is unreachable: ${String(error)}`);
    }
    const text = await response.text();
    if (!response.ok) {
      throw classify(response.status, errorDetail(text));
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(text) as unknown;
    } catch {
      throw new RetryableKmsError("Cloud KMS returned a body that is not JSON");
    }
    if (typeof parsed !== "object" || parsed === null) {
      throw new RetryableKmsError("Cloud KMS returned a body that is not an object");
    }
    return parsed as Record<string, unknown>;
  }
}
