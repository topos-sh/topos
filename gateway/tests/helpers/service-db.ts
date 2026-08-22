import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import pg from "pg";
import { runMigrations } from "../../service/migrate";

/**
 * The service suites' scratch-database rig — the web scratch-db harness re-shaped for a
 * two-application database: one fresh database per rig on the session cluster, BOTH app roles
 * created idempotently, the PRODUCTION grant design applied (default privileges before either
 * lineage runs, so tables arrive granted), then the WEB drizzle lineage as `topos_web` and the
 * gateway lineage as `topos_gateway` — each logging in as its own role, never SET ROLE, because
 * SET ROLE adopts neither the search_path nor the login-time privileges the probes must prove.
 */

const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgres://postgres:postgres@127.0.0.1:5461/postgres";

const GATEWAY_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const WEB_ROOT = resolve(GATEWAY_ROOT, "..", "web");

export const MIGRATIONS_DIR = join(GATEWAY_ROOT, "migrations");

const quietLog = () => {};

export interface ServiceDb {
  /** Superuser on the scratch database — seeding and out-of-band witnesses. */
  q<Row extends Record<string, unknown> = Record<string, unknown>>(
    sql: string,
    params?: unknown[],
  ): Promise<Row[]>;
  adminUrl: string;
  /** Connection string for role `topos_gateway` — what the service itself uses. */
  gatewayUrl: string;
  /** Connection string for role `topos_web` — the grants probes log in with this. */
  webUrl: string;
  drop(): Promise<void>;
}

async function withClient<T>(url: string, fn: (client: pg.Client) => Promise<T>): Promise<T> {
  const client = new pg.Client({ connectionString: url });
  await client.connect();
  try {
    return await fn(client);
  } finally {
    await client.end();
  }
}

function urlForRole(base: string, role: string, password: string, database: string): string {
  const url = new URL(base);
  url.username = role;
  url.password = password;
  url.pathname = `/${database}`;
  return url.toString();
}

export async function createServiceDb(): Promise<ServiceDb> {
  const name = `gw_${Date.now()}_${Math.floor(Math.random() * 10000)}`;
  await withClient(ADMIN_URL, async (admin) => {
    for (const [role, password] of [
      ["topos_web", "web"],
      ["topos_gateway", "gateway"],
    ] as const) {
      const { rows } = await admin.query("SELECT 1 FROM pg_roles WHERE rolname = $1", [role]);
      if (rows.length === 0) {
        // Parallel suite files race this create; the loser's duplicate is the same outcome.
        await admin.query(`CREATE ROLE ${role} LOGIN PASSWORD '${password}'`).catch((error) => {
          if ((error as { code?: string }).code !== "42710") {
            throw error;
          }
        });
      }
    }
    await admin.query(`CREATE DATABASE ${name}`);
  });

  const scratchAdminUrl = new URL(ADMIN_URL);
  scratchAdminUrl.pathname = `/${name}`;
  const adminUrl = scratchAdminUrl.toString();

  await withClient(adminUrl, async (admin) => {
    // The production provisioning recipe, in miniature: schemas born with AUTHORIZATION,
    // topos_web alone may CREATE (drizzle's unconditional CREATE SCHEMA IF NOT EXISTS), role
    // search_paths pinned per database, and the cross-lane read grants declared BEFORE either
    // lineage runs so every table arrives already granted.
    await admin.query(`REVOKE ALL ON DATABASE ${name} FROM PUBLIC`);
    await admin.query(`GRANT CONNECT ON DATABASE ${name} TO topos_web`);
    await admin.query(`GRANT CONNECT ON DATABASE ${name} TO topos_gateway`);
    // BOTH migrators run CREATE SCHEMA IF NOT EXISTS, and Postgres checks CREATE-on-database
    // before honoring the IF-NOT-EXISTS short-circuit — the same gotcha the web migrator
    // documents. The initdb design needs the same pair of grants.
    await admin.query(`GRANT CREATE ON DATABASE ${name} TO topos_web`);
    await admin.query(`GRANT CREATE ON DATABASE ${name} TO topos_gateway`);
    await admin.query("CREATE SCHEMA IF NOT EXISTS web AUTHORIZATION topos_web");
    await admin.query("CREATE SCHEMA IF NOT EXISTS gateway AUTHORIZATION topos_gateway");
    await admin.query(`ALTER ROLE topos_web IN DATABASE ${name} SET search_path = web, gateway`);
    await admin.query(
      `ALTER ROLE topos_gateway IN DATABASE ${name} SET search_path = gateway, web`,
    );
    // No blanket web grant here — the web lineage's 0028 grants topos_gateway exactly the tables
    // it reads (topos_gateway exists by now, so its guard fires), which is what production does.
    // Granting all of web here would let this rig read the identity store and drift from prod.
  });

  const webUrl = urlForRole(ADMIN_URL, "topos_web", "web", name);
  const gatewayUrl = urlForRole(ADMIN_URL, "topos_gateway", "gateway", name);

  // The REAL web lineage, as topos_web — the same runner the web app's own boot uses.
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: webUrl },
    stdio: "pipe",
  });
  // The gateway lineage, as topos_gateway (its migrations GRANT web's reads themselves).
  await runMigrations(gatewayUrl, MIGRATIONS_DIR, quietLog);

  const adminPool = new pg.Pool({ connectionString: adminUrl, max: 3 });
  return {
    adminUrl,
    gatewayUrl,
    webUrl,
    async q(sql, params = []) {
      const result = await adminPool.query(sql, params);
      return result.rows as never;
    },
    async drop() {
      await adminPool.end();
      await withClient(ADMIN_URL, async (admin) => {
        await admin.query(`DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
      });
    },
  };
}

// ── Row seeds (superuser-side; ids are the identity, displays are snapshots) ─────────────────

export async function seedIdentity(
  db: ServiceDb,
  ws: string,
  userId: string,
): Promise<void> {
  await db.q(
    `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, $1, $1, now())
     ON CONFLICT (id) DO NOTHING`,
    [ws],
  );
  await db.q(
    `INSERT INTO web."user" (id, name, email) VALUES ($1, $1, $1 || '@example.test')
     ON CONFLICT (id) DO NOTHING`,
    [userId],
  );
  await db.q(
    `INSERT INTO web.seat (workspace_id, user_id, role) VALUES ($1, $2, 'member')
     ON CONFLICT DO NOTHING`,
    [ws, userId],
  );
}

export function bearerSha256Hex(bearer: string): string {
  return createHash("sha256").update(bearer).digest("hex");
}

export async function seedSession(
  db: ServiceDb,
  id: string,
  ws: string,
  userId: string,
  bearer: string,
  status: "active" | "pending" = "active",
): Promise<void> {
  await db.q(
    `INSERT INTO web.cli_session (id, workspace_id, user_id, display_name, credential_sha256, status)
     VALUES ($1, $2, $3, $1, decode($4, 'hex'), $5)`,
    [id, ws, userId, bearerSha256Hex(bearer), status],
  );
}

export interface SeedServerOptions {
  url?: string | null;
  authMode?: "none" | "oauth" | "manual" | null;
  document?: Record<string, unknown>;
  /** A second revision the connection pins (the current stays on the first). */
  pinned?: boolean;
  bundleStatus?: "active" | "archived";
}

export interface SeededServer {
  serverId: string;
  bundleId: string;
  bundleName: string;
  currentRevisionId: string;
  pinnedRevisionId: string | null;
}

/** A connected server: bundle (kind mcp) + catalog row + revision(s) + the connection row. */
export async function seedConnectedServer(
  db: ServiceDb,
  ws: string,
  tag: string,
  opts: SeedServerOptions = {},
): Promise<SeededServer> {
  const serverId = `msrv_${tag}`;
  const bundleId = `b_${tag}`;
  const bundleName = `srv-${tag}`;
  const url = opts.url === undefined ? `https://mcp.example.com/${tag}` : opts.url;
  const document = opts.document ?? {
    name: `io.test/${tag}`,
    description: "test server",
    ...(url === null ? {} : { remotes: [{ type: "streamable-http", url }] }),
  };
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, display_name, auth_mode) VALUES ($1, NULL, $2, $3)`,
    [serverId, `Server ${tag}`, opts.authMode ?? null],
  );
  const mintRevision = async (seq: number, revisionUrl: string | null): Promise<string> => {
    const revisionId = `mcpr_${randomBytes(16).toString("hex")}`;
    await db.q(
      `INSERT INTO web.mcp_server_revision (id, server_id, seq, document, transport, url)
       VALUES ($1, $2, $3, $4, $5, $6)`,
      [
        revisionId,
        serverId,
        seq,
        JSON.stringify(document),
        revisionUrl === null ? null : "streamable-http",
        revisionUrl,
      ],
    );
    return revisionId;
  };
  const currentRevisionId = await mintRevision(1, url);
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [
    serverId,
    currentRevisionId,
  ]);
  let pinnedRevisionId: string | null = null;
  if (opts.pinned === true) {
    // The pin's address diverges so a test can SEE which revision resolution answered.
    pinnedRevisionId = await mintRevision(2, url === null ? null : `${url}/pinned`);
  }
  await db.q(
    `INSERT INTO web.bundle (id, workspace_id, kind, name, status, archived_at)
     VALUES ($1, $2, 'mcp', $3, $4, CASE WHEN $4 = 'archived' THEN now() END)`,
    [bundleId, ws, bundleName, opts.bundleStatus ?? "active"],
  );
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id, pinned_revision_id)
     VALUES ($1, $2, $3, $4)`,
    [bundleId, ws, serverId, pinnedRevisionId],
  );
  return { serverId, bundleId, bundleName, currentRevisionId, pinnedRevisionId };
}

export async function setToolPolicy(
  db: ServiceDb,
  ws: string,
  serverId: string,
  mode: "all" | "selected",
  selected: string[] = [],
): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_tool_policy (workspace_id, server_id, mode) VALUES ($1, $2, $3)
     ON CONFLICT (workspace_id, server_id) DO UPDATE SET mode = EXCLUDED.mode`,
    [ws, serverId, mode],
  );
  for (const tool of selected) {
    await db.q(
      `INSERT INTO web.mcp_tool_selection (workspace_id, server_id, tool_name) VALUES ($1, $2, $3)
       ON CONFLICT DO NOTHING`,
      [ws, serverId, tool],
    );
  }
}
