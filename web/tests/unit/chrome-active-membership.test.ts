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

  it("is null off-workspace — the multi root and unknown slugs", () => {
    expect(activeMembership(req("/_.data"), seats)).toBeNull();
    expect(activeMembership(req("/"), seats)).toBeNull();
    expect(activeMembership(req("/nowhere.data"), seats)).toBeNull();
    expect(activeMembership(req("/account/devices.data"), seats)).toBeNull();
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
