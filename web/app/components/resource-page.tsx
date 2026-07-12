import { Link } from "react-router";
import { buttonClasses, Card } from "@/components/ui";

/**
 * The CONSTANT anonymous teaser a resource address renders in a browser. One page for every
 * address, existing or not — the body carries NO path-derived content (the address bar is the
 * visitor's own knowledge; this document must not confirm it names anything). The sign-in link
 * is the constant `/login` for the same reason: a next-path echo would make the bytes differ
 * per path.
 */
export function ResourcePage() {
  return (
    <main className="mx-auto flex min-h-svh w-full max-w-xl flex-col justify-center gap-6 px-4 py-16">
      <div className="space-y-2">
        <h1 className="font-semibold text-2xl text-ink">A Topos resource address</h1>
        <p className="text-dim text-sm">
          Topos keeps a team&apos;s agent skills — bundles of instructions, scripts, and reference
          docs — current on every machine: publish once, every subscribed agent picks the update up
          at its next session start.
        </p>
      </div>
      <Card className="space-y-3 px-4 py-4">
        <p className="text-ink text-sm">
          <strong>Have an agent?</strong> Paste this page&apos;s URL to it and ask it to follow — it
          runs <code className="font-mono">topos follow &lt;this page&apos;s URL&gt;</code> and
          walks you through the rest. Nothing installs silently: every skill lands only after its
          content digest is disclosed and you say yes.
        </p>
      </Card>
      <Card className="space-y-3 px-4 py-4">
        <p className="text-dim text-sm">
          <strong className="text-ink">Already a member?</strong> Sign in to open this resource in
          your workspace.
        </p>
        <Link to="/login" className={buttonClasses("primary")}>
          Sign in
        </Link>
      </Card>
    </main>
  );
}
