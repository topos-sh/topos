import { beforeAll, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The canonical-origin redirect (app/lib/canonical.server.ts): a BROWSER on an alias origin is
 * 301'd to the canonical one; every machine face — the card's JSON/markdown negotiation, the
 * `/api` dials — passes untouched, so the CLI keeps working against an alias while a person
 * never lands on a session the auth layer would refuse.
 */

let canonicalOriginRedirect: typeof import("@/lib/canonical.server").canonicalOriginRedirect;

beforeAll(async () => {
  installTestEnv({ TOPOS_PUBLIC_URL: "https://canonical.test" });
  ({ canonicalOriginRedirect } = await import("@/lib/canonical.server"));
});

function req(url: string, accept?: string): Request {
  return new Request(url, { headers: accept === undefined ? {} : { accept } });
}

describe("canonicalOriginRedirect", () => {
  it("301s a browser on an alias host to the same path on the canonical origin", () => {
    const res = canonicalOriginRedirect(req("http://alias.test/some/page?x=1", "text/html"));
    expect(res?.status).toBe(301);
    expect(res?.headers.get("location")).toBe("https://canonical.test/some/page?x=1");
  });

  it("is host-keyed, not origin-keyed (plain-http behind the TLS proxy still matches)", () => {
    // The container sees http on the canonical HOST — that must NOT redirect (a loop otherwise).
    expect(canonicalOriginRedirect(req("http://canonical.test/page", "text/html"))).toBeNull();
  });

  it("never touches a machine face on the alias", () => {
    expect(canonicalOriginRedirect(req("http://alias.test/ws", "application/json"))).toBeNull();
    // A bare fetch (curl's */*) reads the markdown card — untouched too.
    expect(canonicalOriginRedirect(req("http://alias.test/ws", "*/*"))).toBeNull();
    expect(canonicalOriginRedirect(req("http://alias.test/api/v1/x"))).toBeNull();
  });

  it("is path-blind: the redirect is the same 301 for every path (no existence oracle)", () => {
    const a = canonicalOriginRedirect(req("http://alias.test/real-workspace", "text/html"));
    const b = canonicalOriginRedirect(req("http://alias.test/no/such/path", "text/html"));
    expect(a?.status).toBe(b?.status);
  });
});
