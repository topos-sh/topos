import { readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

/**
 * THE MIGRATION JOURNAL'S ONE INVARIANT: its timestamps only ever go up.
 *
 * The migrator does not walk the folder in name order. It reads the LAST applied migration's
 * timestamp and applies every journal entry whose own timestamp is greater — so an entry stamped
 * EARLIER than one already applied is silently skipped. Forever, on exactly the databases that
 * have been running longest, and never on a fresh one, where there is no last-applied row and the
 * whole journal runs regardless of order. That is the shape of a bug that passes every test and
 * reaches only production.
 *
 * It happens by accident: a hand-written migration takes a round number, the next generated one
 * takes the real clock, and the clock is behind. This is the guard, and it costs one read.
 */

const HERE = fileURLToPath(new URL(".", import.meta.url));
const DRIZZLE = resolve(HERE, "..", "..", "drizzle");

interface JournalEntry {
  idx: number;
  when: number;
  tag: string;
}

function journal(): JournalEntry[] {
  const text = readFileSync(resolve(DRIZZLE, "meta", "_journal.json"), "utf8");
  return (JSON.parse(text) as { entries: JournalEntry[] }).entries;
}

describe("the web tier's migration journal", () => {
  it("stamps every migration later than the one before it", () => {
    const entries = journal();
    expect(entries.length).toBeGreaterThan(0);
    const outOfOrder = entries
      .map((entry, i) => ({ entry, previous: entries[i - 1] }))
      .filter(({ entry, previous }) => previous !== undefined && entry.when <= previous.when)
      .map(({ entry, previous }) => `${entry.tag} (${entry.when}) <= ${previous?.tag}`);
    expect(outOfOrder).toEqual([]);
  });

  it("has exactly one journal entry per migration file, in the same order", () => {
    const files = readdirSync(DRIZZLE)
      .filter((name) => name.endsWith(".sql"))
      .sort();
    const entries = journal();
    expect(entries.map((e) => `${e.tag}.sql`)).toEqual(files);
    // The index column is the folder order, so a hand-edited journal cannot quietly renumber.
    expect(entries.map((e) => e.idx)).toEqual(entries.map((_, i) => i));
  });
});
