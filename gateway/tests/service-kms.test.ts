import { createHash, createHmac, createVerify, generateKeyPairSync, randomBytes } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { EnvelopeCrypto, FileMasterKey, workspaceKeyAad } from "../service/crypto";
import type { MasterKeyBackend } from "../service/crypto";
import { awsRegionFromKeyArn, parseGatewayEnv } from "../service/env";
import { metadataTokens, parseServiceAccountKey, serviceAccountTokens } from "../service/gcp-auth";
import { KmsMasterKey, RetryableKmsError } from "../service/kms";
import { AwsKms, deriveSigningKey, signKmsRequest } from "../service/kms-aws";
import { crc32c, GcpKms } from "../service/kms-gcp";
import {
  assertStoredWrapsOpen,
  createMasterKeyBackend,
  proveMasterKeyBackend,
} from "../service/master-key";
import { fakeAwsKms, fakeGcpKms } from "./helpers/fake-kms";

/**
 * The KMS key backend, end to end against FAKE key services (`tests/helpers/fake-kms.ts`). No
 * test here reaches a cloud: what is proven is the envelope, the dispatch, the wire shapes, the
 * integrity checks, the retry classification, and the refusals — everything except whether a real
 * Google or AWS endpoint accepts these bytes, which only a credentialed deployment can answer.
 */

const KEY_NAME = "projects/topos-test/locations/us-east1/keyRings/gateway/cryptoKeys/master";
const KEY_ARN = "arn:aws:kms:us-east-1:123456789012:key/2f4b1c3d-1111-2222-3333-444455556666";
const quiet = () => {};
const nosleep = async (): Promise<void> => {};

const dir = mkdtempSync(join(tmpdir(), "gw-kms-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

function gcpBackend(fake = fakeGcpKms({ keyName: KEY_NAME })): {
  backend: KmsMasterKey;
  fake: ReturnType<typeof fakeGcpKms>;
} {
  const backend = new KmsMasterKey(
    new GcpKms({
      keyName: KEY_NAME,
      tokens: { describe: "a test token", token: async () => "test-token" },
      fetch: fake.fetch,
      sleep: nosleep,
    }),
  );
  return { backend, fake };
}

function awsBackend(fake = fakeAwsKms({ keyArn: KEY_ARN, region: "us-east-1" })): {
  backend: KmsMasterKey;
  fake: ReturnType<typeof fakeAwsKms>;
} {
  const backend = new KmsMasterKey(
    new AwsKms({
      keyArn: KEY_ARN,
      region: "us-east-1",
      credentials: { accessKeyId: "AKIDEXAMPLE", secretAccessKey: "secret" },
      fetch: fake.fetch,
      sleep: nosleep,
    }),
  );
  return { backend, fake };
}

describe("crc32c", () => {
  it("matches the published check values", () => {
    // The standard CRC-32/ISCSI check value for "123456789", and the empty-input identity.
    expect(crc32c(Buffer.from("123456789", "utf8"))).toBe(0xe3069283);
    expect(crc32c(Buffer.alloc(0))).toBe(0);
    expect(crc32c(Buffer.from([0x00]))).toBe(0x527d5351);
  });
});

describe("the KMS backend round-trips", () => {
  it("wraps and unwraps a data key without the master key entering the process", async () => {
    const { backend, fake } = gcpBackend();
    const key = randomBytes(32);
    const wrapped = await backend.wrapDataKey(key, workspaceKeyAad("ws1"));
    expect(wrapped[0]).toBe(0x02);
    expect(wrapped[1]).toBe(0x01);
    // Nothing in the stored envelope is the data key itself.
    expect(wrapped.subarray(2).includes(key)).toBe(false);
    expect(await backend.unwrapDataKey(wrapped, workspaceKeyAad("ws1"))).toEqual(key);
    expect(fake.calls.map((call) => call.target)).toEqual(["encrypt", "decrypt"]);
  });

  it("carries a whole credential through the same two layers", async () => {
    const { backend } = gcpBackend();
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    const payload = { secret: "tok-abc", refreshToken: "ref-xyz" };
    const sealed = await crypto.encryptCredential(payload, "cred_a", "ws1", wrapped);
    expect(await crypto.decryptCredential(sealed, "cred_a", "ws1", wrapped)).toEqual(payload);
  });

  it("costs one KMS round trip per workspace, not one per proxied call", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    const { backend } = gcpBackend(fake);
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    for (let call = 0; call < 20; call += 1) {
      const sealed = await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
      await crypto.decryptCredential(sealed, "cred_a", "ws1", wrapped);
    }
    // One encrypt to mint the data key, one decrypt to unwrap it. Forty credential operations
    // added nothing: this is what keeps a KMS backend off the per-request path.
    expect(fake.calls.map((call) => call.target)).toEqual(["encrypt", "decrypt"]);
  });

  it("does the same through AWS", async () => {
    const { backend } = awsBackend();
    const crypto = new EnvelopeCrypto(backend);
    const wrapped = await crypto.mintWrappedWorkspaceKey("ws1");
    expect(wrapped[0]).toBe(0x02);
    expect(wrapped[1]).toBe(0x02);
    const sealed = await crypto.encryptCredential({ secret: "s" }, "cred_a", "ws1", wrapped);
    expect((await crypto.decryptCredential(sealed, "cred_a", "ws1", wrapped)).secret).toBe("s");
  });
});

describe("AAD binds a wrapped key to its workspace on both clouds", () => {
  it("a GCP-wrapped key does not unwrap under another workspace", async () => {
    const { backend } = gcpBackend();
    const wrapped = await backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    await expect(backend.unwrapDataKey(wrapped, workspaceKeyAad("ws2"))).rejects.toThrow(
      /refused the request/,
    );
  });

  it("an AWS-wrapped key does not unwrap under another workspace", async () => {
    const { backend } = awsBackend();
    const wrapped = await backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    await expect(backend.unwrapDataKey(wrapped, workspaceKeyAad("ws2"))).rejects.toThrow(
      /InvalidCiphertextException/,
    );
  });

  it("refuses to wrap with no AAD at all", async () => {
    const { backend } = gcpBackend();
    await expect(backend.wrapDataKey(randomBytes(32), "")).rejects.toThrow(/empty AAD/);
  });
});

describe("the version byte decides which key opens a row", () => {
  it("a 0x01 row refuses to open under a KMS backend", async () => {
    const fileWrapped = await new FileMasterKey(randomBytes(32)).wrapDataKey(
      randomBytes(32),
      workspaceKeyAad("ws1"),
    );
    const { backend, fake } = gcpBackend();
    await expect(backend.unwrapDataKey(fileWrapped, workspaceKeyAad("ws1"))).rejects.toThrow(
      /written by the file key backend; this gateway runs the gcp-kms backend/,
    );
    // Refused locally: the bytes never left the process.
    expect(fake.calls).toEqual([]);
  });

  it("a 0x02 row refuses to open under the file backend", async () => {
    const { backend } = gcpBackend();
    const kmsWrapped = await backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    await expect(
      new FileMasterKey(randomBytes(32)).unwrapDataKey(kmsWrapped, workspaceKeyAad("ws1")),
    ).rejects.toThrow(/written by a KMS key backend; this gateway runs the file backend/);
  });

  it("a GCP row refuses to open under an AWS-configured gateway", async () => {
    const { backend: gcp } = gcpBackend();
    const { backend: aws, fake } = awsBackend();
    const wrapped = await gcp.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    await expect(aws.unwrapDataKey(wrapped, workspaceKeyAad("ws1"))).rejects.toThrow(
      /written by gcp-kms; this gateway runs aws-kms/,
    );
    expect(fake.calls).toEqual([]);
  });

  it("refuses a truncated envelope rather than guessing", async () => {
    const { backend } = gcpBackend();
    await expect(backend.unwrapDataKey(Buffer.alloc(0), workspaceKeyAad("ws1"))).rejects.toThrow(
      /empty value/,
    );
    await expect(
      backend.unwrapDataKey(Buffer.from([0x02]), workspaceKeyAad("ws1")),
    ).rejects.toThrow(/truncated envelope/);
  });
});

describe("Cloud KMS integrity checks", () => {
  it("retries a response the service did not checksum-verify, then accepts the good one", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    fake.failures.push("unverified-plaintext-crc");
    const { backend } = gcpBackend(fake);
    const key = randomBytes(32);
    const wrapped = await backend.wrapDataKey(key, workspaceKeyAad("ws1"));
    expect(fake.calls.length).toBe(2);
    expect(await backend.unwrapDataKey(wrapped, workspaceKeyAad("ws1"))).toEqual(key);
  });

  it("retries a ciphertext whose checksum does not match what arrived", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    fake.failures.push("corrupt-ciphertext-crc");
    const { backend } = gcpBackend(fake);
    await backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    expect(fake.calls.length).toBe(2);
  });

  it("puts the plaintext, the AAD and both checksums on the wire", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    const { backend } = gcpBackend(fake);
    const key = randomBytes(32);
    await backend.wrapDataKey(key, workspaceKeyAad("ws1"));
    const sent = JSON.parse(fake.calls[0]?.body ?? "{}") as Record<string, string>;
    expect(fake.calls[0]?.url).toBe(`https://cloudkms.googleapis.com/v1/${KEY_NAME}:encrypt`);
    expect(Buffer.from(sent.plaintext ?? "", "base64")).toEqual(key);
    expect(Buffer.from(sent.additionalAuthenticatedData ?? "", "base64").toString("utf8")).toBe(
      "topos:gateway:workspace-key:ws1",
    );
    // int64 on the wire is a decimal STRING, and it is the checksum of the RAW bytes.
    expect(sent.plaintextCrc32c).toBe(crc32c(key).toString());
    expect(sent.additionalAuthenticatedDataCrc32c).toBe(
      crc32c(Buffer.from("topos:gateway:workspace-key:ws1", "utf8")).toString(),
    );
  });

  it("refuses an answer that names a different key", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME, answerAsKeyName: `${KEY_NAME}-other` });
    const { backend } = gcpBackend(fake);
    await expect(backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"))).rejects.toThrow(
      /answered for key .*-other/,
    );
  });
});

describe("retry classification", () => {
  it("retries a 503 and then succeeds", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    fake.failures.push({ status: 503, body: { error: { message: "backend down", status: "UNAVAILABLE" } } });
    const { backend } = gcpBackend(fake);
    await backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    expect(fake.calls.length).toBe(2);
  });

  it("does not retry a permission refusal", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    for (let i = 0; i < 3; i += 1) {
      fake.failures.push({
        status: 403,
        body: { error: { message: "caller lacks cloudkms.cryptoKeyVersions.useToEncrypt", status: "PERMISSION_DENIED" } },
      });
    }
    const { backend } = gcpBackend(fake);
    await expect(backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"))).rejects.toThrow(
      /PERMISSION_DENIED/,
    );
    expect(fake.calls.length).toBe(1);
  });

  it("gives up after a bounded number of tries", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    for (let i = 0; i < 5; i += 1) {
      fake.failures.push({ status: 500, body: { error: { message: "internal", status: "INTERNAL" } } });
    }
    const { backend } = gcpBackend(fake);
    await expect(backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"))).rejects.toThrow(
      /unavailable/,
    );
    expect(fake.calls.length).toBe(3);
  });

  it("retries an AWS throttle even though it arrives as HTTP 400", async () => {
    const fake = fakeAwsKms({ keyArn: KEY_ARN, region: "us-east-1" });
    fake.failures.push({ status: 400, body: { __type: "ThrottlingException", message: "slow down" } });
    const { backend } = awsBackend(fake);
    await backend.wrapDataKey(randomBytes(32), workspaceKeyAad("ws1"));
    expect(fake.calls.length).toBe(2);
  });

  it("does not retry an AWS ciphertext refusal", async () => {
    const fake = fakeAwsKms({ keyArn: KEY_ARN, region: "us-east-1" });
    const { backend } = awsBackend(fake);
    await expect(
      backend.unwrapDataKey(
        Buffer.concat([Buffer.from([0x02, 0x02]), randomBytes(64)]),
        workspaceKeyAad("ws1"),
      ),
    ).rejects.toThrow(/InvalidCiphertextException/);
    expect(fake.calls.length).toBe(1);
  });
});

describe("SigV4", () => {
  it("derives the signing key of the published example", () => {
    // AWS's own worked example — the value below was also re-derived independently with openssl.
    expect(
      deriveSigningKey(
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        "20150830",
        "us-east-1",
        "iam",
      ).toString("hex"),
    ).toBe("c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
  });

  it("builds the canonical request and signature the spec describes", () => {
    const body = '{"KeyId":"k"}';
    const signed = signKmsRequest({
      host: "kms.us-east-1.amazonaws.com",
      target: "TrentService.Encrypt",
      body,
      region: "us-east-1",
      credentials: { accessKeyId: "AKIDEXAMPLE", secretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY" },
      nowMs: Date.UTC(2015, 7, 30, 12, 36, 0),
    });
    // Written out by hand — headers lowercased and sorted, a blank line after them, the body's
    // hash last, no trailing newline. The same construction reproduces AWS's published
    // `get-vanilla` signature when computed with an independent tool.
    const expectedCanonical = [
      "POST",
      "/",
      "",
      "content-type:application/x-amz-json-1.1",
      "host:kms.us-east-1.amazonaws.com",
      "x-amz-date:20150830T123600Z",
      "x-amz-target:TrentService.Encrypt",
      "",
      "content-type;host;x-amz-date;x-amz-target",
      createHash("sha256").update(body).digest("hex"),
    ].join("\n");
    expect(signed.canonicalRequest).toBe(expectedCanonical);
    expect(signed.stringToSign).toBe(
      [
        "AWS4-HMAC-SHA256",
        "20150830T123600Z",
        "20150830/us-east-1/kms/aws4_request",
        createHash("sha256").update(expectedCanonical).digest("hex"),
      ].join("\n"),
    );
    const expectedSignature = createHmac(
      "sha256",
      deriveSigningKey("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20150830", "us-east-1", "kms"),
    )
      .update(signed.stringToSign)
      .digest("hex");
    expect(signed.headers.authorization).toBe(
      "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/kms/aws4_request, " +
        "SignedHeaders=content-type;host;x-amz-date;x-amz-target, " +
        `Signature=${expectedSignature}`,
    );
    // `host` is signed but never sent: fetch forbids the header and sets it from the URL.
    expect(signed.headers.host).toBeUndefined();
    expect(signed.headers["x-amz-date"]).toBe("20150830T123600Z");
  });

  it("signs and sends a session token when one is configured", () => {
    const signed = signKmsRequest({
      host: "kms.eu-west-1.amazonaws.com",
      target: "TrentService.Decrypt",
      body: "{}",
      region: "eu-west-1",
      credentials: { accessKeyId: "A", secretAccessKey: "S", sessionToken: "SESSION" },
      nowMs: Date.UTC(2026, 0, 2, 3, 4, 5),
    });
    expect(signed.canonicalRequest).toContain("x-amz-security-token:SESSION");
    expect(signed.headers.authorization).toContain(
      "SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target",
    );
    expect(signed.headers["x-amz-security-token"]).toBe("SESSION");
  });
});

describe("Google credentials", () => {
  const { privateKey, publicKey } = generateKeyPairSync("rsa", {
    modulusLength: 2048,
    publicKeyEncoding: { type: "spki", format: "pem" },
    privateKeyEncoding: { type: "pkcs8", format: "pem" },
  });
  const keyJson = JSON.stringify({
    type: "service_account",
    project_id: "topos-test",
    private_key_id: "kid1",
    private_key: privateKey,
    client_email: "gateway@topos-test.iam.gserviceaccount.com",
    token_uri: "https://oauth2.googleapis.com/token",
  });

  function tokenEndpoint(): { fetch: typeof fetch; assertions: string[]; minted: number } {
    const state = { assertions: [] as string[], minted: 0, fetch: null as unknown as typeof fetch };
    state.fetch = (async (_input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const body = new URLSearchParams(String(init?.body ?? ""));
      state.assertions.push(body.get("assertion") ?? "");
      state.minted += 1;
      expect(body.get("grant_type")).toBe("urn:ietf:params:oauth:grant-type:jwt-bearer");
      return new Response(JSON.stringify({ access_token: `tok-${state.minted}`, expires_in: 3600 }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    }) as typeof fetch;
    return state;
  }

  it("refuses a key file that is not a service account", () => {
    expect(() => parseServiceAccountKey('{"type":"authorized_user"}', "/k.json")).toThrow(
      /service_account/,
    );
    expect(() => parseServiceAccountKey("not json", "/k.json")).toThrow(/valid JSON/);
    expect(() => parseServiceAccountKey('{"type":"service_account"}', "/k.json")).toThrow(
      /client_email/,
    );
  });

  it("signs an RFC 7523 assertion the account's own key verifies", async () => {
    const endpoint = tokenEndpoint();
    const source = serviceAccountTokens(parseServiceAccountKey(keyJson, "/k.json"), {
      fetch: endpoint.fetch,
      now: () => Date.UTC(2026, 5, 1),
    });
    expect(await source.token()).toBe("tok-1");
    const assertion = endpoint.assertions[0] ?? "";
    const [header, claims, signature] = assertion.split(".");
    expect(JSON.parse(Buffer.from(header ?? "", "base64url").toString("utf8"))).toEqual({
      alg: "RS256",
      typ: "JWT",
    });
    const parsedClaims = JSON.parse(Buffer.from(claims ?? "", "base64url").toString("utf8")) as Record<string, unknown>;
    expect(parsedClaims.iss).toBe("gateway@topos-test.iam.gserviceaccount.com");
    expect(parsedClaims.aud).toBe("https://oauth2.googleapis.com/token");
    expect(parsedClaims.scope).toBe("https://www.googleapis.com/auth/cloudkms");
    expect(Number(parsedClaims.exp) - Number(parsedClaims.iat)).toBe(3600);
    expect(
      createVerify("RSA-SHA256")
        .update(`${header}.${claims}`)
        .verify(publicKey, Buffer.from(signature ?? "", "base64url")),
    ).toBe(true);
  });

  it("mints one token for a burst and re-mints only after expiry", async () => {
    const endpoint = tokenEndpoint();
    let now = Date.UTC(2026, 5, 1);
    const source = serviceAccountTokens(parseServiceAccountKey(keyJson, "/k.json"), {
      fetch: endpoint.fetch,
      now: () => now,
    });
    const burst = await Promise.all([source.token(), source.token(), source.token()]);
    expect(burst).toEqual(["tok-1", "tok-1", "tok-1"]);
    expect(endpoint.minted).toBe(1);
    // Still inside the token's life minus the refresh margin.
    now += 3000 * 1000;
    expect(await source.token()).toBe("tok-1");
    now += 600 * 1000;
    expect(await source.token()).toBe("tok-2");
    expect(endpoint.minted).toBe(2);
  });

  it("reads the metadata server with the header it requires", async () => {
    const seen: Record<string, string>[] = [];
    const source = metadataTokens({
      fetch: (async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        expect(String(input)).toBe(
          "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token",
        );
        seen.push((init?.headers ?? {}) as Record<string, string>);
        return new Response(JSON.stringify({ access_token: "metadata-tok", expires_in: 3599 }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }) as typeof fetch,
    });
    expect(await source.token()).toBe("metadata-tok");
    expect(seen[0]?.["metadata-flavor"]).toBe("Google");
  });

  it("surfaces a token endpoint refusal instead of a decrypt failure later", async () => {
    const source = serviceAccountTokens(parseServiceAccountKey(keyJson, "/k.json"), {
      fetch: (async (): Promise<Response> =>
        new Response(JSON.stringify({ error: "invalid_grant" }), { status: 400 })) as typeof fetch,
    });
    await expect(source.token()).rejects.toThrow(/token endpoint refused \(400\)/);
  });
});

describe("the env refuses to boot on an incomplete key backend", () => {
  const base = {
    DATABASE_URL: "postgres://topos_gateway@localhost/topos",
    GATEWAY_PUBLIC_URL: "https://gateway.example.com",
    TOPOS_PUBLIC_URL: "https://topos.example.com",
  };

  it("defaults to the file backend and still requires the key file", () => {
    expect(parseGatewayEnv({ ...base, GATEWAY_MASTER_KEY_FILE: "/k" }).GATEWAY_KEY_BACKEND).toBe(
      "file",
    );
    expect(() => parseGatewayEnv({ ...base })).toThrow(/requires GATEWAY_MASTER_KEY_FILE/);
  });

  it("refuses a KMS backend with no key named — it never falls back to a file key", () => {
    expect(() =>
      parseGatewayEnv({ ...base, GATEWAY_KEY_BACKEND: "gcp-kms", GATEWAY_MASTER_KEY_FILE: "/k" }),
    ).toThrow(/requires GATEWAY_KMS_KEY/);
    expect(() =>
      parseGatewayEnv({ ...base, GATEWAY_KEY_BACKEND: "aws-kms", GATEWAY_MASTER_KEY_FILE: "/k" }),
    ).toThrow(/requires GATEWAY_KMS_KEY/);
  });

  it("refuses a key name that is not the provider's resource shape", () => {
    expect(() =>
      parseGatewayEnv({ ...base, GATEWAY_KEY_BACKEND: "gcp-kms", GATEWAY_KMS_KEY: "master" }),
    ).toThrow(/crypto key resource name/);
    expect(() =>
      parseGatewayEnv({ ...base, GATEWAY_KEY_BACKEND: "gcp-kms", GATEWAY_KMS_KEY: KEY_ARN }),
    ).toThrow(/crypto key resource name/);
    expect(() =>
      parseGatewayEnv({
        ...base,
        GATEWAY_KEY_BACKEND: "aws-kms",
        GATEWAY_KMS_KEY: "alias/gateway",
        AWS_ACCESS_KEY_ID: "A",
        AWS_SECRET_ACCESS_KEY: "S",
      }),
    ).toThrow(/key ARN/);
  });

  it("refuses AWS with no credentials in the environment", () => {
    expect(() =>
      parseGatewayEnv({ ...base, GATEWAY_KEY_BACKEND: "aws-kms", GATEWAY_KMS_KEY: KEY_ARN }),
    ).toThrow(/requires AWS_ACCESS_KEY_ID/);
    expect(() =>
      parseGatewayEnv({
        ...base,
        GATEWAY_KEY_BACKEND: "aws-kms",
        GATEWAY_KMS_KEY: KEY_ARN,
        AWS_ACCESS_KEY_ID: "A",
      }),
    ).toThrow(/requires AWS_SECRET_ACCESS_KEY/);
  });

  it("refuses a hand-pasted access token in production", () => {
    const env = {
      ...base,
      GATEWAY_KEY_BACKEND: "gcp-kms",
      GATEWAY_KMS_KEY: KEY_NAME,
      GATEWAY_GCP_ACCESS_TOKEN: "ya29.short-lived",
    };
    expect(parseGatewayEnv({ ...env, APP_ENV: "development" }).GATEWAY_GCP_ACCESS_TOKEN).toBe(
      "ya29.short-lived",
    );
    expect(() => parseGatewayEnv({ ...env, APP_ENV: "production" })).toThrow(
      /GATEWAY_GCP_ACCESS_TOKEN is a short-lived token/,
    );
  });

  it("accepts a complete KMS configuration and keeps the file path for the re-wrap", () => {
    const env = parseGatewayEnv({
      ...base,
      GATEWAY_KEY_BACKEND: "gcp-kms",
      GATEWAY_KMS_KEY: KEY_NAME,
      GATEWAY_MASTER_KEY_FILE: "/run/topos/gateway-master.key",
      GOOGLE_APPLICATION_CREDENTIALS: "/run/topos/sa.json",
    });
    expect(env.GATEWAY_KEY_BACKEND).toBe("gcp-kms");
    expect(env.GATEWAY_MASTER_KEY_FILE).toBe("/run/topos/gateway-master.key");
  });
});

describe("the boot proof", () => {
  const base = {
    DATABASE_URL: "postgres://topos_gateway@localhost/topos",
    GATEWAY_PUBLIC_URL: "https://gateway.example.com",
    TOPOS_PUBLIC_URL: "https://topos.example.com",
  };

  it("builds a gcp-kms backend from the environment and proves it round-trips", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    const env = parseGatewayEnv({
      ...base,
      GATEWAY_KEY_BACKEND: "gcp-kms",
      GATEWAY_KMS_KEY: KEY_NAME,
      GATEWAY_GCP_ACCESS_TOKEN: "ya29.test",
    });
    const lines: { message: string; fields?: Record<string, string | number> }[] = [];
    const backend = createMasterKeyBackend(env, quiet, { fetch: fake.fetch });
    await proveMasterKeyBackend(backend, (_level, message, fields) => lines.push({ message, fields }));
    expect(fake.calls.map((call) => call.target)).toEqual(["encrypt", "decrypt"]);
    expect(lines[0]?.message).toBe("master key backend ready");
    expect(lines[0]?.fields?.envelope).toBe("0x02");
  });

  it("builds a file backend from the environment", async () => {
    const path = join(dir, "master.key");
    writeFileSync(path, randomBytes(32), { mode: 0o600 });
    const env = parseGatewayEnv({ ...base, GATEWAY_MASTER_KEY_FILE: path });
    const backend = createMasterKeyBackend(env, quiet);
    expect(backend.formatVersion).toBe(0x01);
    await proveMasterKeyBackend(backend, quiet);
  });

  it("fails the boot when the key service refuses", async () => {
    const fake = fakeGcpKms({ keyName: KEY_NAME });
    for (let i = 0; i < 3; i += 1) {
      fake.failures.push({
        status: 403,
        body: { error: { message: "denied", status: "PERMISSION_DENIED" } },
      });
    }
    const env = parseGatewayEnv({
      ...base,
      GATEWAY_KEY_BACKEND: "gcp-kms",
      GATEWAY_KMS_KEY: KEY_NAME,
      GATEWAY_GCP_ACCESS_TOKEN: "ya29.test",
    });
    await expect(
      proveMasterKeyBackend(createMasterKeyBackend(env, quiet, { fetch: fake.fetch }), quiet),
    ).rejects.toThrow(/PERMISSION_DENIED/);
  });
});

describe("the boot check against what is already stored", () => {
  // A backend that opens exactly the wraps it is told to, and fails the rest the way a real one
  // does — a definite refusal, not a transport error.
  const backendThatOpens = (openable: (ws: string) => boolean): MasterKeyBackend => ({
    describe: "fake",
    formatVersion: 0x01,
    wrapDataKey: async (key: Buffer) => key,
    unwrapDataKey: async (envelope: Buffer, aad: string) => {
      if (!openable(aad)) {
        throw new Error("refused");
      }
      return envelope;
    },
  });

  it("passes trivially when nothing is stored yet", async () => {
    const seen: string[] = [];
    await assertStoredWrapsOpen([], backendThatOpens(() => true), (_l, m) => {
      seen.push(m);
    });
    expect(seen).toContain("no stored workspace keys to verify");
  });

  it("refuses to serve when NONE of the sampled wraps open — the wrong-key deployment", async () => {
    const rows = [
      { workspace_id: "ws_a", wrapped_key: Buffer.from([1]) },
      { workspace_id: "ws_b", wrapped_key: Buffer.from([2]) },
    ];
    await expect(
      assertStoredWrapsOpen(rows, backendThatOpens(() => false), () => {}),
    ).rejects.toThrow(/none of the 2 stored workspace keys sampled can be opened/);
  });

  it("warns rather than refuses when only SOME will not open", async () => {
    // The re-wrap deliberately leaves a row it could not convert, as evidence. That must not stop
    // every other workspace from being served.
    const rows = [
      { workspace_id: "ws_ok", wrapped_key: Buffer.from([1]) },
      { workspace_id: "ws_evidence", wrapped_key: Buffer.from([2]) },
    ];
    const warned: string[] = [];
    await assertStoredWrapsOpen(
      rows,
      backendThatOpens((aad) => aad.endsWith("ws_ok")),
      (level, m) => {
        if (level === "warn") warned.push(m);
      },
    );
    expect(warned.join()).toMatch(/some stored workspace keys cannot be opened/);
  });

  it("does not refuse when the key service could not be ASKED", async () => {
    // A KMS blip must never produce a refusal telling the operator to run a migration.
    const rows = [{ workspace_id: "ws_a", wrapped_key: Buffer.from([1]) }];
    const backend: MasterKeyBackend = {
      describe: "fake",
      formatVersion: 0x02,
      wrapDataKey: async (k: Buffer) => k,
      unwrapDataKey: async () => {
        throw new RetryableKmsError("Cloud KMS is unavailable (503)");
      },
    };
    await expect(assertStoredWrapsOpen(rows, backend, () => {})).resolves.toBeUndefined();
  });

  it("samples rather than opening every row — boot stays O(1) in workspaces", async () => {
    const rows = Array.from({ length: 400 }, (_, i) => ({
      workspace_id: `ws_${i}`,
      wrapped_key: Buffer.from([1]),
    }));
    let calls = 0;
    const backend: MasterKeyBackend = {
      describe: "fake",
      formatVersion: 0x02,
      wrapDataKey: async (k: Buffer) => k,
      unwrapDataKey: async (e: Buffer) => {
        calls += 1;
        return e;
      },
    };
    await assertStoredWrapsOpen(rows, backend, () => {});
    expect(calls).toBeLessThanOrEqual(5);
  });
});

describe("the AWS key ARN's partition", () => {
  it("refuses a partition whose endpoint this client would dial wrongly", () => {
    // arn:aws-cn keys live at .amazonaws.com.cn; accepting one here would boot against a host
    // that does not serve it. Refuse rather than dial wrong.
    expect(() =>
      awsRegionFromKeyArn("arn:aws-cn:kms:cn-north-1:123456789012:key/abc-def"),
    ).toThrow();
  });

  it("accepts a commercial ARN and names its region", () => {
    expect(awsRegionFromKeyArn("arn:aws:kms:us-west-2:123456789012:key/abc-def")).toBe("us-west-2");
  });
});

describe("the guards the security review asked for", () => {
  it("refuses aws-kms in production — hand-rolled signing, never verified against AWS", () => {
    expect(() =>
      parseGatewayEnv({
        DATABASE_URL: "postgres://x",
        GATEWAY_PUBLIC_URL: "https://gw.example.com",
        TOPOS_PUBLIC_URL: "https://topos.example.com",
        APP_ENV: "production",
        GATEWAY_KEY_BACKEND: "aws-kms",
        GATEWAY_KMS_KEY: "arn:aws:kms:us-west-2:123456789012:key/abc-def",
        AWS_ACCESS_KEY_ID: "AKIA",
        AWS_SECRET_ACCESS_KEY: "secret",
      }),
    ).toThrow(/aws-kms is not verified against a real AWS endpoint/);
  });

  it("still allows aws-kms outside production, where an operator chose it deliberately", () => {
    expect(() =>
      parseGatewayEnv({
        DATABASE_URL: "postgres://x",
        GATEWAY_PUBLIC_URL: "https://gw.example.com",
        TOPOS_PUBLIC_URL: "https://topos.example.com",
        APP_ENV: "development",
        GATEWAY_KEY_BACKEND: "aws-kms",
        GATEWAY_KMS_KEY: "arn:aws:kms:us-west-2:123456789012:key/abc-def",
        AWS_ACCESS_KEY_ID: "AKIA",
        AWS_SECRET_ACCESS_KEY: "secret",
      }),
    ).not.toThrow();
  });
});
