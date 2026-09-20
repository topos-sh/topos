import { data, UNSAFE_ErrorResponseImpl as ErrorResponseImpl } from "react-router";
import { describe, expect, it } from "vitest";
import { isClientRouteError } from "@/lib/router-error-report";

/**
 * The server entry's Sentry filter (app/lib/router-error-report.ts): the router's own 4xx
 * verdicts — a POST with no action, a data request for no route — are noise, not faults, and
 * stay out of the error tracker; everything else still reports.
 */
describe("isClientRouteError", () => {
  it("skips the router's own 405 for a POST to a route without an action", () => {
    // The exact shape React Router mints via getInternalRouterError(405, …).
    const error = new ErrorResponseImpl(
      405,
      "Method Not Allowed",
      new Error(
        'You made a POST request to "/wp-json/batch/v1" but did not provide an `action` for route "catch-all", so there is no way to handle the request.',
      ),
      true,
    );
    expect(isClientRouteError(error)).toBe(true);
  });

  it("skips the router's own 404 for a data request that matches no route", () => {
    const error = new ErrorResponseImpl(404, "Not Found", new Error("No route matches URL"), true);
    expect(isClientRouteError(error)).toBe(true);
  });

  it("still reports a 5xx error response", () => {
    const error = new ErrorResponseImpl(500, "Internal Server Error", new Error("boom"), true);
    expect(isClientRouteError(error)).toBe(false);
  });

  it("still reports a thrown Error and everything that is not a route error response", () => {
    expect(isClientRouteError(new Error("a real fault"))).toBe(false);
    expect(isClientRouteError(new TypeError("x is not a function"))).toBe(false);
    expect(isClientRouteError(new Response("nope", { status: 405 }))).toBe(false);
    expect(isClientRouteError(data({ ok: false }, { status: 400 }))).toBe(false);
    expect(isClientRouteError(undefined)).toBe(false);
    expect(isClientRouteError(null)).toBe(false);
    expect(isClientRouteError("405")).toBe(false);
  });
});
