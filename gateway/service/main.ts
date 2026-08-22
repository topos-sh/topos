import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import pg from "pg";
import { handleGatewayRequest } from "../core/engine";
import type { GatewayContext } from "../core/ports";
import { EnvelopeCrypto } from "./crypto";
import { gatewayEnv, parseBind } from "./env";
import { createGuardedFetch } from "./guarded-fetch";
import { createInternalHandler } from "./internal";
import { stderrLogger } from "./log";
import {
  assertStoredWrapsOpen,
  createMasterKeyBackend,
  proveMasterKeyBackend,
} from "./master-key";
import { runMigrations } from "./migrate";
import type { OauthConfig } from "./oauth";
import { createPublicHandler, type EngineHandler } from "./server";
import { PgStore } from "./store";
import { BufferedUsageSink } from "./usage";

/**
 * The container entry point. Boot order mirrors the plane's: logs armed → env parsed → the master
 * key backend built and PROVEN by a wrap/unwrap round trip → migrations under the advisory lock →
 * adapters wired → BOTH listeners bound (public MCP + the never-published internal lane) →
 * signal-driven shutdown that drains the usage buffer before the pool closes.
 */

const log = stderrLogger();
const env = gatewayEnv();

// Before the database, before the listeners: a deployment that cannot open its own credentials
// must fail in the deploy, not on some agent's first tool call.
const masterKey = createMasterKeyBackend(env, log);
if (
  env.GATEWAY_KEY_BACKEND !== "file" &&
  env.GATEWAY_MASTER_KEY_FILE !== undefined &&
  existsSync(env.GATEWAY_MASTER_KEY_FILE)
) {
  // A real key file still on disk under a KMS backend is the middle of a migration — or its
  // forgotten leftover. Either way the host is still holding a master key that opens nothing.
  log("warn", "a master key file is still present but this key backend does not use it", {
    backend: env.GATEWAY_KEY_BACKEND,
  });
}
await proveMasterKeyBackend(masterKey, log);

const migrationsDir = join(dirname(fileURLToPath(import.meta.url)), "..", "migrations");
await runMigrations(env.DATABASE_URL, migrationsDir, log);

const pool = new pg.Pool({ connectionString: env.DATABASE_URL, max: 10 });
const crypto = new EnvelopeCrypto(masterKey);
const store = new PgStore(pool, crypto, env.TOPOS_PUBLIC_URL, log);
// The key works (proved above) — but is it the key that wrapped what is already stored? A
// deployment moved to a KMS without its re-wrap, or pointed at a different valid key, would
// otherwise bind and then fail every credential. Nothing stored yet passes trivially.
const storedWraps = await pool.query<{ workspace_id: string; wrapped_key: Buffer }>(
  "SELECT workspace_id, wrapped_key FROM gateway.workspace_key",
);
await assertStoredWrapsOpen(storedWraps.rows, crypto, log);

const usage = new BufferedUsageSink(pool, log);
const guardedFetch = createGuardedFetch(
  { allowPrivate: env.GATEWAY_ALLOW_PRIVATE_UPSTREAMS === "1" },
  log,
);
const config: OauthConfig = {
  gatewayPublicUrl: env.GATEWAY_PUBLIC_URL,
  toposPublicUrl: env.TOPOS_PUBLIC_URL,
};

const engine: EngineHandler = handleGatewayRequest;
const context: GatewayContext = {
  store,
  usage,
  env: {
    fetch: guardedFetch,
    now: () => Date.now(),
    log,
  },
};

const publicBind = parseBind(env.GATEWAY_BIND);
const publicServer = Bun.serve({
  ...publicBind,
  idleTimeout: 0,
  fetch: createPublicHandler({ engine, context, store, config, guardedFetch, log }),
});
log("info", "public listener bound", { hostname: publicBind.hostname, port: publicBind.port });

const internalBind = parseBind(env.GATEWAY_INTERNAL_BIND);
const internalServer = Bun.serve({
  ...internalBind,
  fetch: createInternalHandler({
    store,
    config,
    guardedFetch,
    log,
    internalToken: env.GATEWAY_INTERNAL_TOKEN,
  }),
});
log("info", "internal listener bound", {
  hostname: internalBind.hostname,
  port: internalBind.port,
  armed: env.GATEWAY_INTERNAL_TOKEN === undefined ? "no" : "yes",
});

// PID 1 in its image: SIGTERM must drain, not die.
let shuttingDown = false;
async function shutdown(signal: string): Promise<void> {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  log("info", "shutting down", { signal });
  await publicServer.stop(true);
  await internalServer.stop(true);
  await usage.close();
  await pool.end();
  process.exit(0);
}

process.on("SIGTERM", () => void shutdown("SIGTERM"));
process.on("SIGINT", () => void shutdown("SIGINT"));
