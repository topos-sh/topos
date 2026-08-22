import pg from "pg";
import { gatewayEnv } from "../service/env";
import { stderrLogger } from "../service/log";
import { createMasterKeyBackend, proveMasterKeyBackend } from "../service/master-key";
import { planRewrap, rewrapWorkspaceKeys } from "../service/rewrap";

/**
 * Re-wrap every workspace data key from the OLD master key backend to the one this environment now
 * configures. Run it as a one-off against the same environment the service runs with:
 *
 *   bun scripts/rewrap-workspace-keys.ts --from file --dry-run
 *   bun scripts/rewrap-workspace-keys.ts --from file
 *
 * The DESTINATION is `GATEWAY_KEY_BACKEND` — the same variable the service reads. `--from` names
 * the backend that wrote the rows today, and its configuration must still be present (the key file
 * still mounted, for a file → KMS move).
 *
 * Both backends are PROVEN by a wrap/unwrap round trip before a single row is touched: a run that
 * cannot reach the KMS stops before it has done anything.
 *
 * STOP THE GATEWAY FIRST. The service speaks exactly one backend, so a converted row is unreadable
 * to a still-running old process — and a workspace signing in mid-run would mint a key under the
 * old backend after the sweep had passed it. Convert with nothing serving, then deploy.
 *
 * Exit code is 0 only when every row is readable under the destination.
 */

const log = stderrLogger();
const env = gatewayEnv();

let plan;
try {
  plan = planRewrap(process.argv.slice(2), env.GATEWAY_KEY_BACKEND);
} catch (error) {
  process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(2);
}

const source = createMasterKeyBackend(env, log, { backend: plan.from });
const destination = createMasterKeyBackend(env, log);
await proveMasterKeyBackend(source, log);
await proveMasterKeyBackend(destination, log);

const pool = new pg.Pool({ connectionString: env.DATABASE_URL, max: 2 });
try {
  const summary = await rewrapWorkspaceKeys(pool, {
    from: source,
    to: destination,
    dryRun: plan.dryRun,
    log,
  });
  process.exitCode = summary.failed === 0 ? 0 : 1;
} finally {
  await pool.end();
}
