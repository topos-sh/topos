import { z } from "zod";

/**
 * The gateway's environment — parsed lazily (deploy environments inject env at runtime, and a CI
 * build runs without production secrets). Every secret lives here and only here; log fields never
 * carry these values.
 */
const gatewaySchema = z.object({
  /** Connects as role `topos_gateway` (owns schema `gateway`; SELECT on the web tables it reads). */
  DATABASE_URL: z.string().min(1),
  /** The public MCP listener. */
  GATEWAY_BIND: z.string().default("127.0.0.1:8788"),
  /**
   * The internal lane's OWN listener — a second socket, never published, so the lane is
   * unreachable from the public bind by construction rather than by middleware order.
   */
  GATEWAY_INTERNAL_BIND: z.string().default("127.0.0.1:8789"),
  /**
   * The gateway's public base URL — the OAuth redirect_uri is `{here}/oauth/callback`, and the
   * MCP endpoints delivery hands out ride it.
   */
  GATEWAY_PUBLIC_URL: z.url(),
  /** The web app's public origin — authorize-page links and the callback's return fence. */
  TOPOS_PUBLIC_URL: z.url(),
  /**
   * The shared bearer for the web→gateway lane. Unset = the lane answers uniform 404 (unarmed);
   * only the sha256 is held past boot.
   */
  GATEWAY_INTERNAL_TOKEN: z.string().min(1).optional(),
  /**
   * Where the master key lives. `file` is today's deployment and the default; the KMS spellings
   * keep the key inside a key service and require that provider's configuration below — a missing
   * piece REFUSES TO BOOT rather than falling back to a file key.
   */
  GATEWAY_KEY_BACKEND: z.enum(["file", "gcp-kms", "aws-kms"]).default("file"),
  /**
   * Path to the 32-byte master key file (refused at boot if the size is anything else). Required
   * for the `file` backend; permitted alongside a KMS backend only because the re-wrap script
   * needs the OLD key to read rows it is migrating.
   */
  GATEWAY_MASTER_KEY_FILE: z.string().min(1).optional(),
  /**
   * The KMS key, as the provider's own full resource name:
   *   gcp-kms  projects/{p}/locations/{l}/keyRings/{r}/cryptoKeys/{k}
   *   aws-kms  arn:aws:kms:{region}:{account}:key/{id}   (the region is read out of the ARN)
   */
  GATEWAY_KMS_KEY: z.string().min(1).optional(),
  /** GCP: path to a service-account JSON key file — the container-on-a-box credential path. */
  GOOGLE_APPLICATION_CREDENTIALS: z.string().min(1).optional(),
  /**
   * GCP: a ready-made OAuth access token (`gcloud auth print-access-token`) for smoke-testing a
   * KMS key by hand. Refused in production — it expires in an hour and nothing here renews it.
   */
  GATEWAY_GCP_ACCESS_TOKEN: z.string().min(1).optional(),
  /** AWS: static credentials. Instance/container credential providers are not implemented. */
  AWS_ACCESS_KEY_ID: z.string().min(1).optional(),
  AWS_SECRET_ACCESS_KEY: z.string().min(1).optional(),
  AWS_SESSION_TOKEN: z.string().min(1).optional(),
  /** "1" lets upstream addresses resolve to private ranges (self-host with internal servers). */
  GATEWAY_ALLOW_PRIVATE_UPSTREAMS: z.enum(["0", "1"]).default("0"),
  APP_ENV: z.enum(["production", "development", "test"]).default("development"),
});

export type GatewayEnv = z.infer<typeof gatewaySchema>;

/**
 * Placeholder tokens refused in production forever — the same class of string the web app
 * refuses for its own lane. A deployment that copied an example env forward must not quietly
 * keep running on a bearer anyone can look up.
 */
const REFUSED_PRODUCTION_TOKENS = ["change-me-internal-token", "change-me-gateway-token"];

function assertNoShippedDefaults(env: GatewayEnv): void {
  if (env.APP_ENV !== "production") {
    return;
  }
  if (
    env.GATEWAY_INTERNAL_TOKEN !== undefined &&
    REFUSED_PRODUCTION_TOKENS.includes(env.GATEWAY_INTERNAL_TOKEN)
  ) {
    throw new Error(
      "refusing to start: GATEWAY_INTERNAL_TOKEN holds a placeholder value. " +
        "Set a real secret before exposing this deployment.",
    );
  }
}

/** GCP resource names are positional; a typo here is a 404 at first use, so shape-check it early. */
const GCP_KEY_NAME =
  /^projects\/[^/]+\/locations\/[^/]+\/keyRings\/[^/]+\/cryptoKeys\/[^/]+$/;
/**
 * A KEY arn (not an alias): the region sits in field 4 and the endpoint is derived from it.
 *
 * The COMMERCIAL partition only — `arn:aws:`. The client builds `kms.{region}.amazonaws.com`, and
 * that host is wrong for every other partition (China is `.amazonaws.com.cn`, the isolated regions
 * differ again), so accepting `arn:aws-cn:` here would send a boot call to a host that does not
 * serve that key. Refusing an ARN we would dial wrongly is the honest answer; widening this means
 * deriving the suffix from the partition in the client, not loosening the pattern here.
 */
const AWS_KEY_ARN = /^arn:aws:kms:([a-z0-9-]+):\d{12}:key\/[A-Za-z0-9-]+$/;

/**
 * The key backend's configuration is complete or the process does not start. A KMS backend that
 * fell back to a file key on a missing variable would write rows the operator believes are
 * KMS-held; a KMS backend that started without credentials would fail on the first credentialed
 * request instead of at deploy. Both are refusals, here, with the variable named.
 */
function assertKeyBackendConfigured(env: GatewayEnv): void {
  const missing = (variable: string): never => {
    throw new Error(
      `refusing to start: GATEWAY_KEY_BACKEND=${env.GATEWAY_KEY_BACKEND} requires ${variable}`,
    );
  };
  if (env.GATEWAY_KEY_BACKEND === "file") {
    if (env.GATEWAY_MASTER_KEY_FILE === undefined) {
      missing("GATEWAY_MASTER_KEY_FILE");
    }
    return;
  }
  if (env.GATEWAY_KMS_KEY === undefined) {
    missing("GATEWAY_KMS_KEY (the key's full resource name)");
  }
  if (env.GATEWAY_KEY_BACKEND === "gcp-kms") {
    if (!GCP_KEY_NAME.test(env.GATEWAY_KMS_KEY ?? "")) {
      throw new Error(
        "refusing to start: GATEWAY_KMS_KEY must be a Cloud KMS crypto key resource name " +
          "(projects/{p}/locations/{l}/keyRings/{r}/cryptoKeys/{k})",
      );
    }
    if (env.APP_ENV === "production" && env.GATEWAY_GCP_ACCESS_TOKEN !== undefined) {
      throw new Error(
        "refusing to start: GATEWAY_GCP_ACCESS_TOKEN is a short-lived token for hand smoke tests. " +
          "Production reads a service account from GOOGLE_APPLICATION_CREDENTIALS or the metadata server.",
      );
    }
    return;
  }
  if (!AWS_KEY_ARN.test(env.GATEWAY_KMS_KEY ?? "")) {
    throw new Error(
      "refusing to start: GATEWAY_KMS_KEY must be a KMS key ARN " +
        "(arn:aws:kms:{region}:{account}:key/{id}) — the region is read out of it",
    );
  }
  // No instance/container credential provider here: an unset pair is a refusal, never an ambient
  // guess at what role the host might be carrying.
  if (env.AWS_ACCESS_KEY_ID === undefined) {
    missing("AWS_ACCESS_KEY_ID");
  }
  if (env.AWS_SECRET_ACCESS_KEY === undefined) {
    missing("AWS_SECRET_ACCESS_KEY");
  }
}

/** The AWS region an ARN names — the endpoint host is built from it, never configured twice. */
export function awsRegionFromKeyArn(arn: string): string {
  const region = AWS_KEY_ARN.exec(arn)?.[1];
  if (region === undefined) {
    throw new Error("KMS key ARN carries no region");
  }
  return region;
}

let cached: GatewayEnv | undefined;

export function gatewayEnv(source: NodeJS.ProcessEnv = process.env): GatewayEnv {
  if (cached === undefined) {
    const parsed = gatewaySchema.parse(source);
    assertNoShippedDefaults(parsed);
    assertKeyBackendConfigured(parsed);
    cached = parsed;
  }
  return cached;
}

/** Parse without the process-wide cache — the tests' door. */
export function parseGatewayEnv(source: Record<string, string | undefined>): GatewayEnv {
  const parsed = gatewaySchema.parse(source);
  assertNoShippedDefaults(parsed);
  assertKeyBackendConfigured(parsed);
  return parsed;
}

/** "host:port" → Bun.serve inputs. Refuses shapes that would silently bind the wrong thing. */
export function parseBind(bind: string): { hostname: string; port: number } {
  const at = bind.lastIndexOf(":");
  if (at <= 0 || at === bind.length - 1) {
    throw new Error(`bind address must be host:port (got a malformed value)`);
  }
  const hostname = bind.slice(0, at);
  const port = Number(bind.slice(at + 1));
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error("bind port must be an integer in 1..65535");
  }
  return { hostname, port };
}
