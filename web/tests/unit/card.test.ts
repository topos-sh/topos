import { beforeAll, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The CONSTANT protocol card + its content negotiation (app/lib/card.server.ts). Two invariants
 * carry the no-existence-oracle promise:
 *  - Accept negotiation mirrors the vault's `wants_json` (any JSON signal → JSON; a browser's
 *    text/html is the caller's to render → null; everything else → the markdown card);
 *  - the card body is BYTE-IDENTICAL for every request path — the ONLY thing that shapes it is the
 *    deployment's follow base, never the URL — so a card fetched at a real address and at a
 *    nonexistent one cannot differ.
 *
 * No Postgres: the card module only reads request headers + the (memoized) server env. beforeAll
 * installs a complete env and clears PLANE_PUBLIC_URL so followBase resolves the request ORIGIN
 * (the door-cutover default), making the api_base_url deterministic across paths.
 */

const ORIGIN = "http://localhost:3000";

let cardFace: typeof import("@/lib/card.server").cardFace;
let cardResponse: typeof import("@/lib/card.server").cardResponse;
let INSTALL_LINE: string;

beforeAll(async () => {
  installTestEnv();
  // Force the origin arm of followBase (unset → new URL(request.url).origin), so the card body is
  // pinned to the request's own origin and identical across every same-origin path.
  process.env.PLANE_PUBLIC_URL = undefined;
  delete process.env.PLANE_PUBLIC_URL;
  const mod = await import("@/lib/card.server");
  cardFace = mod.cardFace;
  cardResponse = mod.cardResponse;
  INSTALL_LINE = mod.INSTALL_LINE;
});

/** A request at `path` carrying `accept` (omit `accept` for no Accept header at all). */
function req(path: string, accept?: string): Request {
  const headers: Record<string, string> = {};
  if (accept !== undefined) {
    headers.accept = accept;
  }
  return new Request(`${ORIGIN}${path}`, { headers });
}

describe("cardFace — the Accept negotiation", () => {
  it("resolves JSON for every JSON signal", () => {
    expect(cardFace(req("/x", "application/json"))).toBe("json");
    // A vendor +json media type still wants JSON.
    expect(cardFace(req("/x", "application/vnd.api+json"))).toBe("json");
    // An `application/*` wildcard wants JSON (the machine arm).
    expect(cardFace(req("/x", "application/*"))).toBe("json");
    // Case-insensitive: cardFace lowercases before matching.
    expect(cardFace(req("/x", "APPLICATION/JSON"))).toBe("json");
  });

  it("resolves HTML only for a browser's text/html", () => {
    expect(cardFace(req("/x", "text/html"))).toBe("html");
    // A real browser's Accept (text/html first, no JSON signal) is a browser → HTML.
    expect(
      cardFace(req("/x", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")),
    ).toBe("html");
  });

  it("resolves markdown for the absent header and for non-JSON non-HTML values", () => {
    // No Accept header at all — a bare fetch.
    expect(cardFace(req("/x"))).toBe("markdown");
    // A curl-ish */* is not a JSON signal and not text/html.
    expect(cardFace(req("/x", "*/*"))).toBe("markdown");
    expect(cardFace(req("/x", "text/plain"))).toBe("markdown");
    // An empty Accept string is markdown, never a throw.
    expect(cardFace(req("/x", ""))).toBe("markdown");
  });

  it("prefers JSON over HTML when both appear (the machine arm wins the multi-value Accept)", () => {
    // JSON is checked first, so a client that lists both reads the machine card.
    expect(cardFace(req("/x", "text/html, application/json"))).toBe("json");
    expect(cardFace(req("/x", "application/json, text/html"))).toBe("json");
  });
});

describe("cardResponse — the served card", () => {
  it("returns null for a browser (the route renders its own HTML)", () => {
    expect(cardResponse(req("/anything", "text/html"))).toBeNull();
  });

  it("serves the JSON card with the no-store/vary/noindex headers", async () => {
    const res = cardResponse(req("/some/workspace", "application/json"));
    expect(res).not.toBeNull();
    const card = res as Response;
    expect(card.status).toBe(200);
    expect(card.headers.get("content-type")).toContain("application/json");
    expect(card.headers.get("cache-control")).toBe("no-store");
    expect(card.headers.get("vary")).toBe("accept");
    expect(card.headers.get("x-robots-tag")).toBe("noindex");
    const body = (await card.json()) as {
      schema_version: number;
      card: string;
      api_base_url: string;
    };
    expect(body.schema_version).toBe(1);
    expect(body.card).toBe("topos-protocol-card");
    // PLANE_PUBLIC_URL is unset → the follow base is the request's own origin.
    expect(body.api_base_url).toBe(ORIGIN);
  });

  it("serves the markdown card as text/plain with the same headers and the teaching copy", async () => {
    const res = cardResponse(req("/some/workspace", "*/*"));
    expect(res).not.toBeNull();
    const card = res as Response;
    expect(card.status).toBe(200);
    expect(card.headers.get("content-type")).toContain("text/plain");
    expect(card.headers.get("cache-control")).toBe("no-store");
    expect(card.headers.get("vary")).toBe("accept");
    expect(card.headers.get("x-robots-tag")).toBe("noindex");
    const text = await card.text();
    expect(text).toContain("# A Topos resource address");
    expect(text).toContain("topos follow");
    // The checksummed installer one-liner is taught verbatim.
    expect(text).toContain(INSTALL_LINE);
  });

  it("JSON card bytes are IDENTICAL across different request paths (no existence oracle)", async () => {
    // Two DIFFERENT URLs, same Accept — the bodies must be byte-for-byte equal.
    const a = cardResponse(req("/real-workspace", "application/json")) as Response;
    const b = cardResponse(req("/deep/nested/nonexistent/path", "application/json")) as Response;
    expect(await a.text()).toBe(await b.text());
  });

  it("markdown card bytes are IDENTICAL across different request paths (no path echo)", async () => {
    const a = cardResponse(req("/real-workspace", "text/plain")) as Response;
    const b = cardResponse(req("/totally/made/up", "text/plain")) as Response;
    const textA = await a.text();
    const textB = await b.text();
    expect(textA).toBe(textB);
    // And the body genuinely carries no path fragment from either request.
    expect(textA).not.toContain("real-workspace");
    expect(textA).not.toContain("made/up");
  });
});
