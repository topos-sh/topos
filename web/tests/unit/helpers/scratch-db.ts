import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import type { MemberActor, OwnerActor, SessionActor, UserActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../../helpers/plane-ddl";
import { installTestEnv } from "./test-env";

/**
 * The ONE scratch-database harness the DB-backed unit suites share (the identity-core pattern,
 * extracted): CREATE DATABASE on the session cluster, install the test env pointing the app pool
 * at it, apply the vault's REAL custody DDL (schema `plane`, from the in-repo migrations) and the
 * app's OWN drizzle migrations (schema `web`, via scripts/migrate.mjs), then hand back a raw-SQL
 * seeder over the app pool. afterAll: end the pool, DROP … WITH (FORCE).
 *
 * Actors are minted here by CAST — the one thing production code must never do (the brand is
 * module-private to guards.server.ts). Every helper mirrors the guards' invariants: ids are THE
 * identity, displays are snapshots.
 */

const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:identity2@localhost:5443/postgres";

export interface ScratchDb {
  /** The scratch database's connection string (the app pool already points at it). */
  url: string;
  /** Raw SQL through the app pool — seeding and out-of-band probes. */
  q<Row extends Record<string, unknown> = Record<string, unknown>>(
    sql: string,
    params?: unknown[],
  ): Promise<Row[]>;
  /** End the app pool and drop the scratch database. */
  drop(): Promise<void>;
}

async function adminQuery(sql: string): Promise<void> {
  const client = new Client({ connectionString: ADMIN_URL });
  await client.connect();
  try {
    await client.query(sql);
  } finally {
    await client.end();
  }
}

export async function createScratchDb(
  prefix: string,
  envOverrides: Record<string, string> = {},
): Promise<ScratchDb> {
  const name = `${prefix}_${Date.now()}_${Math.floor(Math.random() * 10000)}`;
  await adminQuery(`CREATE DATABASE ${name}`);
  const url = new URL(ADMIN_URL);
  url.pathname = `/${name}`;
  const scratchUrl = url.toString();
  installTestEnv({ DATABASE_URL: scratchUrl, ...envOverrides });
  await applyPlaneDdl(scratchUrl);
  const WEB_ROOT = resolve(__dirname, "..", "..", "..");
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: scratchUrl },
    stdio: "pipe",
  });
  return {
    url: scratchUrl,
    async q(sql, params = []) {
      const { getPool } = await import("@/lib/db/index.server");
      const result = await getPool().query(sql, params);
      return result.rows as never;
    },
    async drop() {
      const { getPool } = await import("@/lib/db/index.server");
      await getPool().end();
      await adminQuery(`DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
    },
  };
}

// ── Actor mints (CAST — test-only; production actors come from the guards) ──────────────────

export function asUser(userId: string, display = userId): UserActor {
  return { userId, display } as UserActor;
}

export function asMember(
  workspaceId: string,
  userId: string,
  role: "owner" | "reviewer" | "member" = "member",
  display = userId,
): MemberActor {
  return { userId, display, workspaceId, role } as MemberActor;
}

export function asOwner(workspaceId: string, userId: string, display = userId): OwnerActor {
  return asMember(workspaceId, userId, "owner", display) as OwnerActor;
}

export function asSession(
  workspaceId: string,
  userId: string,
  sessionId: string,
  role: "owner" | "reviewer" | "member" = "member",
  display = userId,
): SessionActor {
  return {
    userId,
    display,
    workspaceId,
    sessionId,
    role,
    sessionStatus: "active",
  } as SessionActor;
}

// ── Row seeds (raw SQL — the scratch database is superuser-owned) ────────────────────────────

export async function seedUser(
  db: ScratchDb,
  id: string,
  name: string,
  email: string,
): Promise<void> {
  await db.q(`INSERT INTO web."user" (id, name, email) VALUES ($1, $2, $3)`, [id, name, email]);
}

export async function seatUser(
  db: ScratchDb,
  ws: string,
  userId: string,
  role: "owner" | "reviewer" | "member",
  invitedBy: string | null = null,
): Promise<void> {
  await db.q(
    `INSERT INTO web.seat (workspace_id, user_id, role, invited_by) VALUES ($1, $2, $3, $4)`,
    [ws, userId, role, invitedBy],
  );
}

/** A session row without the ceremony — the credential hash is derived from the id (unique).
 * Requires the (workspace, user) seat to exist (the composite FK is the anchoring). */
export async function seedSession(
  db: ScratchDb,
  id: string,
  ws: string,
  userId: string,
  status: "active" | "pending" = "active",
  displayName = id,
): Promise<void> {
  await db.q(
    `INSERT INTO web.cli_session (id, workspace_id, user_id, display_name, credential_sha256, status)
     VALUES ($1, $2, $3, $4, sha256(convert_to($1, 'UTF8')), $5)`,
    [id, ws, userId, displayName, status],
  );
}

/** A deterministic 64-hex version id derived from the bundle id (a seed, not a real digest). */
export function versionIdFor(bundleId: string): string {
  return `${bundleId.replaceAll("_", "")}0`.padEnd(64, "a").slice(0, 64);
}

export interface SeedBundleOptions {
  status?: "active" | "archived" | "deleted";
  baseName?: string | null;
  displayName?: string | null;
  protection?: "open" | "reviewed" | null;
  /** Default true: a version + current pointer + digest land plane-side. */
  withPointer?: boolean;
  /** Override the seeded version id (e.g. a REAL 64-hex one where a route validates it). */
  versionId?: string;
}

/** A bundle row + (by default) its plane custody rows: version, current pointer, digest. */
export async function seedBundle(
  db: ScratchDb,
  ws: string,
  id: string,
  name: string,
  opts: SeedBundleOptions = {},
): Promise<{ versionId: string | null }> {
  const status = opts.status ?? "active";
  await db.q(
    `INSERT INTO web.bundle (id, workspace_id, name, display_name, status, protection, base_name, archived_at, deleted_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7,
             CASE WHEN $5 IN ('archived', 'deleted') THEN now() END,
             CASE WHEN $5 = 'deleted' THEN now() END)`,
    [
      id,
      ws,
      name,
      opts.displayName ?? null,
      status,
      opts.protection ?? null,
      opts.baseName ?? null,
    ],
  );
  if (opts.withPointer === false) {
    return { versionId: null };
  }
  const versionId = opts.versionId ?? versionIdFor(id);
  await db.q(
    `INSERT INTO plane.version (workspace_id, bundle_id, version_id, commit_id, author_display)
     VALUES ($1, $2, $3, $3, 'seed')`,
    [ws, id, versionId],
  );
  await db.q(
    `INSERT INTO plane.current_pointer (workspace_id, bundle_id, version_id, moved_by_display)
     VALUES ($1, $2, $3, 'seed')`,
    [ws, id, versionId],
  );
  await db.q(
    `INSERT INTO plane.version_digest (workspace_id, bundle_id, version_id, bundle_digest)
     VALUES ($1, $2, $3, $4)`,
    [ws, id, versionId, "d".repeat(64)],
  );
  return { versionId };
}

export async function seedChannel(
  db: ScratchDb,
  ws: string,
  id: string,
  name: string,
  mode: "open" | "curated" = "open",
): Promise<void> {
  await db.q(`INSERT INTO web.channel (id, workspace_id, name, mode) VALUES ($1, $2, $3, $4)`, [
    id,
    ws,
    name,
    mode,
  ]);
}

/** Place a bundle reference into a channel (by ids). */
export async function placeBundle(
  db: ScratchDb,
  ws: string,
  channelId: string,
  bundleId: string,
): Promise<void> {
  await db.q(
    `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id) VALUES ($1, $2, $3)`,
    [channelId, ws, bundleId],
  );
}

/** Place a bundle into the workspace's DEFAULT channel. */
export async function placeInDefault(db: ScratchDb, ws: string, bundleId: string): Promise<void> {
  await db.q(
    `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id)
     SELECT id, workspace_id, $2 FROM web.channel WHERE is_default AND workspace_id = $1`,
    [ws, bundleId],
  );
}

/** Boot the workspace (ensureSetup) and return its id — the single-tenant anchor. */
export async function bootWorkspace(): Promise<string> {
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup("http://localhost:3000");
  const ws = await identity.theWorkspace();
  if (ws === null) {
    throw new Error("ensureSetup did not mint the workspace");
  }
  return ws.id;
}
