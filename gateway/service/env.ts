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
  /** Path to the 32-byte master key file (refused at boot if the size is anything else). */
  GATEWAY_MASTER_KEY_FILE: z.string().min(1),
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

let cached: GatewayEnv | undefined;

export function gatewayEnv(source: NodeJS.ProcessEnv = process.env): GatewayEnv {
  if (cached === undefined) {
    const parsed = gatewaySchema.parse(source);
    assertNoShippedDefaults(parsed);
    cached = parsed;
  }
  return cached;
}

/** Parse without the process-wide cache — the tests' door. */
export function parseGatewayEnv(source: Record<string, string | undefined>): GatewayEnv {
  const parsed = gatewaySchema.parse(source);
  assertNoShippedDefaults(parsed);
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
