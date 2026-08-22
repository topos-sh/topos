import { readFileSync } from "node:fs";
import { createSign } from "node:crypto";
import { KMS_CALL_TIMEOUT_MS } from "./kms";

/**
 * Google access tokens for the KMS calls — the two credential paths a container actually has, and
 * nothing else. No ADC search, no gcloud, no vendor SDK: the token is either signed from a service
 * account key file the operator mounted, or fetched from the instance metadata server.
 *
 * TRANSPORT NOTE: these requests do NOT ride the guarded fetch. The metadata server lives at a
 * link-local address the SSRF guard exists to refuse, and both hosts here are configuration rather
 * than anything an upstream document can influence — the guard's threat model does not apply.
 *
 * Tokens are cached until shortly before expiry and minted at most once at a time; a token is
 * never logged, and neither is the assertion that bought it.
 */

/** Narrowest scope that permits encrypt/decrypt — `cloud-platform` would also open everything else. */
const KMS_SCOPE = "https://www.googleapis.com/auth/cloudkms";
const DEFAULT_TOKEN_URI = "https://oauth2.googleapis.com/token";
const METADATA_TOKEN_URL =
  "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const JWT_BEARER_GRANT = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/** Assertions are minted per token, so a short life costs nothing and bounds a leaked one. */
const ASSERTION_TTL_SECONDS = 3600;
const REFRESH_EARLY_MS = 5 * 60 * 1000;

export interface AccessTokenSource {
  /** Log-safe: which credential path, and whose identity — never the token. */
  readonly describe: string;
  token(): Promise<string>;
}

interface Minted {
  accessToken: string;
  expiresInSeconds: number;
}

function base64url(input: Buffer | string): string {
  return (typeof input === "string" ? Buffer.from(input, "utf8") : input)
    .toString("base64")
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

/** One mint at a time, reused until expiry — a burst of proxied calls must not mint a burst. */
function cache(
  mint: () => Promise<Minted>,
  now: () => number,
): () => Promise<string> {
  let token: string | undefined;
  let expiresAt = 0;
  let inFlight: Promise<string> | undefined;
  return async () => {
    if (token !== undefined && now() < expiresAt) {
      return token;
    }
    if (inFlight === undefined) {
      inFlight = (async () => {
        try {
          const minted = await mint();
          const lifetimeMs = minted.expiresInSeconds * 1000;
          // A token shorter than the refresh margin still gets used, for half its life.
          const early = Math.min(REFRESH_EARLY_MS, Math.floor(lifetimeMs / 2));
          token = minted.accessToken;
          expiresAt = now() + lifetimeMs - early;
          return minted.accessToken;
        } finally {
          inFlight = undefined;
        }
      })();
    }
    return inFlight;
  };
}

function readMinted(body: unknown, where: string): Minted {
  const record = body as { access_token?: unknown; expires_in?: unknown };
  if (typeof record.access_token !== "string" || record.access_token === "") {
    throw new Error(`${where} returned no access_token`);
  }
  // A missing expires_in is treated as the shortest plausible life rather than as forever.
  const expiresIn = typeof record.expires_in === "number" ? record.expires_in : 60;
  return { accessToken: record.access_token, expiresInSeconds: expiresIn };
}

async function postJson(
  fetchImpl: typeof fetch,
  url: string,
  init: RequestInit,
  where: string,
): Promise<unknown> {
  const response = await fetchImpl(url, { ...init, signal: AbortSignal.timeout(KMS_CALL_TIMEOUT_MS) });
  const text = await response.text();
  if (!response.ok) {
    // The body of a token failure carries `error`/`error_description`, never a credential — but
    // it is upstream text, so it is truncated rather than trusted to be small.
    throw new Error(`${where} refused (${response.status}): ${text.slice(0, 300)}`);
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    throw new Error(`${where} returned a body that is not JSON`);
  }
}

export interface ServiceAccountKey {
  clientEmail: string;
  privateKeyPem: string;
  tokenUri: string;
  projectId: string | null;
}

/**
 * Parse a service-account JSON key. Every field the JWT flow needs is checked HERE, at boot, so a
 * truncated or wrong-type key file refuses to start instead of failing at the first credential.
 */
export function parseServiceAccountKey(json: string, path: string): ServiceAccountKey {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json) as unknown;
  } catch {
    throw new Error(`${path} is not valid JSON`);
  }
  if (typeof parsed !== "object" || parsed === null) {
    throw new Error(`${path} is not a service account key file`);
  }
  const record = parsed as Record<string, unknown>;
  if (record.type !== "service_account") {
    throw new Error(
      `${path} has type "${String(record.type)}" — the gateway needs a service_account key file`,
    );
  }
  const clientEmail = record.client_email;
  const privateKey = record.private_key;
  if (typeof clientEmail !== "string" || clientEmail === "") {
    throw new Error(`${path} has no client_email`);
  }
  if (typeof privateKey !== "string" || !privateKey.includes("PRIVATE KEY")) {
    throw new Error(`${path} has no PEM private_key`);
  }
  return {
    clientEmail,
    privateKeyPem: privateKey,
    tokenUri: typeof record.token_uri === "string" ? record.token_uri : DEFAULT_TOKEN_URI,
    projectId: typeof record.project_id === "string" ? record.project_id : null,
  };
}

export function loadServiceAccountKey(path: string): ServiceAccountKey {
  return parseServiceAccountKey(readFileSync(path, "utf8"), path);
}

/**
 * RFC 7523 self-signed JWT bearer: sign an assertion with the account's own key, trade it at
 * Google's token endpoint for an access token. This is the path a container with a mounted key
 * file takes — the one the box will run.
 */
export function serviceAccountTokens(
  key: ServiceAccountKey,
  deps: { fetch?: typeof fetch; now?: () => number } = {},
): AccessTokenSource {
  const fetchImpl = deps.fetch ?? fetch;
  const now = deps.now ?? Date.now;
  const mint = async (): Promise<Minted> => {
    const issuedAt = Math.floor(now() / 1000);
    const header = base64url(JSON.stringify({ alg: "RS256", typ: "JWT" }));
    const claims = base64url(
      JSON.stringify({
        iss: key.clientEmail,
        scope: KMS_SCOPE,
        aud: key.tokenUri,
        iat: issuedAt,
        exp: issuedAt + ASSERTION_TTL_SECONDS,
      }),
    );
    const signingInput = `${header}.${claims}`;
    const signature = base64url(
      createSign("RSA-SHA256").update(signingInput).sign(key.privateKeyPem),
    );
    const body = new URLSearchParams({
      grant_type: JWT_BEARER_GRANT,
      assertion: `${signingInput}.${signature}`,
    });
    const parsed = await postJson(
      fetchImpl,
      key.tokenUri,
      {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded" },
        body: body.toString(),
      },
      "the Google token endpoint",
    );
    return readMinted(parsed, "the Google token endpoint");
  };
  return { describe: `service account ${key.clientEmail}`, token: cache(mint, now) };
}

/** The instance metadata server — GCE, GKE, Cloud Run. Plain http and the header are both required. */
export function metadataTokens(
  deps: { fetch?: typeof fetch; now?: () => number } = {},
): AccessTokenSource {
  const fetchImpl = deps.fetch ?? fetch;
  const now = deps.now ?? Date.now;
  const mint = async (): Promise<Minted> => {
    const parsed = await postJson(
      fetchImpl,
      METADATA_TOKEN_URL,
      { method: "GET", headers: { "metadata-flavor": "Google" } },
      "the GCE metadata server",
    );
    return readMinted(parsed, "the GCE metadata server");
  };
  return { describe: "instance metadata server", token: cache(mint, now) };
}

/**
 * A token handed in whole (`gcloud auth print-access-token`) — a hand smoke test against a real
 * key, refused in production by the env check because nothing renews it.
 */
export function staticToken(token: string): AccessTokenSource {
  return { describe: "a supplied access token", token: async () => token };
}
