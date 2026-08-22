import type { Pool } from "pg";
import type { UsageEvent, UsageSink } from "../core/ports";
import type { Logger } from "./log";

/**
 * The container's usage sink — a small buffer flushed on an interval and at shutdown, so the
 * response path never waits on an insert. `record` never throws (the port's contract); a failed
 * flush logs counts, never event contents.
 */

const FLUSH_INTERVAL_MS = 2000;
const FLUSH_AT = 200;

export class BufferedUsageSink implements UsageSink {
  private buffer: UsageEvent[] = [];
  private timer: ReturnType<typeof setInterval>;
  private flushing = false;

  constructor(
    private pool: Pool,
    private log: Logger,
  ) {
    this.timer = setInterval(() => {
      void this.flush();
    }, FLUSH_INTERVAL_MS);
    // A pending flush timer must not hold the process open past its listeners.
    this.timer.unref?.();
  }

  record(event: UsageEvent): void {
    try {
      this.buffer.push(event);
      if (this.buffer.length >= FLUSH_AT) {
        void this.flush();
      }
    } catch {
      // Unreachable in practice; the contract says never throw, so nothing may escape.
    }
  }

  async flush(): Promise<void> {
    if (this.flushing || this.buffer.length === 0) {
      return;
    }
    this.flushing = true;
    const batch = this.buffer;
    this.buffer = [];
    try {
      await this.pool.query(
        `INSERT INTO gateway.usage_event
           (workspace_id, server_id, session_id, user_id, tool_name, method, outcome, duration_ms)
         SELECT * FROM unnest(
           $1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::int[]
         )`,
        [
          batch.map((e) => e.workspaceId),
          batch.map((e) => e.serverId),
          batch.map((e) => e.sessionId),
          batch.map((e) => e.userId),
          batch.map((e) => e.toolName),
          batch.map((e) => e.method),
          batch.map((e) => e.outcome),
          batch.map((e) => Math.max(0, Math.round(e.durationMs))),
        ],
      );
    } catch {
      this.log("error", "usage flush failed", { dropped: batch.length });
    } finally {
      this.flushing = false;
    }
  }

  /** Drain what stands and stop the interval — the shutdown path. */
  async close(): Promise<void> {
    clearInterval(this.timer);
    await this.flush();
  }
}

/**
 * Retention for `gateway.usage_event`. One row lands per proxied call and nothing else ever
 * deletes one, so a deployment that runs for a year holds a year of rows; this is the sweep that
 * gives the table a window. It runs on its own interval BESIDE the flush above — never on the
 * request path, and never in the same statement as a write.
 *
 * It also refuses to be a lock event. The delete runs in bounded batches with a ceiling per run,
 * each statement picking its victims by `created_at` (the index migration 0002 adds) and taking
 * only rows no one else has claimed, so several replicas sweeping at once divide the work instead
 * of blocking on each other, and a first sweep over years of accumulated rows is spread across
 * runs rather than held open as one enormous transaction.
 */

/** Hourly. The window is measured in days, so looking oftener buys nothing. */
const RETENTION_INTERVAL_MS = 60 * 60 * 1000;
/** Rows per DELETE — small enough that each statement holds its locks briefly. */
const RETENTION_BATCH = 5_000;
/** The ceiling per run: a backlog drains over several runs instead of in one long transaction. */
const RETENTION_MAX_PER_RUN = 100_000;

export class UsageRetention {
  private timer: ReturnType<typeof setInterval> | null = null;
  private sweeping = false;

  constructor(
    private pool: Pool,
    private log: Logger,
    /** Days to keep; 0 (the default, and what an unset variable means) keeps every row forever. */
    private retentionDays: number,
  ) {}

  /** Arm the interval and take one pass now. A no-op — announced once — when retention is off. */
  start(): void {
    if (this.retentionDays <= 0) {
      this.log("info", "usage retention is off; every usage row is kept", {});
      return;
    }
    this.timer = setInterval(() => {
      void this.sweep();
    }, RETENTION_INTERVAL_MS);
    // A pending sweep must not hold the process open past its listeners.
    this.timer.unref?.();
    // One pass at boot as well: a deployment restarted more often than the interval would
    // otherwise never reach a sweep at all.
    void this.sweep();
  }

  /** Delete what is past the window, in batches, up to this run's ceiling. Never throws. */
  async sweep(): Promise<void> {
    if (this.sweeping || this.retentionDays <= 0) {
      return;
    }
    this.sweeping = true;
    let deleted = 0;
    try {
      while (deleted < RETENTION_MAX_PER_RUN) {
        const batch = Math.min(RETENTION_BATCH, RETENTION_MAX_PER_RUN - deleted);
        const result = await this.pool.query(
          `WITH doomed AS (
             SELECT id FROM gateway.usage_event
              WHERE created_at < now() - make_interval(days => $1::int)
              ORDER BY created_at
              LIMIT $2::int
              FOR UPDATE SKIP LOCKED
           )
           DELETE FROM gateway.usage_event AS u USING doomed WHERE u.id = doomed.id`,
          [this.retentionDays, batch],
        );
        const removed = result.rowCount ?? 0;
        deleted += removed;
        if (removed < batch) {
          break;
        }
      }
      if (deleted === 0) {
        return;
      }
      const oldest = await this.pool.query<{ oldest: Date | null }>(
        "SELECT min(created_at) AS oldest FROM gateway.usage_event",
      );
      // ONE line per sweep that did something — a silent sweep says nothing, so a quiet deployment
      // does not pay an hourly log line for a table that never grows past its window.
      this.log("info", "usage retention swept", {
        deleted,
        retentionDays: this.retentionDays,
        oldestRetained: oldest.rows[0]?.oldest?.toISOString() ?? "none",
        ...(deleted >= RETENTION_MAX_PER_RUN ? { hitRunCeiling: "yes" } : {}),
      });
    } catch {
      // Retention is housekeeping: a failed sweep is worth a line and nothing more. The next run
      // (or the next boot) tries again against the same rows.
      this.log("error", "usage retention sweep failed", { deletedBeforeFailure: deleted });
    } finally {
      this.sweeping = false;
    }
  }

  /** Stop the interval — the shutdown path. An in-flight sweep finishes its current statement. */
  close(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }
}
