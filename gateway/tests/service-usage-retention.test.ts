/**
 * Usage retention: the window that keeps `gateway.usage_event` from being a table that only grows,
 * and the discipline that keeps the sweep from ever being felt — it deletes in bounded batches, off
 * the request path, and it does nothing at all unless a deployment asked for a window.
 */
import pg from "pg";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { parseGatewayEnv } from "../service/env";
import type { Logger } from "../service/log";
import { UsageRetention } from "../service/usage";
import { createServiceDb, type ServiceDb } from "./helpers/service-db";

let db: ServiceDb;
let pool: pg.Pool;

const logs: { level: string; message: string; fields: Record<string, string | number> }[] = [];
const log: Logger = (level, message, fields) => logs.push({ level, message, fields: fields ?? {} });

beforeAll(async () => {
  db = await createServiceDb();
  pool = new pg.Pool({ connectionString: db.gatewayUrl, max: 5 });
}, 120_000);

afterAll(async () => {
  await pool?.end();
  await db?.drop();
});

beforeEach(async () => {
  logs.length = 0;
  await pool.query("DELETE FROM gateway.usage_event");
});

/** `count` rows all `daysAgo` old. One statement, so a 12,000-row case is still a fast test. */
async function seedRows(count: number, daysAgo: number): Promise<void> {
  await pool.query(
    `INSERT INTO gateway.usage_event
       (workspace_id, server_id, session_id, user_id, tool_name, method, outcome, duration_ms, created_at)
     SELECT 'ws1', 'srv1', 'sn1', 'u1', NULL, 'tools/list', 'ok', 3,
            now() - make_interval(days => $2::int)
       FROM generate_series(1, $1::int)`,
    [count, daysAgo],
  );
}

async function rowCount(): Promise<number> {
  const { rows } = await pool.query<{ n: string }>("SELECT count(*) AS n FROM gateway.usage_event");
  return Number(rows[0]?.n ?? 0);
}

describe("the retention window", () => {
  it("deletes what is past the window and nothing that is inside it", async () => {
    await seedRows(5, 200); // Long past a 90-day window.
    await seedRows(3, 91);
    await seedRows(7, 89); // Inside it, by a day.
    await seedRows(4, 0);

    await new UsageRetention(pool, log, 90).sweep();

    expect(await rowCount()).toBe(11);
    const { rows } = await pool.query<{ oldest: Date }>(
      "SELECT min(created_at) AS oldest FROM gateway.usage_event",
    );
    const ageDays = (Date.now() - (rows[0]?.oldest.getTime() ?? 0)) / 86_400_000;
    expect(ageDays).toBeGreaterThan(88);
    expect(ageDays).toBeLessThan(90);
  });

  it("logs one summary line naming the count and the oldest row it kept", async () => {
    await seedRows(6, 200);
    await seedRows(2, 10);

    await new UsageRetention(pool, log, 90).sweep();

    const summaries = logs.filter((entry) => entry.message === "usage retention swept");
    expect(summaries).toHaveLength(1);
    expect(summaries[0]?.level).toBe("info");
    expect(summaries[0]?.fields["deleted"]).toBe(6);
    expect(summaries[0]?.fields["retentionDays"]).toBe(90);
    expect(String(summaries[0]?.fields["oldestRetained"])).toMatch(/^\d{4}-\d{2}-\d{2}T/);
  });

  it("says nothing at all on a sweep with nothing to delete", async () => {
    await seedRows(4, 3);
    await new UsageRetention(pool, log, 90).sweep();
    expect(await rowCount()).toBe(4);
    expect(logs).toEqual([]);
  });

  it("clears a backlog larger than one batch, in more than one statement", async () => {
    // Past the 5,000-row batch: a sweep that stopped at one statement would leave 7,000 behind.
    await seedRows(12_000, 120);
    await seedRows(10, 1);

    await new UsageRetention(pool, log, 90).sweep();

    expect(await rowCount()).toBe(10);
  }, 60_000);
});

describe("keeping everything, which is the default", () => {
  it("deletes nothing when the window is 0, however old the rows are", async () => {
    await seedRows(5, 4_000); // Eleven years old.

    const retention = new UsageRetention(pool, log, 0);
    retention.start();
    await retention.sweep();
    retention.close();

    expect(await rowCount()).toBe(5);
    // The one line it does emit says the deployment is keeping everything — silence would leave an
    // operator unable to tell "retention is off" from "retention ran and found nothing".
    expect(logs.map((entry) => entry.message)).toEqual([
      "usage retention is off; every usage row is kept",
    ]);
  });

  it("is what an unset GATEWAY_USAGE_RETENTION_DAYS means", () => {
    const base = {
      DATABASE_URL: "postgres://topos_gateway@localhost/topos",
      GATEWAY_PUBLIC_URL: "https://gateway.example.com",
      TOPOS_PUBLIC_URL: "https://topos.example.com",
      GATEWAY_MASTER_KEY_FILE: "/k",
    };
    expect(parseGatewayEnv({ ...base }).GATEWAY_USAGE_RETENTION_DAYS).toBe(0);
    expect(parseGatewayEnv({ ...base, GATEWAY_USAGE_RETENTION_DAYS: "90" }).GATEWAY_USAGE_RETENTION_DAYS).toBe(90);
    // A typo is a refusal to boot, not a silently ignored window.
    expect(() => parseGatewayEnv({ ...base, GATEWAY_USAGE_RETENTION_DAYS: "ninety" })).toThrow();
    expect(() => parseGatewayEnv({ ...base, GATEWAY_USAGE_RETENTION_DAYS: "-1" })).toThrow();
  });
});

describe("the index the sweep needs", () => {
  it("is in the lineage, on created_at alone", async () => {
    const { rows } = await pool.query<{ indexdef: string }>(
      "SELECT indexdef FROM pg_indexes WHERE schemaname = 'gateway' AND indexname = 'usage_event_created_idx'",
    );
    expect(rows[0]?.indexdef).toMatch(/\(created_at\)/);
  });
});
