import { describe, expect, it, vi } from "vitest";
import type { WorkspaceMembership } from "@/lib/db/queries.server";
import { destinationPathname } from "@/lib/destination-path";
import { activeMembership } from "@/lib/shell/chrome.server";

/**
 * The chrome loader's active-seat derivation against React Router's request shapes: the
 * document URL (`/acme`), the client-side single-fetch data URL (`/acme.data`), and the
 * trailing-slash spelling (`/acme/_.data`; `/` itself is `/_.data`).
 * REGRESSION: the derivation once split the RAW pathname, so a client-side
 * arrival at a workspace dashboard parsed its first segment as `acme.data`, matched no seat,
 * and the panel rendered only logo + account — after every in-app workspace creation
 * (`/new → /:ws`) and every dropdown switch. Deep destinations (`/acme/settings.data`) never
 * bit, which is why the bug hid behind them.
 *
 * Plus the OFF-WORKSPACE fallback: a multi-tenancy URL miss (a person-scoped page like
 * /account/devices) resolves the seat the sidebar remembered in the `topos_active_ws` cookie,
 * else the first seat — never null while any seat exists — and only ever selects from the
 * proven `memberships` rows, so a stale cookie can't steer `requireMember`.
 */

const tenancy = vi.hoisted(() => ({ mode: "multi" as "single" | "multi" }));

vi.mock("@/composition.server", () => ({
  composition: {
    get tenancy() {
      return tenancy.mode;
    },
  },
}));

function seat(address: string): WorkspaceMembership {
  return {
    id: `ws_${address}`,
    displayName: address.toUpperCase(),
    address,
    role: "member",
    navigable: true,
  };
}

const seats = [seat("acme"), seat("beta")];

function req(path: string): Request {
  return new Request(`http://x${path}`);
}

function reqWithCookie(path: string, cookie: string): Request {
  return new Request(`http://x${path}`, { headers: { cookie } });
}

describe("destinationPathname", () => {
  it("passes a document pathname through untouched", () => {
    expect(destinationPathname(req("/acme"))).toBe("/acme");
    expect(destinationPathname(req("/acme/settings"))).toBe("/acme/settings");
  });

  it("strips the single-fetch .data suffix down to the destination", () => {
    expect(destinationPathname(req("/acme.data"))).toBe("/acme");
    expect(destinationPathname(req("/acme/settings.data"))).toBe("/acme/settings");
    expect(destinationPathname(req("/acme.data?_routes=routes%2Fshell"))).toBe("/acme");
  });

  it("reads the trailing-slash spelling `<path>/_.data` as the framework does", () => {
    // react-router's singleFetchUrl: a trailing-slash destination appends `_.data`; `/` itself
    // becomes `/_.data`. Mirrors getNormalizedPath (lib/server-runtime/urls.ts).
    expect(destinationPathname(req("/_.data"))).toBe("/");
    expect(destinationPathname(req("/acme/_.data"))).toBe("/acme/");
  });
});

describe("activeMembership (multi tenancy)", () => {
  it("resolves the seat from a document navigation", () => {
    expect(activeMembership(req("/acme"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/beta/settings"), seats)?.address).toBe("beta");
  });

  it("resolves the SAME seat from the workspace dashboard's .data URL (the regression)", () => {
    // Old code: first segment "acme.data" → no match → chrome.workspace null → minimal panel.
    expect(activeMembership(req("/acme.data"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/beta.data"), seats)?.address).toBe("beta");
  });

  it("still resolves deep and trailing-slash .data destinations", () => {
    expect(activeMembership(req("/acme/settings.data"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/acme/_.data"), seats)?.address).toBe("acme");
  });

  it("off-workspace paths keep a seat — the fallback, so the panel never blanks", () => {
    expect(activeMembership(req("/_.data"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/nowhere.data"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/account/devices.data"), seats)?.address).toBe("acme");
  });
});

describe("activeMembership (multi tenancy) — the off-workspace fallback", () => {
  it("resolves the seat the sidebar remembered in the topos_active_ws cookie", () => {
    expect(
      activeMembership(reqWithCookie("/account/devices", "topos_active_ws=ws_beta"), seats)
        ?.address,
    ).toBe("beta");
    // Amid other cookies (the collapse state rides the same header), and on a .data URL.
    expect(
      activeMembership(
        reqWithCookie("/account/devices.data", "sidebar_state=false; topos_active_ws=ws_beta"),
        seats,
      )?.address,
    ).toBe("beta");
  });

  it("the URL segment still wins over the cookie — the cookie is a fallback, never an override", () => {
    expect(
      activeMembership(reqWithCookie("/acme", "topos_active_ws=ws_beta"), seats)?.address,
    ).toBe("acme");
    expect(
      activeMembership(reqWithCookie("/acme.data", "topos_active_ws=ws_beta"), seats)?.address,
    ).toBe("acme");
  });

  it("a stale cookie — no matching seat — falls through to the first seat, never past the roster", () => {
    // The seat-proof property: the fallback only SELECTS from `memberships`, so a cookie naming
    // a workspace the person left can never reach loadChrome's requireMember.
    expect(
      activeMembership(reqWithCookie("/account/devices", "topos_active_ws=ws_gone"), seats)
        ?.address,
    ).toBe("acme");
  });

  it("no cookie → the first seat; no seats at all → null, cookie or not", () => {
    expect(activeMembership(req("/account/devices"), seats)?.address).toBe("acme");
    expect(activeMembership(req("/account/devices"), [])).toBeNull();
    expect(
      activeMembership(reqWithCookie("/account/devices", "topos_active_ws=ws_acme"), []),
    ).toBeNull();
  });
});

describe("activeMembership (single tenancy)", () => {
  it("returns the sole seat regardless of the path shape", () => {
    tenancy.mode = "single";
    try {
      expect(activeMembership(req("/_.data"), seats)?.address).toBe("acme");
      expect(activeMembership(req("/members.data"), seats)?.address).toBe("acme");
      expect(activeMembership(req("/"), [])).toBeNull();
    } finally {
      tenancy.mode = "multi";
    }
  });
});
