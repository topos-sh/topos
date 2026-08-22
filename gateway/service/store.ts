import { randomBytes } from "node:crypto";
import type { Pool, PoolClient } from "pg";
import type {
  AuthMode,
  Credential,
  GatewayStore,
  ObservedTool,
  ResolvedServer,
  SessionRef,
  ToolPolicy,
} from "../core/ports";
import type { CredentialPayload, EnvelopeCrypto } from "./crypto";
import type { Logger } from "./log";

/**
 * The container's GatewayStore — every answer read from Postgres per call, which is what makes
 * revocation a row change. Reads cross into schema `web` under SELECT grants (session, the
 * connection chain, tool policy); writes stay in schema `gateway`.
 *
 * Custody: ciphertext never leaves this module undecrypted-into-anything-but-a-Credential, and
 * plaintext never goes back out except inside one. The web-facing rows (`credential` metadata)
 * carry no secret material at all.
 */

const HEX_64 = /^[0-9a-f]{64}$/;

type Queryable = Pick<Pool, "query">;

function mintId(prefix: string): string {
  return `${prefix}_${randomBytes(16).toString("hex")}`;
}

/** The first streamable-http remote's declared secret-header slot, if the document names one. */
export function secretHeaderFromDocument(document: unknown): string | null {
  if (typeof document !== "object" || document === null) {
    return null;
  }
  const remotes = (document as { remotes?: unknown }).remotes;
  if (!Array.isArray(remotes)) {
    return null;
  }
  const placed = remotes.find(
    (entry): entry is Record<string, unknown> =>
      typeof entry === "object" &&
      entry !== null &&
      (entry as { type?: unknown }).type === "streamable-http" &&
      typeof (entry as { url?: unknown }).url === "string",
  );
  if (placed === undefined || !Array.isArray(placed.headers)) {
    return null;
  }
  for (const header of placed.headers) {
    if (
      typeof header === "object" &&
      header !== null &&
      (header as { isSecret?: unknown }).isSecret === true &&
      typeof (header as { name?: unknown }).name === "string"
    ) {
      return (header as { name: string }).name;
    }
  }
  return null;
}

export interface OauthFlowRow {
  id: string;
  state: string;
  workspaceId: string;
  serverId: string;
  userId: string | null;
  codeVerifier: string;
  clientId: string;
  tokenEndpoint: string;
  resource: string;
  returnTo: string;
  expiresAt: Date;
}

export interface StoreCredentialInput {
  workspaceId: string;
  serverId: string;
  userId: string | null;
  authKind: "oauth" | "manual";
  payload: CredentialPayload;
  createdByDisplay: string;
}

export class PgStore implements GatewayStore {
  /**
   * While `withRefreshLock` holds a credential's row FOR UPDATE, writes to that credential must
   * ride the SAME connection — a pool sibling updating the locked row would deadlock against the
   * transaction waiting on the caller's callback. Single-threaded process, per-id entries.
   */
  private refreshClients = new Map<string, PoolClient>();

  constructor(
    private pool: Pool,
    private crypto: EnvelopeCrypto,
    private toposPublicUrl: string,
    private log: Logger,
  ) {}

  async sessionByTokenSha256(hex: string): Promise<SessionRef | null> {
    if (!HEX_64.test(hex)) {
      return null;
    }
    const result = await this.pool.query<{
      id: string;
      workspace_id: string;
      user_id: string;
      display_name: string;
    }>(
      `SELECT id, workspace_id, user_id, display_name
         FROM web.cli_session
        WHERE credential_sha256 = decode($1, 'hex') AND status = 'active'`,
      [hex],
    );
    const row = result.rows[0];
    if (row === undefined) {
      return null;
    }
    return {
      sessionId: row.id,
      workspaceId: row.workspace_id,
      userId: row.user_id,
      displayName: row.display_name,
    };
  }

  async connectedServer(workspaceId: string, serverId: string): Promise<ResolvedServer | null> {
    // The one resolution: a connection's pin exactly, else the server's `current` — the same
    // COALESCE the web spells, aliased the same way (bm the connection, ms the server).
    const result = await this.pool.query<{
      server_id: string;
      display_name: string;
      auth_mode: string | null;
      url: string | null;
      transport: string | null;
      document: unknown;
      bundle_name: string;
      workspace_display_name: string;
    }>(
      `SELECT ms.id AS server_id,
              ms.display_name,
              ms.auth_mode,
              r.url,
              r.transport,
              r.document,
              b.name AS bundle_name,
              w.display_name AS workspace_display_name
         FROM web.bundle_mcp bm
         JOIN web.bundle b ON b.id = bm.bundle_id AND b.workspace_id = bm.workspace_id
         JOIN web.workspace w ON w.id = bm.workspace_id
         JOIN web.mcp_server ms ON ms.id = bm.server_id
         LEFT JOIN web.mcp_server_revision r
           ON r.id = COALESCE(bm.pinned_revision_id, ms.current_revision_id)
          AND r.server_id = ms.id
        WHERE bm.workspace_id = $1 AND bm.server_id = $2 AND b.status = 'active'`,
      [workspaceId, serverId],
    );
    const row = result.rows[0];
    if (row === undefined || row.url === null || row.transport === null) {
      // No connection, no resolvable revision, or a packages-only server (nothing to dial).
      return null;
    }
    return {
      serverId: row.server_id,
      displayName: row.display_name,
      url: row.url,
      transport: row.transport,
      authMode: (row.auth_mode ?? null) as AuthMode,
      secretHeader: secretHeaderFromDocument(row.document),
      // Single-tenancy grammar: the server page is origin-rooted under the mcp base.
      authorizeUrl: `${this.toposPublicUrl.replace(/\/$/, "")}/mcp/${row.bundle_name}`,
      workspaceDisplayName: row.workspace_display_name,
    };
  }

  async toolPolicy(workspaceId: string, serverId: string): Promise<ToolPolicy> {
    const policy = await this.pool.query<{ mode: string }>(
      `SELECT mode FROM web.mcp_tool_policy WHERE workspace_id = $1 AND server_id = $2`,
      [workspaceId, serverId],
    );
    const mode = policy.rows[0]?.mode;
    if (mode !== "selected") {
      // No row = all; an unknown mode value would be a broken write, and `all` is the shape the
      // schema's CHECK keeps it from ever being.
      return { mode: "all", selected: new Set() };
    }
    const selection = await this.pool.query<{ tool_name: string }>(
      `SELECT tool_name FROM web.mcp_tool_selection WHERE workspace_id = $1 AND server_id = $2`,
      [workspaceId, serverId],
    );
    return { mode: "selected", selected: new Set(selection.rows.map((row) => row.tool_name)) };
  }

  async credentialFor(
    workspaceId: string,
    serverId: string,
    userId: string,
  ): Promise<Credential | null> {
    const result = await this.pool.query<{
      id: string;
      auth_kind: "oauth" | "manual";
      ciphertext: Buffer;
    }>(
      `SELECT c.id, c.auth_kind, s.ciphertext
         FROM gateway.credential c
         JOIN gateway.credential_secret s ON s.credential_id = c.id
        WHERE c.workspace_id = $1 AND c.server_id = $2
          AND (c.user_id = $3 OR c.user_id IS NULL)
        ORDER BY (c.user_id IS NULL) ASC
        LIMIT 1`,
      [workspaceId, serverId, userId],
    );
    const row = result.rows[0];
    if (row === undefined) {
      return null;
    }
    const wrapped = await this.wrappedWorkspaceKey(this.pool, workspaceId);
    if (wrapped === null) {
      throw new Error("credential exists without a workspace key");
    }
    const payload = await this.crypto.decryptCredential(row.ciphertext, row.id, workspaceId, wrapped);
    return {
      id: row.id,
      kind: row.auth_kind,
      secret: payload.secret,
      ...(payload.refreshToken === undefined ? {} : { refreshToken: payload.refreshToken }),
      ...(payload.tokenEndpoint === undefined ? {} : { tokenEndpoint: payload.tokenEndpoint }),
      ...(payload.clientId === undefined ? {} : { clientId: payload.clientId }),
    };
  }

  async saveRotatedCredential(
    id: string,
    next: { secret: string; refreshToken?: string },
  ): Promise<void> {
    const ambient = this.refreshClients.get(id);
    if (ambient !== undefined) {
      await this.rotateOn(ambient, id, next);
      return;
    }
    // No surrounding refresh lock: take the row lock for just this write.
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      await client.query("SELECT id FROM gateway.credential WHERE id = $1 FOR UPDATE", [id]);
      await this.rotateOn(client, id, next);
      await client.query("COMMIT");
    } catch (error) {
      await client.query("ROLLBACK");
      throw error;
    } finally {
      client.release();
    }
  }

  private async rotateOn(
    client: PoolClient,
    id: string,
    next: { secret: string; refreshToken?: string },
  ): Promise<void> {
    const result = await client.query<{ workspace_id: string; ciphertext: Buffer }>(
      `SELECT c.workspace_id, s.ciphertext
         FROM gateway.credential c
         JOIN gateway.credential_secret s ON s.credential_id = c.id
        WHERE c.id = $1`,
      [id],
    );
    const row = result.rows[0];
    if (row === undefined) {
      throw new Error("credential no longer exists");
    }
    const wrapped = await this.wrappedWorkspaceKey(client, row.workspace_id);
    if (wrapped === null) {
      throw new Error("credential exists without a workspace key");
    }
    const payload = await this.crypto.decryptCredential(
      row.ciphertext,
      id,
      row.workspace_id,
      wrapped,
    );
    const rotated: CredentialPayload = {
      ...payload,
      secret: next.secret,
      // A refresh response that carries no new refresh token keeps the standing one.
      ...(next.refreshToken === undefined ? {} : { refreshToken: next.refreshToken }),
    };
    const ciphertext = await this.crypto.encryptCredential(rotated, id, row.workspace_id, wrapped);
    await client.query(
      "UPDATE gateway.credential_secret SET ciphertext = $2 WHERE credential_id = $1",
      [id, ciphertext],
    );
    await client.query(
      "UPDATE gateway.credential SET last_refreshed_at = now(), updated_at = now() WHERE id = $1",
      [id],
    );
  }

  async withRefreshLock<T>(credentialId: string, fn: () => Promise<T>): Promise<T> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      // Serializes refreshes per credential across replicas. A concurrently deleted credential
      // locks nothing — the callback's own save then refuses on the missing row.
      await client.query("SELECT id FROM gateway.credential WHERE id = $1 FOR UPDATE", [
        credentialId,
      ]);
      this.refreshClients.set(credentialId, client);
      try {
        const out = await fn();
        await client.query("COMMIT");
        return out;
      } catch (error) {
        await client.query("ROLLBACK");
        throw error;
      }
    } finally {
      this.refreshClients.delete(credentialId);
      client.release();
    }
  }

  async recordObservedTools(
    workspaceId: string,
    serverId: string,
    tools: readonly ObservedTool[],
  ): Promise<void> {
    // Fire-and-forget semantics: a bookkeeping failure must never surface into the proxied call.
    try {
      const names = tools.map((tool) => tool.name);
      const descriptions = tools.map((tool) => tool.description ?? null);
      const client = await this.pool.connect();
      try {
        await client.query("BEGIN");
        await client.query(
          `UPDATE gateway.observed_tool SET currently_offered = false
            WHERE workspace_id = $1 AND server_id = $2 AND NOT (name = ANY($3::text[]))`,
          [workspaceId, serverId, names],
        );
        if (names.length > 0) {
          await client.query(
            `INSERT INTO gateway.observed_tool (workspace_id, server_id, name, description)
             SELECT $1, $2, t.name, t.description
               FROM unnest($3::text[], $4::text[]) AS t(name, description)
             ON CONFLICT (workspace_id, server_id, name) DO UPDATE
                SET description = EXCLUDED.description,
                    last_seen_at = now(),
                    currently_offered = true`,
            [workspaceId, serverId, names, descriptions],
          );
        }
        await client.query("COMMIT");
      } catch (error) {
        await client.query("ROLLBACK");
        throw error;
      } finally {
        client.release();
      }
    } catch {
      this.log("warn", "observed-tool upsert failed", { serverId });
    }
  }

  // ── The internal lane's writes (never part of GatewayStore — the engine has no business here) ──

  /** Replace whatever credential stands in the slot; a re-connect is a fresh row, fresh id. */
  async storeCredential(input: StoreCredentialInput): Promise<string> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const wrapped = await this.ensureWorkspaceKey(client, input.workspaceId);
      await client.query(
        `DELETE FROM gateway.credential
          WHERE workspace_id = $1 AND server_id = $2 AND user_id IS NOT DISTINCT FROM $3`,
        [input.workspaceId, input.serverId, input.userId],
      );
      const id = mintId("cred");
      const ciphertext = await this.crypto.encryptCredential(
        input.payload,
        id,
        input.workspaceId,
        wrapped,
      );
      await client.query(
        `INSERT INTO gateway.credential (id, workspace_id, server_id, user_id, auth_kind, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)`,
        [id, input.workspaceId, input.serverId, input.userId, input.authKind, input.createdByDisplay],
      );
      await client.query(
        "INSERT INTO gateway.credential_secret (credential_id, ciphertext) VALUES ($1, $2)",
        [id, ciphertext],
      );
      await client.query("COMMIT");
      return id;
    } catch (error) {
      await client.query("ROLLBACK");
      throw error;
    } finally {
      client.release();
    }
  }

  /** True when a row was deleted (the secret cascades with it). */
  async deleteCredential(credentialId: string): Promise<boolean> {
    const result = await this.pool.query("DELETE FROM gateway.credential WHERE id = $1", [
      credentialId,
    ]);
    return (result.rowCount ?? 0) > 0;
  }

  async mintOauthFlow(row: Omit<OauthFlowRow, "id">): Promise<string> {
    // Expired ceremony rows die opportunistically here — no sweeper process to run dry.
    await this.pool.query("DELETE FROM gateway.oauth_flow WHERE expires_at < now()");
    const id = mintId("oaf");
    await this.pool.query(
      `INSERT INTO gateway.oauth_flow
         (id, state, workspace_id, server_id, user_id, code_verifier, client_id, token_endpoint,
          resource, return_to, expires_at)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)`,
      [
        id,
        row.state,
        row.workspaceId,
        row.serverId,
        row.userId,
        row.codeVerifier,
        row.clientId,
        row.tokenEndpoint,
        row.resource,
        row.returnTo,
        row.expiresAt,
      ],
    );
    return id;
  }

  /**
   * SINGLE-USE: the row is deleted in the same statement that reads it, and the delete commits
   * before any upstream exchange — a replayed state finds nothing, whatever the exchange did.
   */
  async takeOauthFlow(state: string): Promise<OauthFlowRow | null> {
    const result = await this.pool.query<{
      id: string;
      state: string;
      workspace_id: string;
      server_id: string;
      user_id: string | null;
      code_verifier: string;
      client_id: string;
      token_endpoint: string;
      resource: string;
      return_to: string;
      expires_at: Date;
    }>("DELETE FROM gateway.oauth_flow WHERE state = $1 RETURNING *", [state]);
    const row = result.rows[0];
    if (row === undefined) {
      return null;
    }
    return {
      id: row.id,
      state: row.state,
      workspaceId: row.workspace_id,
      serverId: row.server_id,
      userId: row.user_id,
      codeVerifier: row.code_verifier,
      clientId: row.client_id,
      tokenEndpoint: row.token_endpoint,
      resource: row.resource,
      returnTo: row.return_to,
      expiresAt: row.expires_at,
    };
  }

  private async wrappedWorkspaceKey(on: Queryable, workspaceId: string): Promise<Buffer | null> {
    const result = await on.query<{ wrapped_key: Buffer }>(
      "SELECT wrapped_key FROM gateway.workspace_key WHERE workspace_id = $1",
      [workspaceId],
    );
    return result.rows[0]?.wrapped_key ?? null;
  }

  private async ensureWorkspaceKey(client: PoolClient, workspaceId: string): Promise<Buffer> {
    const standing = await this.wrappedWorkspaceKey(client, workspaceId);
    if (standing !== null) {
      return standing;
    }
    // ON CONFLICT DO NOTHING + re-read: two racing first-credential writes agree on whichever
    // key landed, and neither ever overwrites a key ciphertexts already depend on.
    const minted = await this.crypto.mintWrappedWorkspaceKey(workspaceId);
    await client.query(
      `INSERT INTO gateway.workspace_key (workspace_id, wrapped_key) VALUES ($1, $2)
       ON CONFLICT (workspace_id) DO NOTHING`,
      [workspaceId, minted],
    );
    const landed = await this.wrappedWorkspaceKey(client, workspaceId);
    if (landed === null) {
      throw new Error("workspace key failed to land");
    }
    return landed;
  }
}
