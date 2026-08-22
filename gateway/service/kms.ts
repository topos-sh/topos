import { assertWrapVersion, type MasterKeyBackend } from "./crypto";

/**
 * The KMS-wrapped data key — format 0x02. The master key never enters this process: `wrapDataKey`
 * and `unwrapDataKey` are RPCs to a key service that holds it, and what lands in
 * `gateway.workspace_key` is the service's opaque ciphertext under a two-byte header.
 *
 *   [0] 0x02 — a KMS wrote this
 *   [1] provider tag — 0x01 gcp-kms, 0x02 aws-kms
 *   [2..] the provider's own ciphertext, byte-for-byte
 *
 * The provider tag is not redundant with the deployment's configuration: it is what makes an
 * AWS-configured gateway REFUSE a GCP-wrapped row here, with a sentence, instead of shipping the
 * bytes to a cloud that will reject them with a generic error. Tags are permanent — a row written
 * today must still name its provider years from now.
 *
 * AAD IS LOAD-BEARING and every provider must carry it into the call as authenticated data (GCP:
 * `additionalAuthenticatedData`; AWS: `EncryptionContext`). It binds a wrapped key to its
 * workspace, which is what makes a row copied between workspaces fail rather than open.
 */

export const KMS_FORMAT_VERSION = 0x02;

export const PROVIDER_TAG = {
  gcp: 0x01,
  aws: 0x02,
} as const;

const TAG_NAMES = new Map<number, string>([
  [PROVIDER_TAG.gcp, "gcp-kms"],
  [PROVIDER_TAG.aws, "aws-kms"],
]);

/**
 * One key service's symmetric encrypt/decrypt. Deliberately smaller than any vendor SDK surface:
 * a provider owns its wire format, its auth, and nothing else — no key creation, no rotation, no
 * listing. Adding a sibling provider touches this file's tag table and nothing above it.
 */
export interface KmsProvider {
  /** The `GATEWAY_KEY_BACKEND` spelling — used in refusals an operator has to act on. */
  readonly name: string;
  /** The permanent byte that marks this provider's rows. */
  readonly tag: number;
  /** Log-safe identity: the key resource name, which is configuration, not a secret. */
  readonly describe: string;
  encrypt(plaintext: Buffer, aad: string): Promise<Buffer>;
  decrypt(ciphertext: Buffer, aad: string): Promise<Buffer>;
}

/**
 * An empty AAD would be a wrap with no row binding — accepted by both clouds, and silently
 * unbound. Every provider passes its input through here before the call.
 */
export function assertAad(aad: string): string {
  if (aad === "") {
    throw new Error("refusing a KMS wrap with an empty AAD");
  }
  return aad;
}

/** Bounded, jittered retry for the transport-level failures a key service is allowed to have. */
export class RetryableKmsError extends Error {}

export const KMS_ATTEMPTS = 3;

export async function withKmsRetry<T>(
  attempt: () => Promise<T>,
  sleep: (ms: number) => Promise<void>,
): Promise<T> {
  let last: unknown;
  for (let tries = 0; tries < KMS_ATTEMPTS; tries += 1) {
    try {
      return await attempt();
    } catch (error) {
      // Only the marked failures repeat. A refusal (bad key, denied permission, wrong AAD) is an
      // answer, and answering it three times just delays the log line an operator needs.
      if (!(error instanceof RetryableKmsError)) {
        throw error;
      }
      last = error;
      if (tries + 1 < KMS_ATTEMPTS) {
        // Full jitter over an exponential window — encrypt and decrypt are both safe to repeat.
        await sleep(Math.floor(Math.random() * Math.min(2000, 100 * 2 ** tries)));
      }
    }
  }
  throw last;
}

export class KmsMasterKey implements MasterKeyBackend {
  readonly formatVersion = KMS_FORMAT_VERSION;

  constructor(private provider: KmsProvider) {}

  get describe(): string {
    return `${this.provider.name} ${this.provider.describe}`;
  }

  async wrapDataKey(plaintextKey: Buffer, aad: string): Promise<Buffer> {
    const ciphertext = await this.provider.encrypt(plaintextKey, aad);
    if (ciphertext.length === 0) {
      // A KMS that answered 200 with nothing would otherwise write a header-only row that reads
      // back as an empty key. Refuse at the write, where the caller can still recover.
      throw new Error(`${this.provider.name} returned an empty ciphertext`);
    }
    return Buffer.concat([Buffer.from([KMS_FORMAT_VERSION, this.provider.tag]), ciphertext]);
  }

  async unwrapDataKey(envelope: Buffer, aad: string): Promise<Buffer> {
    assertWrapVersion(envelope, KMS_FORMAT_VERSION, this.provider.name);
    const tag = envelope[1];
    if (tag !== this.provider.tag) {
      const wrote =
        tag === undefined
          ? "a truncated envelope"
          : (TAG_NAMES.get(tag) ?? `provider tag 0x${tag.toString(16).padStart(2, "0")}`);
      throw new Error(
        `wrapped workspace key was written by ${wrote}; this gateway runs ${this.provider.name}`,
      );
    }
    const ciphertext = envelope.subarray(2);
    if (ciphertext.length === 0) {
      // A header with no body is a corrupted row, not something to ask a cloud about.
      throw new Error("wrapped workspace key carries no ciphertext");
    }
    return this.provider.decrypt(ciphertext, aad);
  }
}
