import { describe, expect, it } from "vitest";
import {
  CURATED_MCP_SERVERS,
  CURATED_VERSION,
  curatedDocumentFor,
  curatedServerByName,
  curatedServerDocument,
  curatedServerRows,
} from "@/lib/mcp/curated.server";
import { canonicalServerJson } from "@/lib/mcp/fetch.server";
import { STREAMABLE_HTTP, validateCandidateFiles } from "@/lib/mcp/validate.server";

/**
 * THE BUILT-IN LIST, HELD TO ITS OWN GATE.
 *
 * The picker's promise is that choosing a row is exactly a publish someone typed by hand — same
 * bytes, same gate, same refusals. This suite is what keeps that true: every entry is turned into
 * the bundle it would actually publish (one `server.json`, the canonical bytes) and driven through
 * `validateCandidateFiles` — the same function the session lane's publish and the import page's
 * second click run. A row that ever picks up a credential, a `{placeholder}`, a plain-http
 * endpoint, or a name the registry grammar refuses fails HERE, in CI, rather than in front of
 * whoever clicked it.
 *
 * The identity checks beside it exist because the registry read lane resolves a bundle by its
 * embedded name: two rows claiming one name would make that lookup a coin flip, and two rows
 * claiming one catalog slug would collide the moment a workspace took both.
 */

/** The bundle one entry would publish — the single file, as bytes, exactly as the page builds it. */
function bundleFor(entry: (typeof CURATED_MCP_SERVERS)[number]) {
  return [
    {
      path: "server.json",
      bytes: new TextEncoder().encode(canonicalServerJson(curatedServerDocument(entry))),
    },
  ];
}

describe("every curated server passes the real publish gate", () => {
  it.each(
    CURATED_MCP_SERVERS.map((entry) => [entry.title, entry] as const),
  )("%s", (_title, entry) => {
    const validated = validateCandidateFiles(bundleFor(entry));
    // The whole refusal, not just a boolean — a failure here should name what is wrong.
    expect(validated.ok ? "ok" : `${validated.code}: ${validated.message}`).toBe("ok");
    if (!validated.ok) {
      return;
    }
    // What the gate read back out is what the picker showed: same identity, same address, same
    // auth word. A row whose chip disagreed with its document would be a lie on the card.
    expect(validated.summary.name).toBe(entry.name);
    expect(validated.summary.description).toBe(entry.description);
    expect(validated.summary.version).toBe(CURATED_VERSION);
    expect(validated.summary.url).toBe(entry.url);
    expect(validated.summary.transport).toBe(STREAMABLE_HTTP);
    expect(validated.summary.authHint).toBe(entry.auth);
    // No literal headers: a curated row is an address and nothing else.
    expect(validated.summary.headers).toEqual([]);
  });
});

describe("the list itself", () => {
  it("holds 20–25 servers", () => {
    expect(CURATED_MCP_SERVERS.length).toBeGreaterThanOrEqual(20);
    expect(CURATED_MCP_SERVERS.length).toBeLessThanOrEqual(25);
  });

  it("claims each registry name once", () => {
    const names = CURATED_MCP_SERVERS.map((entry) => entry.name);
    expect(new Set(names).size).toBe(names.length);
  });

  it("claims each catalog slug once, and each is a publishable name", () => {
    const slugs = CURATED_MCP_SERVERS.map((entry) => entry.slug);
    expect(new Set(slugs).size).toBe(slugs.length);
    for (const slug of slugs) {
      // The same shape the publish form's `pattern` accepts — otherwise the suggested name
      // would arrive pre-rejected by the browser.
      expect(slug).toMatch(/^[a-z0-9][a-z0-9-]*$/);
    }
  });

  it("points every entry at a distinct https endpoint", () => {
    const urls = CURATED_MCP_SERVERS.map((entry) => entry.url);
    expect(new Set(urls).size).toBe(urls.length);
    for (const url of urls) {
      expect(new URL(url).protocol).toBe("https:");
    }
  });

  it("keeps every description to one line the gate accepts", () => {
    for (const entry of CURATED_MCP_SERVERS) {
      expect(entry.description.length).toBeGreaterThan(0);
      expect(entry.description.length).toBeLessThanOrEqual(100);
      expect(entry.description).not.toContain("\n");
    }
  });
});

describe("the lookups the page uses", () => {
  it("resolves a row by its registry name and refuses anything else", () => {
    const first = CURATED_MCP_SERVERS[0];
    expect(first).toBeDefined();
    expect(curatedServerByName(first?.name ?? "")).toBe(first);
    expect(curatedServerByName("io.github.nobody/not-on-the-list")).toBeUndefined();
    expect(curatedServerByName("")).toBeUndefined();
  });

  it("projects a row per entry, carrying the exact bytes a publish would store", () => {
    const rows = curatedServerRows();
    expect(rows).toHaveLength(CURATED_MCP_SERVERS.length);
    for (const [index, entry] of CURATED_MCP_SERVERS.entries()) {
      const row = rows[index];
      expect(row).toBeDefined();
      if (row === undefined) {
        continue;
      }
      expect(row.name).toBe(entry.name);
      expect(row.slug).toBe(entry.slug);
      expect(row.host).toBe(new URL(entry.url).host);
      expect(row.url).toBe(entry.url);
      expect(row.version).toBe(CURATED_VERSION);
      expect(row.transport).toBe(STREAMABLE_HTTP);
      // The dialog shows what would land WITHOUT asking the server for it, so the row has to be
      // the same bytes the publish arm derives — byte for byte, or the page would be showing a
      // document that is not the one it publishes.
      expect(row.document).toBe(canonicalServerJson(curatedServerDocument(entry)));
      expect(row.document).toBe(curatedDocumentFor(row.name));
    }
  });

  it("hands back bytes only for a name this list holds", () => {
    const first = CURATED_MCP_SERVERS[0];
    expect(first).toBeDefined();
    if (first !== undefined) {
      expect(curatedDocumentFor(first.name)).toBe(
        canonicalServerJson(curatedServerDocument(first)),
      );
    }
    // The publish arm's whole defence against a doctored form field: an unknown id is refused,
    // never turned into a document.
    expect(curatedDocumentFor("io.github.nobody/not-on-the-list")).toBeNull();
    expect(curatedDocumentFor("")).toBeNull();
  });
});
