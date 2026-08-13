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
  it("holds a picker's worth of servers", () => {
    // A range, not a number: one more server is an ordinary data edit, while a list that emptied
    // or doubled is a mistake nobody meant to make.
    expect(CURATED_MCP_SERVERS.length).toBeGreaterThanOrEqual(40);
    expect(CURATED_MCP_SERVERS.length).toBeLessThanOrEqual(60);
  });

  /**
   * THE NOTE RULE, at runtime as well as in the types. `manual` is the only word on this list that
   * costs a person work, and the note is the whole reason such a row is allowed here — a row
   * claiming it without saying what the person must do would put an errand on a team with no way
   * to find out what it is. The other direction matters too: a note on a self-service row is copy
   * about work that does not exist.
   */
  it("gives every manual row its note, and no other row one", () => {
    for (const entry of CURATED_MCP_SERVERS) {
      if (entry.auth === "manual") {
        expect(entry.authNote.trim(), `${entry.title} claims manual and says nothing`).not.toBe("");
        expect(entry.authNote).not.toContain("\n");
        // One line a person reads on a picker card, not a paragraph.
        expect(entry.authNote.length).toBeLessThanOrEqual(120);
        continue;
      }
      expect(entry.authNote, `${entry.title} is self-service and carries a note`).toBeUndefined();
    }
  });

  it("names exactly the servers whose sign-in an agent cannot complete", () => {
    // Pinned by slug rather than counted: these four are here because their vendors run no
    // registration an agent could use, and a fifth appearing silently would be the list quietly
    // getting harder to receive.
    const manual = CURATED_MCP_SERVERS.filter((entry) => entry.auth === "manual").map(
      (entry) => entry.slug,
    );
    expect(manual.sort()).toEqual(["asana", "github", "pagerduty", "slack"]);
  });

  it("stays in the alphabetical order the picker scans in", () => {
    // The list ships in file order and the grid draws it in file order, so the file IS the
    // ordering. At fifty rows a new entry dropped in the wrong place is invisible in review and
    // obvious to whoever is looking for a name.
    const titles = CURATED_MCP_SERVERS.map((entry) => entry.title);
    expect(titles).toEqual(
      [...titles].sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase())),
    );
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
      // The sign-in half travels whole: the word the chip reads and, for a manual row, the line
      // the card prints under it. A row that lost the note on the way out would render a caution
      // with nothing to act on.
      expect(row.auth).toBe(entry.auth);
      expect(row.authNote).toBe(entry.authNote);
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
