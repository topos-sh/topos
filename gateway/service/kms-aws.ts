import { createHash, createHmac } from "node:crypto";
import {
  assertAad,
  PROVIDER_TAG,
  RetryableKmsError,
  withKmsRetry,
  type KmsProvider,
  KMS_CALL_TIMEOUT_MS,
} from "./kms";

/**
 * AWS KMS over its JSON API with plain `fetch` and hand-written SigV4 — the sibling of
 * `kms-gcp.ts`, reaching the same `KmsProvider` contract so nothing above either file knows which
 * cloud holds the master key.
 *
 * WHY SIGV4 BY HAND: this signs ONE shape of request — POST to `/` on one host, no query string,
 * a JSON body always present, a fixed header set. That erases the parts of SigV4 that are actually
 * hard (path/query canonicalization, unsigned and chunked payloads, presigned URLs), leaving the
 * HMAC chain below. `@aws-sdk/client-kms` would be the largest dependency in an image that
 * otherwise carries two.
 *
 * CREDENTIALS: the static environment triple only. The ECS/EKS container credential providers are
 * deliberately NOT implemented — a half-built provider chain that guesses at an ambient role is
 * worse than a refusal at boot, and the env check refuses instead.
 *
 * AAD: `EncryptionContext` is AWS's authenticated-data analogue and decrypt must present the same
 * map, so the row binding survives the move. It is NOT secret — CloudTrail logs it in the clear —
 * which is fine: the AAD here is a workspace id, already in every log line.
 */

const SERVICE = "kms";
const ALGORITHM = "AWS4-HMAC-SHA256";
const CONTENT_TYPE = "application/x-amz-json-1.1";
/** The AAD travels as one entry; the key names the property, the value is the AAD string itself. */
const CONTEXT_KEY = "topos_aad";
/** Symmetric `Plaintext` caps at 4096 bytes decoded — a data key is 32. */
const MAX_PLAINTEXT_BYTES = 4096;

export interface AwsCredentials {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
}

export interface AwsKmsOptions {
  /** `arn:aws:kms:{region}:{account}:key/{id}` — the region below is read out of it. */
  keyArn: string;
  region: string;
  credentials: AwsCredentials;
  fetch?: typeof fetch;
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
}

function sha256Hex(input: string | Buffer): string {
  return createHash("sha256").update(input).digest("hex");
}

function hmac(key: Buffer | string, data: string): Buffer {
  return createHmac("sha256", key).update(data, "utf8").digest();
}

/** `20260821T203825Z` and its `20260821` scope date — the two must agree or the signature fails. */
export function amzDate(nowMs: number): { stamp: string; date: string } {
  const stamp = new Date(nowMs).toISOString().replaceAll("-", "").replaceAll(":", "").replace(/\.\d+/, "");
  return { stamp, date: stamp.slice(0, 8) };
}

/** The four-step derivation, verbatim from the signing spec. */
export function deriveSigningKey(
  secretAccessKey: string,
  date: string,
  region: string,
  service: string,
): Buffer {
  const kDate = hmac(`AWS4${secretAccessKey}`, date);
  const kRegion = hmac(kDate, region);
  const kService = hmac(kRegion, service);
  return hmac(kService, "aws4_request");
}

export interface SignInput {
  host: string;
  target: string;
  body: string;
  region: string;
  credentials: AwsCredentials;
  nowMs: number;
}

export interface SignedRequest {
  /**
   * The headers to SEND — `host` is signed but not among them: it is a forbidden header name for
   * fetch, and the runtime sets it from the URL to exactly the value signed here.
   */
  headers: Record<string, string>;
  /** Exposed for the tests that pin the wire format — the signature is a hash of exactly this. */
  canonicalRequest: string;
  stringToSign: string;
}

/**
 * Sign one POST-to-`/` request. The body string passed here MUST be the same string handed to
 * fetch: the signature covers its bytes, and a second `JSON.stringify` that orders keys
 * differently would produce a valid-looking request that AWS rejects as unsigned.
 */
export function signKmsRequest(input: SignInput): SignedRequest {
  const { stamp, date } = amzDate(input.nowMs);
  const headers: Record<string, string> = {
    "content-type": CONTENT_TYPE,
    host: input.host,
    "x-amz-date": stamp,
    "x-amz-target": input.target,
  };
  if (input.credentials.sessionToken !== undefined) {
    // A session token must be BOTH sent and signed; signing it is what binds the temporary
    // credential to this request.
    headers["x-amz-security-token"] = input.credentials.sessionToken;
  }
  const names = Object.keys(headers).sort();
  const canonicalHeaders = names.map((name) => `${name}:${headers[name] ?? ""}\n`).join("");
  const signedHeaders = names.join(";");
  const canonicalRequest = [
    "POST",
    "/",
    "",
    canonicalHeaders,
    signedHeaders,
    sha256Hex(input.body),
  ].join("\n");
  const scope = `${date}/${input.region}/${SERVICE}/aws4_request`;
  const stringToSign = [ALGORITHM, stamp, scope, sha256Hex(canonicalRequest)].join("\n");
  const signature = hmac(
    deriveSigningKey(input.credentials.secretAccessKey, date, input.region, SERVICE),
    stringToSign,
  ).toString("hex");
  const { host: _signedHost, ...sendable } = headers;
  return {
    headers: {
      ...sendable,
      authorization:
        `${ALGORITHM} Credential=${input.credentials.accessKeyId}/${scope}, ` +
        `SignedHeaders=${signedHeaders}, Signature=${signature}`,
    },
    canonicalRequest,
    stringToSign,
  };
}

/** The shape name out of `__type`, which arrives fully qualified: `com.amazonaws.kms#Name`. */
function errorCode(text: string): { code: string; message: string } {
  try {
    const parsed = JSON.parse(text) as Record<string, unknown>;
    const raw = typeof parsed.__type === "string" ? parsed.__type : "";
    const code = raw.split("#").pop()?.split(":")[0] ?? "";
    const message = parsed.message ?? parsed.Message;
    return { code, message: typeof message === "string" ? message : text.slice(0, 300) };
  } catch {
    return { code: "", message: text.slice(0, 300) };
  }
}

/**
 * Classify on the CODE first, the status second: KMS answers a throttle with HTTP 400, so a
 * status-only reading would treat back-pressure as a permanent refusal and drop the request.
 */
const RETRYABLE_CODES = new Set([
  "ThrottlingException",
  "Throttling",
  "TooManyRequestsException",
  "RequestLimitExceeded",
  "RequestTimeout",
  "RequestTimeoutException",
  "KMSInternalException",
  "DependencyTimeoutException",
  "KeyUnavailableException",
  "InternalFailure",
  "InternalError",
  "ServiceUnavailable",
]);

function classify(status: number, text: string): Error {
  const { code, message } = errorCode(text);
  const label = code === "" ? `HTTP ${status}` : code;
  if (RETRYABLE_CODES.has(code) || (code === "" && status >= 500) || status >= 500) {
    return new RetryableKmsError(`AWS KMS is unavailable (${label}): ${message}`);
  }
  return new Error(`AWS KMS refused the request (${label}): ${message}`);
}

export class AwsKms implements KmsProvider {
  readonly name = "aws-kms";
  readonly tag = PROVIDER_TAG.aws;
  private fetchImpl: typeof fetch;
  private sleep: (ms: number) => Promise<void>;
  private now: () => number;

  constructor(private options: AwsKmsOptions) {
    this.fetchImpl = options.fetch ?? fetch;
    this.sleep = options.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
    this.now = options.now ?? Date.now;
  }

  get describe(): string {
    return this.options.keyArn;
  }

  async encrypt(plaintext: Buffer, aad: string): Promise<Buffer> {
    if (plaintext.length > MAX_PLAINTEXT_BYTES) {
      throw new Error("payload is too large for a single AWS KMS call");
    }
    return withKmsRetry(async () => {
      const body = await this.call("TrentService.Encrypt", {
        KeyId: this.options.keyArn,
        Plaintext: plaintext.toString("base64"),
        EncryptionContext: { [CONTEXT_KEY]: assertAad(aad) },
      });
      if (typeof body.CiphertextBlob !== "string") {
        throw new Error("AWS KMS returned no CiphertextBlob");
      }
      // The key that answered must be the key configured — an alias resolving elsewhere would
      // otherwise write rows this deployment cannot open.
      if (typeof body.KeyId === "string" && body.KeyId !== this.options.keyArn) {
        throw new Error(`AWS KMS answered for key ${body.KeyId}, not ${this.options.keyArn}`);
      }
      return Buffer.from(body.CiphertextBlob, "base64");
    }, this.sleep);
  }

  async decrypt(ciphertext: Buffer, aad: string): Promise<Buffer> {
    return withKmsRetry(async () => {
      const body = await this.call("TrentService.Decrypt", {
        // Naming the key is optional for symmetric ciphertext and load-bearing anyway: without it
        // KMS would decrypt under whatever key the blob names.
        KeyId: this.options.keyArn,
        CiphertextBlob: ciphertext.toString("base64"),
        EncryptionContext: { [CONTEXT_KEY]: assertAad(aad) },
      });
      if (typeof body.Plaintext !== "string") {
        throw new Error("AWS KMS returned no Plaintext");
      }
      return Buffer.from(body.Plaintext, "base64");
    }, this.sleep);
  }

  private async call(
    target: string,
    request: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    const host = `kms.${this.options.region}.amazonaws.com`;
    // ONE serialization, signed and sent — see signKmsRequest.
    const body = JSON.stringify(request);
    const signed = signKmsRequest({
      host,
      target,
      body,
      region: this.options.region,
      credentials: this.options.credentials,
      nowMs: this.now(),
    });
    let response: Response;
    try {
      response = await this.fetchImpl(`https://${host}/`, {
        signal: AbortSignal.timeout(KMS_CALL_TIMEOUT_MS),
        method: "POST",
        headers: signed.headers,
        body,
      });
    } catch (error) {
      throw new RetryableKmsError(`AWS KMS is unreachable: ${String(error)}`);
    }
    const text = await response.text();
    if (!response.ok) {
      throw classify(response.status, text);
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(text) as unknown;
    } catch {
      throw new RetryableKmsError("AWS KMS returned a body that is not JSON");
    }
    if (typeof parsed !== "object" || parsed === null) {
      throw new RetryableKmsError("AWS KMS returned a body that is not an object");
    }
    return parsed as Record<string, unknown>;
  }
}
