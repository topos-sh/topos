import { PassThrough } from "node:stream";
import { createReadableStreamFromReadable } from "@react-router/node";
import * as Sentry from "@sentry/react-router";
import { renderToPipeableStream } from "react-dom/server";
import type { EntryContext, RouterContextProvider } from "react-router";
import { ServerRouter } from "react-router";
import { composition } from "@/composition.server";
import { canonicalOriginRedirect } from "@/lib/canonical.server";
import { cardResponse } from "@/lib/card.server";
import { ensureSetup } from "@/lib/db/identity.server";
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
 * Migrations run EAGERLY, at module load — BEFORE any request is served. In production,
 * react-router-serve imports the server build before it listens, so the container migrates at
 * boot and fails LOUDLY there (the orchestrator restarts it rather than serving unmigrated);
 * in dev, the request handler imports this module before any loader runs. They must NOT wait
 * for handleRequest: React Router runs a document's loaders FIRST and calls handleRequest with
 * their results, so a first-request gate down there always lost the race — a virgin database's
 * first request 500'd (`relation "web.workspace" does not exist`) before the gate ever ran.
 */
await runMigrations();

/**
 * The setup ceremony stays first-request-once: it needs the REQUEST origin for the printed
 * claim link (`TOPOS_PUBLIC_URL` is deliberately un-defaulted — a LAN visitor's own origin must
 * be able to carry the link), and a pre-setup loader read is safe — the schema above exists,
 * and every single-tenant loader treats a workspace-less database as "awaiting its owner". A
 * failure crashes the request loudly (and every subsequent one, since the rejected promise is
 * cached).
 */
let setupPromise: Promise<void> | undefined;
function ensureSetupOnce(request: Request): Promise<void> {
  setupPromise ??= ensureSetup(new URL(request.url).origin, composition.tenancy);
  return setupPromise;
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

  // The CONSTANT protocol card for every non-browser document fetch, served from THIS entry for
  // the same structural reason the canonical redirect lives here: handleRequest sees exactly the
  // document requests — and never the app's own client-side `.data` fetches, whose bare
  // `Accept: */*` is indistinguishable from curl's. A route-level card middleware cannot make
  // that split (it runs for data requests too), and answering a data request with the card
  // poisons every client-side navigation into a carded route: the router cannot decode a
  // markdown card, and the miss surfaces as the root boundary's bogus 500. Served before the
  // migration gate on purpose — the card is constant and needs no database.
  const card = cardResponse(request);
  if (card) {
    return card;
  }

  await ensureSetupOnce(request);

  return new Promise((resolve, reject) => {
    let shellRendered = false;

    const { pipe, abort } = renderToPipeableStream(
      <ServerRouter context={routerContext} url={request.url} />,
      {
        // EVERY response waits for the complete document (onAllReady), never just the shell.
        // This app's SSR is blocking by design — loaders resolve before render, so there is
        // nothing left to stream — and the early-shell path is actively harmful: it ships the
        // router's stream-transfer scripts inside pending Suspense boundaries, which the
        // production React build can leave dehydrated past initial hydration; the first
        // history pop then forces them to hydrate against a client tree that renders nothing
        // there, and the hydration mismatch (React #418) lands in the root ErrorBoundary as a
        // bogus 500. A complete document has no pending boundaries, so none of that machinery
        // ever reaches the client.
        onAllReady() {
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
