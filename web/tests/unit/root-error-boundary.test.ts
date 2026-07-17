import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  createStaticHandler,
  createStaticRouter,
  data,
  type RouteObject,
  StaticRouterProvider,
} from "react-router";
import { describe, expect, it } from "vitest";
import { ErrorBoundary } from "@/root";

/**
 * The root boundary is the app's uniform miss/fault surface: a thrown 404 (the guards' `notFound()`,
 * which throws `data(null, { status: 404 })`) must render the "Page not found" branch, and anything
 * else the "Something went wrong" fault. The branch is chosen from `useRouteError()` — NOT from an
 * `error` prop: a route module's `ErrorBoundary` reads a BUBBLED error only through that hook (a
 * child route throwing, caught by the ancestor root boundary, passes the error via router context,
 * and the framework's own boundary wrapper reads it the same way). Rendering the boundary through
 * the data router here provides the error ONLY via context, so a boundary that reached for a prop
 * would fall through to the 500 branch on a real 404 — the exact regression this pins.
 */

/** Render the root ErrorBoundary as the ancestor of a child route that throws `thrown`. */
async function renderBubbledError(thrown: unknown): Promise<string> {
  const routes: RouteObject[] = [
    {
      path: "/",
      ErrorBoundary,
      children: [
        {
          path: "boom",
          loader: () => {
            throw thrown;
          },
        },
      ],
    },
  ];
  const handler = createStaticHandler(routes);
  const context = await handler.query(new Request("http://localhost/boom"));
  if (context instanceof Response) {
    throw new Error("expected a rendered context, got a Response");
  }
  const router = createStaticRouter(handler.dataRoutes, context);
  return renderToStaticMarkup(createElement(StaticRouterProvider, { router, context }));
}

describe("root ErrorBoundary", () => {
  it("renders the 404 'Page not found' branch for a thrown 404 route response", async () => {
    // Exactly what `notFound()` throws.
    const html = await renderBubbledError(data(null, { status: 404 }));

    expect(html).toContain("404");
    expect(html).toContain("Page not found");
    expect(html).not.toContain("500");
    expect(html).not.toContain("Something went wrong");
  });

  it("renders the 500 fault branch for a non-route error", async () => {
    const html = await renderBubbledError(new Error("kaboom"));

    expect(html).toContain("500");
    expect(html).toContain("Something went wrong");
    expect(html).not.toContain("Not found");
  });
});
