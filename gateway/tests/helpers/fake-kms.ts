import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";
import { crc32c } from "../../service/kms-gcp";

/**
 * Fake key services — in-process stand-ins for Cloud KMS and AWS KMS that speak the real wire
 * shapes over an injected `fetch`. NOTHING in the suite reaches a cloud.
 *
 * They are not mocks that echo canned bytes: each holds a local AES-256-GCM key and performs a
 * real authenticated encryption with the AAD the request carried, so an AAD that does not match
 * on the way back fails the way the real service fails. That is what makes the row-binding tests
 * meaningful rather than decorative.
 *
 * Each returns its `fetch` plus a `calls` log and a `fail` hook, so a suite can make the service
 * misbehave (a 503, an unverified checksum) and watch the provider's answer.
 */

export interface FakeFailure {
  status: number;
  body: unknown;
}

export interface FakeKms {
  fetch: typeof fetch;
  /** One entry per HTTP call the provider made, in order — the request body included. */
  calls: { url: string; target: string; headers: Record<string, string>; body: string }[];
  /** Queued responses — each call shifts one; empty means behave normally. */
  failures: (FakeFailure | "unverified-plaintext-crc" | "corrupt-ciphertext-crc" | null)[];
}

function localSeal(key: Buffer, plaintext: Buffer, aad: Buffer): Buffer {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  cipher.setAAD(aad);
  const body = Buffer.concat([cipher.update(plaintext), cipher.final()]);
  return Buffer.concat([iv, cipher.getAuthTag(), body]);
}

function localOpen(key: Buffer, blob: Buffer, aad: Buffer): Buffer {
  const decipher = createDecipheriv("aes-256-gcm", key, blob.subarray(0, 12));
  decipher.setAAD(aad);
  decipher.setAuthTag(blob.subarray(12, 28));
  return Buffer.concat([decipher.update(blob.subarray(28)), decipher.final()]);
}

function headerMap(init: RequestInit | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  const headers = init?.headers;
  if (headers !== undefined && !Array.isArray(headers) && !(headers instanceof Headers)) {
    for (const [name, value] of Object.entries(headers)) {
      out[name.toLowerCase()] = String(value);
    }
  }
  return out;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function googleError(status: number, message: string, code: string): Response {
  return json(status, { error: { code: status, message, status: code } });
}

/** Cloud KMS: `{name}:encrypt` / `{name}:decrypt`, base64 fields, int64-as-string checksums. */
export function fakeGcpKms(options: {
  keyName: string;
  key?: Buffer;
  /** Answer for a different key than the one asked for. */
  answerAsKeyName?: string;
}): FakeKms {
  const key = options.key ?? randomBytes(32);
  const state: FakeKms = {
    calls: [],
    failures: [],
    fetch: null as unknown as typeof fetch,
  };
  state.fetch = (async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = typeof input === "string" ? input : input.toString();
    const target = url.endsWith(":encrypt") ? "encrypt" : url.endsWith(":decrypt") ? "decrypt" : "";
    const headers = headerMap(init);
    state.calls.push({ url, target, headers, body: String(init?.body ?? "") });
    const failure = state.failures.shift() ?? null;
    if (failure !== null && typeof failure === "object") {
      return json(failure.status, failure.body);
    }
    if (!(headers.authorization ?? "").startsWith("Bearer ")) {
      return googleError(401, "missing bearer", "UNAUTHENTICATED");
    }
    if (!url.startsWith(`https://cloudkms.googleapis.com/v1/${options.keyName}:`)) {
      return googleError(404, "no such key", "NOT_FOUND");
    }
    const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, string>;
    const aad = Buffer.from(body.additionalAuthenticatedData ?? "", "base64");
    const answeringKey = options.answerAsKeyName ?? options.keyName;
    if (target === "encrypt") {
      const plaintext = Buffer.from(body.plaintext ?? "", "base64");
      const ciphertext = localSeal(key, plaintext, aad);
      return json(200, {
        name: `${answeringKey}/cryptoKeyVersions/1`,
        ciphertext: ciphertext.toString("base64"),
        ciphertextCrc32c:
          failure === "corrupt-ciphertext-crc" ? "1" : crc32c(ciphertext).toString(),
        verifiedPlaintextCrc32c: failure !== "unverified-plaintext-crc",
        verifiedAdditionalAuthenticatedDataCrc32c: true,
        protectionLevel: "SOFTWARE",
      });
    }
    if (target === "decrypt") {
      let plaintext: Buffer;
      try {
        plaintext = localOpen(key, Buffer.from(body.ciphertext ?? "", "base64"), aad);
      } catch {
        // What a wrong AAD (or a foreign ciphertext) really looks like from Cloud KMS.
        return googleError(400, "Decryption failed", "INVALID_ARGUMENT");
      }
      return json(200, {
        plaintext: plaintext.toString("base64"),
        plaintextCrc32c: crc32c(plaintext).toString(),
        usedPrimary: true,
        protectionLevel: "SOFTWARE",
      });
    }
    return googleError(400, "unknown method", "INVALID_ARGUMENT");
  }) as typeof fetch;
  return state;
}

/** AWS KMS: one POST to `/`, the operation in `X-Amz-Target`, EncryptionContext as the AAD. */
export function fakeAwsKms(options: { keyArn: string; region: string; key?: Buffer }): FakeKms {
  const key = options.key ?? randomBytes(32);
  const state: FakeKms = {
    calls: [],
    failures: [],
    fetch: null as unknown as typeof fetch,
  };
  state.fetch = (async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = typeof input === "string" ? input : input.toString();
    const headers = headerMap(init);
    const target = headers["x-amz-target"] ?? "";
    state.calls.push({ url, target, headers, body: String(init?.body ?? "") });
    const failure = state.failures.shift() ?? null;
    if (failure !== null && typeof failure === "object") {
      return json(failure.status, failure.body);
    }
    if (!(headers.authorization ?? "").startsWith("AWS4-HMAC-SHA256 ")) {
      return json(403, { __type: "IncompleteSignature", message: "unsigned" });
    }
    const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
    // The context is a map; its ORDER is not part of the match, its pairs are.
    const context = Buffer.from(
      JSON.stringify(
        Object.entries((body.EncryptionContext ?? {}) as Record<string, string>).sort(),
      ),
      "utf8",
    );
    if (body.KeyId !== options.keyArn) {
      return json(400, { __type: "NotFoundException", message: "key not found" });
    }
    if (target === "TrentService.Encrypt") {
      const blob = localSeal(key, Buffer.from(String(body.Plaintext), "base64"), context);
      return json(200, {
        CiphertextBlob: blob.toString("base64"),
        KeyId: options.keyArn,
        EncryptionAlgorithm: "SYMMETRIC_DEFAULT",
      });
    }
    if (target === "TrentService.Decrypt") {
      try {
        const plaintext = localOpen(
          key,
          Buffer.from(String(body.CiphertextBlob), "base64"),
          context,
        );
        return json(200, { Plaintext: plaintext.toString("base64"), KeyId: options.keyArn });
      } catch {
        return json(400, {
          __type: "com.amazonaws.kms#InvalidCiphertextException",
          message: "bad ciphertext or context",
        });
      }
    }
    return json(400, { __type: "UnknownOperationException", message: target });
  }) as typeof fetch;
  return state;
}
