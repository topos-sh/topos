import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  createStaticHandler,
  createStaticRouter,
  type RouteObject,
  StaticRouterProvider,
} from "react-router";
import { describe, expect, it } from "vitest";

/**
 * THE SESSIONS LIST'S OWN HEADING, rendered — the one thing about this page a cold reader can
 * read as a lie.
 *
 * The list holds every session that is not waiting for approval, and staleness is a property OF a
 * session in it, not a reason to leave it out: a machine nobody has heard from in a week sits in
 * the list carrying a `stale` chip. A heading that called the list "Active sessions" therefore
 * contradicted the counts line directly above it ("1 active session · 1 stale") over a list of
 * two. The heading names the list; the counts line qualifies it.
 */

const STALE = 1_000;
const WINDOW_MS = 60_000;
const NOW = Date.now();

function session(overrides: Record<string, unknown>) {
  return {
    sessionId: "sn_one",
    displayName: "topos CLI (mac)",
    ownerDisplay: "Olive Owner",
    ownerEmail: "olive@example.com",
    ownerUserId: "u_owner",
    status: "active",
    createdAtMs: NOW - WINDOW_MS,
    lastSeenAtMs: NOW,
    expired: false,
    freshness: "fresh",
    skills: [],
    declinedButApplied: [],
    ...overrides,
  };
}

/** One fresh machine and one the workspace has not heard from — the counts line's "1 · 1" case. */
function pageData() {
  return {
    view: {
      sessions: [
        session({}),
        session({
          sessionId: "sn_two",
          displayName: "topos CLI (linux)",
          lastSeenAtMs: NOW - WINDOW_MS - STALE,
          freshness: "stale",
        }),
      ],
      stalenessWindowMs: WINDOW_MS,
      sessionApproval: "off",
      sessionMaxAgeMs: null,
      wholeWorkspace: true,
    },
    serviceSessions: [],
    isOwner: true,
  };
}

async function renderPage(): Promise<string> {
  const { default: Component } = await import("@/routes/sessions");
  const routes: RouteObject[] = [{ path: "/", loader: () => pageData(), Component }];
  const handler = createStaticHandler(routes);
  const context = await handler.query(new Request("http://localhost/"));
  if (context instanceof Response) {
    throw new Error("expected a rendered context, got a Response");
  }
  const router = createStaticRouter(handler.dataRoutes, context);
  return renderToStaticMarkup(createElement(StaticRouterProvider, { router, context }));
}

describe("the workspace Sessions page", () => {
  it("heads the list `Sessions`, never `Active sessions`", async () => {
    const html = await renderPage();
    expect(html).toContain('<span id="sessions-heading">Sessions</span>');
    expect(html).not.toContain("Active sessions");
  });

  it("leaves the counts line as the qualifier, adding up to the list under it", async () => {
    const html = await renderPage();
    expect(html).toContain("1 active session");
    expect(html).toContain("1 stale");
  });
});
