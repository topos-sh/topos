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
