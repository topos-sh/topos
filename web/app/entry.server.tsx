import { PassThrough } from "node:stream";
import { createReadableStreamFromReadable } from "@react-router/node";
import * as Sentry from "@sentry/react-router";
import { isbot } from "isbot";
import type { RenderToPipeableStreamOptions } from "react-dom/server";
import { renderToPipeableStream } from "react-dom/server";
import type { EntryContext, RouterContextProvider } from "react-router";
import { ServerRouter } from "react-router";
import { canonicalOriginRedirect } from "@/lib/canonical.server";
import { runMigrations } from "@/lib/db/migrate.server";
import { redactTokenPaths } from "@/lib/sentry-scrub";

/**
 * Server-tier error reporting. Sentry is SERVER-ONLY here: there is NO client init on purpose —
 * this app ships zero client env (no `VITE_` values), and widening that boundary for a browser
 * DSN would be an explicit, reviewed act, not a default.
 *
 * The DSN is read straight from process.env: Sentry comes alive at module load, BEFORE the first
 * request runs the lazy env parse / migrations below, so a broken production environment is
 * itself reported rather than killing observability with it. Unset = disabled — dev, test, and
 * CI report nowhere.
 */
Sentry.init({
  dsn: process.env.SENTRY_DSN || undefined,
  // Error monitoring only: tracing spans would carry the vault's token-bearing URLs as span
  // data, and the scrub surface below is deliberately small and auditable.
  tracesSampleRate: 0,
  // Never attach cookies, headers, or IPs: sessions and capability tokens stay out of
  // third-party hands.
  sendDefaultPii: false,
  beforeSend(event) {
    if (event.request) {
      if (event.request.url) {
        event.request.url = redactTokenPaths(event.request.url);
      }
      if (event.request.query_string && typeof event.request.query_string === "string") {
        event.request.query_string = redactTokenPaths(event.request.query_string);
      }
      event.request.cookies = undefined;
      event.request.headers = undefined;
    }
    for (const exception of event.exception?.values ?? []) {
      if (exception.value) {
        exception.value = redactTokenPaths(exception.value);
      }
    }
    if (event.message) {
      event.message = redactTokenPaths(event.message);
    }
    return event;
  },
  beforeBreadcrumb(breadcrumb) {
    if (typeof breadcrumb.data?.url === "string") {
      breadcrumb.data.url = redactTokenPaths(breadcrumb.data.url);
    }
    if (breadcrumb.message) {
      breadcrumb.message = redactTokenPaths(breadcrumb.message);
    }
    return breadcrumb;
  },
});

/** Loader/action/render failures → Sentry (the leaked errors already pass beforeSend's scrub). */
export const handleError = Sentry.createSentryHandleError({ logErrors: true });

/**
 * Migrations at boot, first-request-once. React Router's serve process has NO boot hook (no
 * `instrumentation.register` equivalent), so migrations run on the first request through a
 * module-level promise that every later request reuses — deterministic and idempotent. Awaited
 * at the top of handleRequest so the DB schema is present before any loader reads it. A failure
 * crashes the request loudly (and every subsequent one, since the rejected promise is cached):
 * the orchestrator restarts the process rather than serving against an unmigrated database.
 */
let migrationsPromise: Promise<void> | undefined;
function ensureMigrations(): Promise<void> {
  migrationsPromise ??= runMigrations();
  return migrationsPromise;
}

export const streamTimeout = 5_000;

export default async function handleRequest(
  request: Request,
  responseStatusCode: number,
  responseHeaders: Headers,
  routerContext: EntryContext,
  _loadContext: RouterContextProvider,
): Promise<Response> {
  // A browser on an ALIAS origin goes to the canonical one before anything renders. This lives
  // HERE — the structurally server-only module — rather than as root middleware, because a
  // superset build re-exports this entry whole while a route module's server-only exports are
  // stripped per-module (an OSS root middleware would drag its .server import into the
  // superset's CLIENT graph). handleRequest sees exactly the document requests the redirect is
  // for; every machine face bypasses it untouched.
  const canonical = canonicalOriginRedirect(request);
  if (canonical) {
    return canonical;
  }

  await ensureMigrations();

  return new Promise((resolve, reject) => {
    let shellRendered = false;
    const userAgent = request.headers.get("user-agent");

    // Bots (and SPA mode) wait for the FULL document so crawlers see complete markup; a real
    // browser gets the shell as soon as it is ready and streams the rest.
    const readyOption: keyof RenderToPipeableStreamOptions =
      (userAgent && isbot(userAgent)) || routerContext.isSpaMode ? "onAllReady" : "onShellReady";

    const { pipe, abort } = renderToPipeableStream(
      <ServerRouter context={routerContext} url={request.url} />,
      {
        [readyOption]() {
          shellRendered = true;
          const body = new PassThrough();
          const stream = createReadableStreamFromReadable(body);
          responseHeaders.set("Content-Type", "text/html");
          resolve(new Response(stream, { headers: responseHeaders, status: responseStatusCode }));
          pipe(body);
        },
        onShellError(error: unknown) {
          reject(error);
        },
        onError(error: unknown) {
          responseStatusCode = 500;
          // Log streaming errors only after the shell rendered — a pre-shell error already
          // rejects above.
          if (shellRendered) {
            console.error(error);
          }
        },
      },
    );

    setTimeout(abort, streamTimeout + 1000);
  });
}
