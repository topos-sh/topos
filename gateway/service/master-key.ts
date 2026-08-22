import { randomBytes } from "node:crypto";
import {
  digestsEqual,
  FileMasterKey,
  KEY_BYTES,
  loadMasterKey,
  type MasterKeyBackend,
  workspaceKeyAad,
} from "./crypto";
import { awsRegionFromKeyArn, type GatewayEnv } from "./env";
import {
  loadServiceAccountKey,
  metadataTokens,
  serviceAccountTokens,
  staticToken,
  type AccessTokenSource,
} from "./gcp-auth";
import { KmsMasterKey, RetryableKmsError } from "./kms";
import { AwsKms } from "./kms-aws";
import { GcpKms } from "./kms-gcp";
import type { Logger } from "./log";

/**
 * Which master key backend a deployment runs, built from its environment — the one place that
 * reads `GATEWAY_KEY_BACKEND`, so a new provider lands here and nowhere else.
 *
 * The KMS clients are handed the PLAIN fetch, not the guarded one: the guard exists to keep
 * upstream-influenced addresses out of private ranges, and one of the credential paths (the
 * instance metadata server) is a link-local address the guard is built to refuse. These hosts are
 * operator configuration — the guard's threat model does not reach them.
 */

export type KeyBackendName = GatewayEnv["GATEWAY_KEY_BACKEND"];

/** Every backend a deployment may name — the env enum and the re-wrap's `--from` share this list. */
export const KEY_BACKENDS: readonly KeyBackendName[] = ["file", "gcp-kms", "aws-kms"];

/** The env check already refuses a missing variable; this is the same refusal, one layer in. */
function required(value: string | undefined, variable: string, backend: string): string {
  if (value === undefined) {
    throw new Error(`the ${backend} key backend requires ${variable}`);
  }
  return value;
}

/**
 * Where Google credentials come from, in order: a mounted service-account key file, then a
 * supplied token (hand smoke tests, refused in production), then the instance metadata server.
 * Whichever answers, boot proves it works before a listener binds — see `proveMasterKeyBackend`.
 */
function gcpTokens(env: GatewayEnv, fetchImpl: typeof fetch): AccessTokenSource {
  if (env.GOOGLE_APPLICATION_CREDENTIALS !== undefined) {
    return serviceAccountTokens(loadServiceAccountKey(env.GOOGLE_APPLICATION_CREDENTIALS), {
      fetch: fetchImpl,
    });
  }
  if (env.GATEWAY_GCP_ACCESS_TOKEN !== undefined) {
    return staticToken(env.GATEWAY_GCP_ACCESS_TOKEN);
  }
  return metadataTokens({ fetch: fetchImpl });
}

export function createMasterKeyBackend(
  env: GatewayEnv,
  log: Logger,
  overrides: { backend?: KeyBackendName; fetch?: typeof fetch } = {},
): MasterKeyBackend {
  const backend = overrides.backend ?? env.GATEWAY_KEY_BACKEND;
  const fetchImpl = overrides.fetch ?? fetch;
  if (backend === "file") {
    const path = required(env.GATEWAY_MASTER_KEY_FILE, "GATEWAY_MASTER_KEY_FILE", backend);
    return new FileMasterKey(loadMasterKey(path, (message, fields) => log("warn", message, fields)));
  }
  const keyName = required(env.GATEWAY_KMS_KEY, "GATEWAY_KMS_KEY", backend);
  if (backend === "gcp-kms") {
    return new KmsMasterKey(
      new GcpKms({ keyName, tokens: gcpTokens(env, fetchImpl), fetch: fetchImpl }),
    );
  }
  return new KmsMasterKey(
    new AwsKms({
      keyArn: keyName,
      region: awsRegionFromKeyArn(keyName),
      credentials: {
        accessKeyId: required(env.AWS_ACCESS_KEY_ID, "AWS_ACCESS_KEY_ID", backend),
        secretAccessKey: required(env.AWS_SECRET_ACCESS_KEY, "AWS_SECRET_ACCESS_KEY", backend),
        ...(env.AWS_SESSION_TOKEN === undefined ? {} : { sessionToken: env.AWS_SESSION_TOKEN }),
      },
      fetch: fetchImpl,
    }),
  );
}

/** Never a workspace's AAD: a probe wrap must not be mistakable for a row's. */
const PROBE_AAD = "topos:gateway:master-key-probe";

/** How many stored wraps the boot check opens. A wrong key fails every row, so a handful settles
 *  it; checking the whole table would be one KMS round trip per workspace on every restart. */
const STORED_WRAP_SAMPLE = 5;

/**
 * Prove the configured backend can wrap AND unwrap before the process serves anything. For a file
 * key that is nearly free; for a KMS it is the round trip that turns "the operator typed a key
 * name and a credential path" into "this deployment can actually read its credentials" — at boot,
 * in the deploy's logs, rather than on the first agent's first tool call.
 */
export async function proveMasterKeyBackend(
  backend: MasterKeyBackend,
  log: Logger,
): Promise<void> {
  const probe = randomBytes(KEY_BYTES);
  const wrapped = await backend.wrapDataKey(probe, PROBE_AAD);
  const opened = await backend.unwrapDataKey(wrapped, PROBE_AAD);
  if (!digestsEqual(probe, opened)) {
    throw new Error("the master key backend did not return what it wrapped");
  }
  log("info", "master key backend ready", {
    backend: backend.describe,
    envelope: `0x${backend.formatVersion.toString(16).padStart(2, "0")}`,
  });
}


/**
 * Every wrap already in the database must open under the CONFIGURED backend, checked before a
 * listener binds. `proveMasterKeyBackend` asks whether the key works; this asks whether it is the
 * RIGHT key for what is stored — the question a half-finished migration, or a correct-but-different
 * key, would otherwise answer at an agent's first tool call instead of at deploy time.
 *
 * A deployment with nothing stored yet passes trivially, which is the ordinary first boot.
 */
export async function assertStoredWrapsOpen(
  rows: readonly { workspace_id: string; wrapped_key: Buffer }[],
  backend: MasterKeyBackend,
  log: Logger,
): Promise<void> {
  if (rows.length === 0) {
    log("info", "no stored workspace keys to verify");
    return;
  }
  // A SAMPLE, not the whole table. The question is "is this the key that wrapped what is stored",
  // and a wrong key fails EVERY row — so a handful answers it. Checking all of them would make
  // boot O(workspaces) sequential KMS round trips on every restart, and would turn one deliberately
  // unreadable row (the re-wrap leaves those as evidence) into a deployment that cannot start.
  const sample = rows.slice(0, STORED_WRAP_SAMPLE);
  let refused = 0;
  for (const row of sample) {
    try {
      // Straight to the backend, NOT through the cache: this needs an answer, not a usable key,
      // and routing it through EnvelopeCrypto would leave every sampled workspace's plaintext data
      // key resident in memory from boot — widening exactly the in-process exposure a key service
      // cannot cover.
      await backend.unwrapDataKey(row.wrapped_key, workspaceKeyAad(row.workspace_id));
    } catch (error) {
      if (error instanceof RetryableKmsError) {
        // "I could not ASK" is not "the key is wrong". A KMS blip must not produce a refusal whose
        // remedy text tells the operator to run a migration.
        log("warn", "could not verify a stored workspace key (the key service did not answer)", {
          error: (error as Error).name,
        });
        return;
      }
      refused += 1;
    }
  }
  if (refused === sample.length) {
    throw new Error(
      `refusing to serve: none of the ${sample.length} stored workspace keys sampled can be opened ` +
        "by the configured key backend, so this deployment would authenticate and then fail every " +
        "credential. Run the re-wrap (bun scripts/rewrap-workspace-keys.ts --from <backend>) or " +
        "point the key configuration back at the key that wrapped them.",
    );
  }
  if (refused > 0) {
    // A minority that will not open is a real problem for those workspaces and not a reason to
    // refuse everyone else — the re-wrap deliberately leaves such rows intact as evidence.
    log("warn", "some stored workspace keys cannot be opened by the configured backend", {
      refused,
      sampled: sample.length,
    });
    return;
  }
  log("info", "stored workspace keys verified", { sampled: sample.length, stored: rows.length });
}
