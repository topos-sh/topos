/* The three Klein families (see DESIGN.md): Martian Mono for display/headings/labels, IBM Plex
   Sans for running text, IBM Plex Mono for commands, hashes, and button labels. Loaded once here
   as @fontsource faces (self-hosted, no third-party font CDN) at exactly the weights the type
   scale uses; app/app.css points the --font-* custom properties at these family names so the
   @theme font tokens resolve everywhere. */
import "@fontsource/martian-mono/500.css";
import "@fontsource/martian-mono/600.css";
import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import type { ReactNode } from "react";
import type { LinksFunction, MetaFunction } from "react-router";
import {
  isRouteErrorResponse,
  Links,
  Meta,
  Outlet,
  Scripts,
  ScrollRestoration,
  useRouteError,
  useRouteLoaderData,
} from "react-router";
import appStylesHref from "./app.css?url";
import { ErrorScreen } from "./components/error-screen";

export const links: LinksFunction = () => [{ rel: "stylesheet", href: appStylesHref }];

/**
 * The one root-level knob the shell reads: the optional GTM container id. Read straight off
 * `process.env` — DELIBERATELY not through env.server: a composing superset build re-exports
 * this module from its own root route, which puts it in that build's CLIENT module graph, where
 * a static `.server` import cannot be stripped and fails the build. The shape fence matches the
 * env.server schema (which still validates the var loudly at boot); an unset or malformed value
 * hands the shell null and the document ships zero third-party script.
 */
export function loader() {
  const raw = (process.env.TOPOS_GTM_CONTAINER_ID ?? "").trim();
  return { gtmId: /^GTM-[A-Z0-9]+$/.test(raw) ? raw : null };
}

/**
 * The standard Google Tag Manager loader, byte-for-byte from Google's install doc with ONLY the
 * container id parameterized — JSON-stringified into the one string slot, and already
 * shape-checked by env.server, so no env value can break out of the literal.
 */
function gtmSnippet(id: string): string {
  return (
    "(function(w,d,s,l,i){w[l]=w[l]||[];w[l].push({'gtm.start':new Date().getTime()," +
    "event:'gtm.js'});var f=d.getElementsByTagName(s)[0],j=d.createElement(s),dl=l!='dataLayer'" +
    "?'&l='+l:'';j.async=true;j.src='https://www.googletagmanager.com/gtm.js?id='+i+dl;" +
    `f.parentNode.insertBefore(j,f);})(window,document,'script','dataLayer',${JSON.stringify(id)});`
  );
}

/**
 * The root default title + description. Pages set their own full `<title>` as `"<page> · Topos"`
 * (React Router has no `%s` template mechanism — the suffix is a convention every page's `meta`
 * applies), and a page title overrides this default on match.
 */
export const meta: MetaFunction = () => [
  { title: "Topos" },
  { name: "description", content: "Behavior sharing for AI agents in organizations." },
];

/** The HTML shell: React Router wraps BOTH the app and the ErrorBoundary in this Layout. */
export function Layout({ children }: { children: ReactNode }) {
  // Defensive: the Layout also wraps the ErrorBoundary, where the root loader's data may be
  // absent — no data reads as no container id, and the shell degrades to script-free.
  const data = useRouteLoaderData<typeof loader>("root");
  const gtmId = data?.gtmId ?? null;
  return (
    <html lang="en">
      <head>
        <meta charSet="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        {gtmId !== null && (
          <script
            // biome-ignore lint/security/noDangerouslySetInnerHtml: the GTM loader is Google's constant snippet with the env-shape-checked id JSON-stringified in — nothing request- or user-derived.
            dangerouslySetInnerHTML={{ __html: gtmSnippet(gtmId) }}
          />
        )}
        <Meta />
        <Links />
      </head>
      <body className="min-h-dvh font-sans">
        {gtmId !== null && (
          <noscript>
            <iframe
              src={`https://www.googletagmanager.com/ns.html?id=${encodeURIComponent(gtmId)}`}
              height="0"
              width="0"
              style={{ display: "none", visibility: "hidden" }}
              title="Google Tag Manager"
            />
          </noscript>
        )}
        {children}
        <ScrollRestoration />
        <Scripts />
      </body>
    </html>
  );
}

export default function App() {
  return <Outlet />;
}

/**
 * The root boundary — the uniform miss/fault surface. A thrown 404 (the guards' `notFound()`)
 * renders the SAME designed "Page not found" as any missing route; anything else renders the same
 * "Something went wrong" fault. It DELIBERATELY passes no `error.data`, message, or stack to the
 * screen: the app discloses nothing about what exists or why a request failed (the 404-not-403
 * posture carried through to the shell), so a resource face that 404s an anonymous visitor is
 * byte-identical to a mistyped path. React Router sets the HTTP status from the thrown response.
 */
export function ErrorBoundary() {
  const error = useRouteError();
  const notFound = isRouteErrorResponse(error) && error.status === 404;
  return <ErrorScreen kind={notFound ? "not-found" : "fault"} />;
}
