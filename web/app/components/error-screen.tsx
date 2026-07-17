import { Link } from "react-router";
import { buttonClasses } from "@/components/ui";

/**
 * The app's designed miss/fault surface — one constant page for a 404 and one for a 500, rendered
 * by the root ErrorBoundary. It is DELIBERATELY existence-blind: no path, `error.data`, message, or
 * stack ever reaches it, so a thrown 404 from a resource guard (`notFound()`) is byte-identical to
 * the catch-all's — an anonymous visitor to a skill or channel face that isn't theirs to see gets
 * exactly this page, indistinguishable from a mistyped URL. Klein voice: warm-gray print ground,
 * the `topos_` wordmark, a big Martian Mono status figure closed by the accent cursor, one action.
 */
export function ErrorScreen({ kind }: { kind: "not-found" | "fault" }) {
  const notFound = kind === "not-found";
  const code = notFound ? "404" : "500";
  return (
    <main className="grid min-h-dvh place-items-center bg-ground px-6 py-16">
      <div className="w-full max-w-md">
        <Link
          to="/"
          className="font-display font-semibold text-ink text-sm tracking-[-0.02em] focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          topos<span className="text-accent">_</span>
        </Link>

        <div className="mt-12">
          <p className="font-display text-[10px] text-faint uppercase tracking-[0.12em]">
            Error {code}
          </p>
          <p
            aria-hidden="true"
            className="mt-3 font-display font-semibold text-[clamp(52px,12vw,84px)] text-ink leading-none tracking-[-0.03em]"
          >
            {code}
            <span className="text-accent">_</span>
          </p>
          <h1 className="mt-7 font-display font-semibold text-[clamp(18px,2.2vw,23px)] text-ink leading-[1.45] tracking-[-0.02em]">
            {notFound ? "Page not found" : "Something went wrong"}
          </h1>
          <p className="mt-3 max-w-[46ch] text-dim">
            {notFound
              ? "The page you’re looking for isn’t here. It may have moved, or the link was mistyped."
              : "An unexpected error occurred on our end. Please try again in a moment."}
          </p>

          <div className="mt-8">
            <Link to="/" className={buttonClasses("primary")}>
              Back home
            </Link>
          </div>
        </div>
      </div>
    </main>
  );
}
