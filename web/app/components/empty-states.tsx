import { Link } from "react-router";
import { buttonClasses } from "@/components/ui";

/**
 * Signed in, member of nothing yet: the one warm onboarding moment. Explains the channel idea in
 * one breath, then offers the single next step — create a workspace — with a quiet aside for
 * someone who arrived on an invite instead.
 */
export function NoWorkspaces() {
  return (
    <div className="mx-auto max-w-xl py-10 sm:py-16">
      <p className="font-display text-[10px] text-faint uppercase tracking-[0.12em]">
        Welcome to Topos
      </p>
      <h1 className="mt-3 font-display font-semibold text-ink text-xl tracking-[-0.02em]">
        Create your first workspace
      </h1>
      <p className="mt-3 text-dim text-sm leading-relaxed">
        A workspace is a channel your team&apos;s agents follow. Publish a skill once and it syncs
        to every agent on the channel — each in its own format. Create one, then paste a single
        command to your agent and you&apos;re live.
      </p>
      <div className="mt-6">
        <Link to="/workspaces/new" className={`${buttonClasses("primary")} min-h-11`}>
          Create a workspace
        </Link>
      </div>
      <div className="mt-8 rounded-lg border border-line-soft bg-panel px-4 py-3">
        <p className="font-medium text-ink text-sm">Have an invite?</p>
        <p className="mt-1 text-dim text-sm leading-relaxed">
          Paste the workspace address from your invite to your agent. Your seat confirms the moment
          a device enrolls, and the channel appears here.
        </p>
      </div>
    </div>
  );
}

/** An empty channel: no skills published yet. Deliberately no CTA — publishing is the agent's move. */
export function NoSkills() {
  return (
    <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
      <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
        No skills published yet
      </h2>
      <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
        Publish from your agent — run{" "}
        <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">topos publish</code>{" "}
        on an enrolled device and the skill appears here on the next load.
      </p>
    </div>
  );
}
